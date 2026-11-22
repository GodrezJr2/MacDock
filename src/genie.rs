//! Genie minimize/restore (port of Genie.cs). Keeps recent snapshots of app windows and, when one minimizes
//! or comes back from the dock, bends the snapshot into the dock icon: a 128-row strip mesh rasterized on the
//! CPU (bilinear, all cores) into a click-through layered overlay. Only the mesh's bounding box is drawn and
//! pushed each frame (UpdateLayeredWindow with a source point), so the cost follows what is on screen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiny_skia::{IntSize, Pixmap};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetCompositionTimingInfo, DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS, DWM_TIMING_INFO};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::genie_gpu::Gpu;
use crate::gfx::Surface;
use crate::{push, shell, Ev};

// Rows of the strip mesh. The GPU draws each strip as two affine triangles, so few rows show as kinks in the
// curved sides and a diagonal shear in the content; 128 keeps both under a pixel and still costs nothing.
pub(crate) const ROWS: usize = 128;
const MAX_SNAPS: usize = 8; // windows still on screen
const MAX_MINIMIZED: usize = 6; // in memory (and SNAP_BYTES_MINIMIZED); older ones are read back from disk
const TIMER_CAPTURE: usize = 1;
const TIMER_PENDING: usize = 2;
const TIMER_DISARM: usize = 3;
const WM_CAPTURED: u32 = WM_APP + 5;
const WM_HOOK_DOWN: u32 = WM_APP + 6;
const WM_HOOK_UP: u32 = WM_APP + 7;
const HTMINBUTTON: isize = 8;

pub enum GenieEv {
    Timer(usize),
    Captured,
    Down(i32, i32),
    Up,
    Display,
}

// ------------------------------------------------------------------ window actions

pub fn minimize_all(wins: &[HWND]) {
    for &w in wins {
        shell::minimize(w);
    }
}

pub fn restore_all(front: HWND, minimized: &[HWND]) {
    for &w in minimized.iter().rev() {
        shell::restore(w);
    }
    shell::activate(front);
}

/// What happens when the overlay is up (`Then` on start) or the motion is over (on done).
#[derive(Clone)]
pub enum Then {
    Nothing,
    Minimize(Vec<HWND>),
    Restore(HWND, Vec<HWND>),
}

impl Then {
    fn run(self) {
        match self {
            Then::Nothing => {}
            Then::Minimize(w) => minimize_all(&w),
            Then::Restore(f, m) => restore_async(f, m),
        }
    }
}

/// restore_all on a thread of its own. ShowWindow on another process's window waits until that app has
/// handled the restore, which for a busy app (a browser, an editor) is a good part of a second; on the frame
/// loop that froze the overlay on its last frame and the window then popped in.
pub fn restore_async(front: HWND, minimized: Vec<HWND>) {
    let (f, m): (isize, Vec<isize>) = (front.0 as isize, minimized.iter().map(|h| h.0 as isize).collect());
    let spawned = std::thread::Builder::new().name("genie-restore".into()).spawn(move || {
        let m: Vec<HWND> = m.into_iter().map(hwnd_of).collect();
        restore_all(hwnd_of(f), &m);
    });
    if spawned.is_err() {
        restore_all(front, &minimized);
    }
}

/// After a restore: the snapshot stays up until the real window is out of the dock and has had a moment to
/// paint, then dissolves into it instead of vanishing, which hides whatever changed since the snapshot.
const RESTORE_WAIT_MAX: f64 = 0.6; // give up waiting for the window after this long
const RESTORE_PAINT: f64 = 0.05; // frames for the window to paint once it is shown
const RESTORE_FADE: f64 = 0.12;

// ------------------------------------------------------------------ Windows' own minimize animation

/// Puts Windows' minimize animation back to the user's saved setting (WindowMetrics\MinAnimate).
pub fn restore_system_animation() {
    set_min_animate(read_min_animate().unwrap_or(true));
}

fn read_min_animate() -> Option<bool> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut buf = [0u16; 8];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, w!("Control Panel\\Desktop\\WindowMetrics"), w!("MinAnimate"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len))
    };
    if r.is_err() {
        return None;
    }
    Some(buf[0] != '0' as u16)
}

/// Not written to the profile (no SPIF_UPDATEINIFILE), so a reboot undoes it.
pub fn set_min_animate(on: bool) {
    let mut ai = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: on as i32 };
    unsafe {
        let _ = SystemParametersInfoW(SPI_SETANIMATION, ai.cbSize, Some(&mut ai as *mut _ as *mut _), SPIF_SENDCHANGE);
    }
}

// ------------------------------------------------------------------ geometry

#[derive(Clone, Copy, Debug)]
pub struct R {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl R {
    fn right(&self) -> f64 {
        self.x + self.w
    }
    fn bottom(&self) -> f64 {
        self.y + self.h
    }
    fn union(&self, o: &R) -> R {
        let (l, t) = (self.x.min(o.x), self.y.min(o.y));
        R { x: l, y: t, w: self.right().max(o.right()) - l, h: self.bottom().max(o.bottom()) - t }
    }
}

fn smooth(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Quintic ease: like `smooth` but its acceleration also starts and ends at zero, so the slide eases into the
/// icon without the small jolt smoothstep has where it lands.
fn smoother(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
}

/// The display refresh a frame presented now will show at, and the refresh period, both in seconds on the QPC
/// clock. Animation time taken from it advances in exact refresh steps, so the motion stays even however late
/// in the loop the frame is drawn (Instant::now() there jitters by a millisecond or two, which at genie speed
/// is several pixels of stutter). None when DWM gives no timing.
fn next_vblank() -> Option<(f64, f64)> {
    unsafe {
        let mut ti = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
        DwmGetCompositionTimingInfo(HWND::default(), &mut ti).ok()?;
        let (mut now, mut freq) = (0i64, 0i64);
        QueryPerformanceCounter(&mut now).ok()?;
        QueryPerformanceFrequency(&mut freq).ok()?;
        let (vb, per) = (ti.qpcVBlank as f64, ti.qpcRefreshPeriod as f64);
        if per <= 0.0 || freq <= 0 || vb <= 0.0 {
            return None;
        }
        // floor, not round: a stale qpcVBlank (DWM not updated it yet) then still lands on the right refresh
        let k = ((now as f64 - vb) / per).floor().max(0.0);
        let f = freq as f64;
        Some(((vb + (k + 1.0) * per) / f, per / f))
    }
}

/// Genie: first the sides bend into a funnel towards the icon (squeeze), then every row slides down and piles
/// up at the icon's bottom edge (slide). p = 0 is the window, p = 1 is fully inside the icon.
/// Returns (left, right, y) per row edge, screen pixels.
fn shape(win: R, icon: R, p: f64) -> Vec<(f64, f64, f64)> {
    let squeeze = smooth(p / 0.45);
    let slide = smoother((p - 0.22) / 0.78);
    let travel = icon.bottom() - win.y;
    let funnel = (icon.y - win.y).max(1.0);
    (0..=ROWS)
        .map(|i| {
            let v = i as f64 / ROWS as f64;
            let y = (win.y + win.h * v + travel * slide).min(icon.bottom());
            let f = smooth((y - win.y) / funnel);
            let l = win.x + (icon.x - win.x) * f * squeeze;
            let r = win.right() + (icon.right() - win.right()) * f * squeeze;
            (l, r, y)
        })
        .collect()
}

/// Rasterize the strip mesh into `dst` (premultiplied BGRA, `stride` px per row), whose top-left pixel sits at
/// screen (`ox`, `oy`). Scanline by scanline: find the strip, interpolate its edges, sample bilinearly in
/// fixed point (two channels per u32 lane pair). Lines go out in small blocks that threads pull from a shared
/// counter, so fast and slow cores (P/E, or parked cores on battery) finish together.
fn raster(dst: &mut [u8], stride: usize, ox: f64, oy: f64, w: usize, h: usize, rows: &[(f64, f64, f64)], tex: &Pixmap) {
    const BLOCK: usize = 24;
    let blocks = h.div_ceil(BLOCK);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16).min(blocks).max(1);
    let (tw, th) = (tex.width() as usize, tex.height() as usize);
    let td: &[u32] = bytemuck_u32(tex.data());
    let base = dst.as_mut_ptr() as usize;
    let len = dst.len();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || loop {
        let b = next.fetch_add(1, Ordering::Relaxed);
        if b >= blocks {
            return;
        }
        let mut seg = 0usize;
        for ly in b * BLOCK..((b + 1) * BLOCK).min(h) {
            let off = ly * stride * 4;
            if off + w * 4 > len {
                return;
            }
            // SAFETY: every line belongs to exactly one block, and blocks are handed out once
            let line = unsafe { std::slice::from_raw_parts_mut((base + off) as *mut u32, w) };
            line.fill(0);
            let yc = oy + ly as f64 + 0.5;
            let (top, bot) = (rows[0].2, rows[ROWS].2);
            let vc = ((yc - top + 0.5).clamp(0.0, 1.0)) * ((bot - yc + 0.5).clamp(0.0, 1.0));
            if vc <= 0.0 || bot - top < 1e-3 {
                continue; // also: fully collapsed into the icon's edge, nothing left to draw
            }
            let yy = yc.clamp(top, bot - 1e-6);
            while seg + 1 < ROWS && rows[seg + 1].2 <= yy {
                seg += 1;
            }
            while seg > 0 && rows[seg].2 > yy {
                seg -= 1;
            }
            let (a, b2) = (rows[seg], rows[seg + 1]);
            let span = b2.2 - a.2;
            if span <= 1e-6 {
                continue;
            }
            let t = ((yy - a.2) / span).clamp(0.0, 1.0);
            let l = a.0 + (b2.0 - a.0) * t;
            let r = a.1 + (b2.1 - a.1) * t;
            if r - l < 0.01 {
                continue;
            }
            let v = (seg as f64 + t) / ROWS as f64;
            let sy = (v * th as f64 - 0.5).clamp(0.0, (th - 1) as f64);
            let y0t = sy as usize;
            let fy = (sy.fract() * 256.0) as u32;
            let y1t = (y0t + 1).min(th - 1);
            let (row0, row1) = (&td[y0t * tw..y0t * tw + tw], &td[y1t * tw..y1t * tw + tw]);
            let x_from = ((l - ox - 1.0).floor().max(0.0)) as usize;
            let x_to = ((r - ox + 1.0).ceil().max(0.0) as usize).min(w);
            // texture x as a linear function of the output x, 16.16 fixed point
            let ds = tw as f64 / (r - l);
            let s0 = ((ox + x_from as f64 + 0.5 - l) * ds - 0.5) * 65536.0;
            let (s0, ds) = (s0 as i64, (ds * 65536.0) as i64);
            let smax = ((tw - 1) as i64) << 16;
            let vc8 = (vc * 256.0 + 0.5) as u32;
            // interior pixels are fully covered horizontally; only the edge pixels need the slow path
            let in_from = (l - ox + 0.5).ceil().max(0.0) as usize;
            let in_to = ((r - ox - 0.5).floor().max(0.0) as usize).min(w);
            for x in x_from..x_to {
                let cov8 = if x >= in_from && x < in_to {
                    vc8
                } else {
                    let xc = ox + x as f64 + 0.5;
                    let c = (xc - l + 0.5).clamp(0.0, 1.0) * (r - xc + 0.5).clamp(0.0, 1.0) * vc;
                    (c * 256.0 + 0.5) as u32
                };
                if cov8 == 0 {
                    continue;
                }
                let sx = (s0 + (x - x_from) as i64 * ds).clamp(0, smax);
                let x0t = (sx >> 16) as usize;
                let fx = ((sx >> 8) & 0xFF) as u32;
                let x1t = (x0t + 1).min(tw - 1);
                let top = lerp2(row0[x0t], row0[x1t], fx);
                let btm = lerp2(row1[x0t], row1[x1t], fx);
                let mut c = lerp2(top, btm, fy) & 0x00FF_FFFF;
                if cov8 < 256 {
                    let rb = ((c & 0x00FF_00FF) * cov8 >> 8) & 0x00FF_00FF;
                    let g = ((c & 0x0000_FF00) * cov8 >> 8) & 0x0000_FF00;
                    c = rb | g;
                }
                line[x] = c | (cov8.min(255) << 24);
            }
        }
    };
    std::thread::scope(|s| {
        for _ in 1..threads {
            s.spawn(&work);
        }
        work();
    });
}

/// Blend two BGRA pixels, f in 0..=256 towards `b`, two channels per masked lane.
#[inline(always)]
fn lerp2(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = ((a & 0x00FF_00FF) * g + (b & 0x00FF_00FF) * f) >> 8 & 0x00FF_00FF;
    let ag = ((a >> 8) & 0x00FF_00FF) * g + ((b >> 8) & 0x00FF_00FF) * f & 0xFF00_FF00;
    rb | ag
}

fn bytemuck_u32(b: &[u8]) -> &[u32] {
    // tiny-skia pixmaps are 4-byte aligned RGBA/BGRA rows
    assert!(b.as_ptr() as usize % 4 == 0 && b.len() % 4 == 0);
    unsafe { std::slice::from_raw_parts(b.as_ptr() as *const u32, b.len() / 4) }
}

// ------------------------------------------------------------------ snapshots

struct Snap {
    img: Arc<Pixmap>,
    rect: R, // screen pixels
    at: Instant,
}

/// Snapshot width limit. The restore ends on the snapshot laid exactly over the real window, so it has to be
/// full resolution: the old 1024 px ones showed as a blurry window that turned sharp when the real one took
/// over (and the other way round as a minimize began). Unscaled is no slower to grab than shrunk (~21 ms for
/// 1920x1040, HALFTONE scaling costs as much as it saves).
const SNAP_W: i32 = 2560;
/// Memory for snapshots of minimized windows (newest first; older ones come back from disk) and for windows
/// still on screen; the newest of each kind is always kept. About what 14 shrunk snapshots used to take.
const SNAP_BYTES_MINIMIZED: usize = 32 << 20;
const SNAP_BYTES_ON_SCREEN: usize = 24 << 20;

/// Copies a window's pixels straight off the screen (the window in front is what's visible there), shrunk to
/// at most `max_w` px wide by GDI (HALFTONE) so no bigger bitmap is ever allocated.
fn grab(r: RECT, max_w: i32) -> Option<Pixmap> {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let dw = w.min(max_w);
    let dh = ((h as f64 * dw as f64 / w as f64).round() as i32).max(1);
    unsafe {
        let sdc = GetDC(HWND::default());
        let mdc = CreateCompatibleDC(sdc);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: 40, biWidth: dw, biHeight: -dh, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let out = match CreateDIBSection(sdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => {
                let old = SelectObject(mdc, dib);
                SetStretchBltMode(mdc, HALFTONE);
                let _ = SetBrushOrgEx(mdc, 0, 0, None);
                let _ = StretchBlt(mdc, 0, 0, dw, dh, sdc, r.left, r.top, w, h, SRCCOPY);
                let _ = GdiFlush();
                SelectObject(mdc, old);
                let mut v = std::slice::from_raw_parts(bits as *const u8, (dw * dh * 4) as usize).to_vec();
                for px in v.chunks_exact_mut(4) {
                    px[3] = 255;
                }
                let _ = DeleteObject(dib);
                Pixmap::from_vec(v, IntSize::from_wh(dw as u32, dh as u32)?)
            }
            Err(_) => None,
        };
        let _ = DeleteDC(mdc);
        ReleaseDC(HWND::default(), sdc);
        out
    }
}

fn virtual_screen() -> R {
    unsafe {
        R {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            y: GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
        }
    }
}

/// A window's own pixels, read from DWM even where other windows cover it (PrintWindow with
/// PW_RENDERFULLCONTENT), cropped to its visible frame `r`. The screen grab only sees what is in front, so
/// this is what lets several windows animate at once. None if it fails, the window is wider than SNAP_W, or
/// it comes back black (some GPU-drawn windows do), so the caller can fall back to the screen.
fn grab_window(h: HWND, r: RECT) -> Option<Pixmap> {
    unsafe {
        let mut wr = RECT::default();
        GetWindowRect(h, &mut wr).ok()?;
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let (cx, cy, w, hh) = (r.left - wr.left, r.top - wr.top, r.right - r.left, r.bottom - r.top);
        if ww <= 0 || wh <= 0 || w <= 0 || hh <= 0 || w > SNAP_W || cx < 0 || cy < 0 || cx + w > ww || cy + hh > wh {
            return None;
        }
        let sdc = GetDC(HWND::default());
        let mdc = CreateCompatibleDC(sdc);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: 40, biWidth: ww, biHeight: -wh, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let out = match CreateDIBSection(sdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => {
                let old = SelectObject(mdc, dib);
                let ok = windows::Win32::Storage::Xps::PrintWindow(h, mdc, windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(2)).as_bool();
                let _ = GdiFlush();
                SelectObject(mdc, old);
                let src = std::slice::from_raw_parts(bits as *const u32, (ww * wh) as usize);
                let mut v: Vec<u8> = Vec::with_capacity((w * hh * 4) as usize);
                let mut lit = 0u32;
                for y in 0..hh {
                    let row = &src[((cy + y) * ww + cx) as usize..((cy + y) * ww + cx + w) as usize];
                    for &px in row {
                        if px & 0x00F0_F0F0 != 0 {
                            lit += 1;
                        }
                        v.extend_from_slice(&(px | 0xFF00_0000).to_le_bytes());
                    }
                }
                let _ = DeleteObject(dib);
                // black (or nearly) means the window did not render into the print: not a usable snapshot
                if ok && lit as i64 * 100 > (w as i64 * hh as i64) { Pixmap::from_vec(v, IntSize::from_wh(w as u32, hh as u32)?) } else { None }
            }
            Err(_) => None,
        };
        let _ = DeleteDC(mdc);
        ReleaseDC(HWND::default(), sdc);
        out
    }
}

/// grab_window for several windows at once, one thread each (~12-22 ms apiece). Those that fail are left out.
fn grab_windows(hs: &[isize]) -> Vec<(isize, Pixmap, R)> {
    std::thread::scope(|s| {
        let jobs: Vec<_> = hs
            .iter()
            .map(|&k| {
                s.spawn(move || {
                    let r = window_rect(hwnd_of(k))?;
                    let img = grab_window(hwnd_of(k), r)?;
                    Some((k, img, R { x: r.left as f64, y: r.top as f64, w: (r.right - r.left) as f64, h: (r.bottom - r.top) as f64 }))
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok().flatten()).collect()
    })
}

/// `wins` sorted by z-order, front first; any not found among the top-level windows go last.
fn front_first(wins: &[HWND]) -> Vec<HWND> {
    let mut order = Vec::with_capacity(wins.len());
    unsafe {
        let mut h = GetTopWindow(HWND::default()).unwrap_or_default();
        while !h.0.is_null() && order.len() < wins.len() {
            if wins.contains(&h) {
                order.push(h);
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    for &w in wins {
        if !order.contains(&w) {
            order.push(w);
        }
    }
    order
}

/// The window's visible frame on screen, clipped to all monitors; None if too small to bother.
fn window_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).is_err() {
            GetWindowRect(h, &mut r).ok()?;
        }
    }
    let v = virtual_screen();
    r.left = r.left.max(v.x as i32);
    r.top = r.top.max(v.y as i32);
    r.right = r.right.min(v.right() as i32);
    r.bottom = r.bottom.min(v.bottom() as i32);
    ((r.right - r.left) >= 80 && (r.bottom - r.top) >= 60).then_some(r)
}

fn snap_dir() -> String {
    format!("{}\\MacDock\\snaps", std::env::var("LOCALAPPDATA").unwrap_or_default())
}

fn snap_files(h: isize) -> Vec<std::path::PathBuf> {
    let prefix = format!("{h}_");
    std::fs::read_dir(snap_dir())
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with(&prefix)).unwrap_or(false)).collect())
        .unwrap_or_default()
}

fn forget(h: isize) {
    for f in snap_files(h) {
        let _ = std::fs::remove_file(f);
    }
}

/// Small diagnostic log of animations that could not play, kept under ~100 KB.
fn log(s: &str) {
    let path = format!("{}\\genie.log", crate::config::dir());
    if std::fs::metadata(&path).map(|m| m.len() > 100_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{t} {s}");
    }
}

thread_local! {
    static LOOP: std::cell::Cell<(f64, f64, u32)> = const { std::cell::Cell::new((0.0, 0.0, 0)) };
}

/// Main loop timings (dock frame ms, DwmFlush ms), summed while an animation runs.
pub fn loop_times(dock: f64, flush: f64) {
    LOOP.with(|c| {
        let (d, f, n) = c.get();
        c.set((d + dock, f + flush, n + 1));
    });
}

fn on_battery() -> bool {
    let mut ps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
    unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut ps).is_ok() && ps.ACLineStatus == 0 }
}

fn describe(h: HWND) -> String {
    let pid = shell::window_pid(h);
    format!("pid {pid} ({:?})", h.0)
}

// ------------------------------------------------------------------ the low-level mouse hook
// It runs on its own thread with its own message loop, so input never waits on our frame loop. It only
// posts to the overlay, except on release over an armed minimize button: then it shows the overlay at once.

static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static SHOW_ON_UP: AtomicIsize = AtomicIsize::new(0); // the overlay window holding the armed frame
static ARMED_FOR: AtomicIsize = AtomicIsize::new(0);
static ARM_X: AtomicI32 = AtomicI32::new(0);
static ARM_Y: AtomicI32 = AtomicI32::new(0);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

fn hwnd_of(v: isize) -> HWND {
    HWND(v as *mut _)
}

/// The window's thread holds the mouse capture (DefWindowProc tracking its caption button): the release then
/// still reaches it under our overlay. WinUI 3 / custom title bars don't capture, so they wait for the event.
fn holds_capture(h: HWND) -> bool {
    unsafe {
        let tid = GetWindowThreadProcessId(h, None);
        let mut gi = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        tid != 0 && GetGUIThreadInfo(tid, &mut gi).is_ok() && !gi.hwndCapture.0.is_null() && GetAncestor(gi.hwndCapture, GA_ROOT) == h
    }
}

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let ms = &*(l.0 as *const MSLLHOOKSTRUCT);
        let ov = hwnd_of(OVERLAY.load(Ordering::Relaxed));
        if w.0 == WM_LBUTTONDOWN as usize {
            let _ = PostMessageW(ov, WM_HOOK_DOWN, WPARAM(ms.pt.x as u32 as usize), LPARAM(ms.pt.y as isize));
        } else if w.0 == WM_LBUTTONUP as usize {
            let armed = ARMED_FOR.load(Ordering::Relaxed);
            if armed != 0 {
                // released where it was pressed: the window is about to minimize, so put the snapshot up now
                // instead of waiting for the (asynchronous) minimize event
                let near = (ms.pt.x - ARM_X.load(Ordering::Relaxed)).abs() < 12 && (ms.pt.y - ARM_Y.load(Ordering::Relaxed)).abs() < 12;
                if near && holds_capture(hwnd_of(armed)) {
                    let _ = ShowWindowAsync(hwnd_of(SHOW_ON_UP.load(Ordering::Relaxed)), SW_SHOWNOACTIVATE);
                }
                let _ = PostMessageW(ov, WM_HOOK_UP, WPARAM(0), LPARAM(0));
            }
        }
    }
    CallNextHookEx(None, code, w, l)
}

fn start_mouse_hook() {
    if HOOK_THREAD.load(Ordering::Relaxed) != 0 {
        return;
    }
    let _ = std::thread::Builder::new().name("mouse-hook".into()).spawn(|| unsafe {
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), HINSTANCE::from(GetModuleHandleW(None).unwrap_or_default()), 0);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
    });
}

fn stop_mouse_hook() {
    let tid = HOOK_THREAD.swap(0, Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

// ------------------------------------------------------------------ overlay window

unsafe extern "system" fn overlay_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER => push(Ev::Genie(GenieEv::Timer(w.0))),
        WM_CAPTURED => push(Ev::Genie(GenieEv::Captured)),
        WM_HOOK_DOWN => push(Ev::Genie(GenieEv::Down(w.0 as u32 as i32, l.0 as i32))),
        WM_HOOK_UP => push(Ev::Genie(GenieEv::Up)),
        WM_DISPLAYCHANGE => push(Ev::Genie(GenieEv::Display)),
        WM_DPICHANGED => return LRESULT(0), // we place ourselves in pixels
        _ => {}
    }
    DefWindowProcW(h, msg, w, l)
}

fn create_overlay() -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(overlay_proc), hInstance: hinst.into(), lpszClassName: w!("MacDockGenie"), ..Default::default() };
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("MacDockGenie"), w!("MacDock Genie"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        )
        .unwrap()
    }
}

// ------------------------------------------------------------------ the genie

struct Anim {
    /// the windows animating together, back to front (the last one is the window in front)
    items: Vec<Item>,
    icon: R,
    restore: bool,
    duration: f64, // of one window; the whole set takes `duration` plus the last one's delay
    frame: u32,
    start: Instant,
    /// (first, last) refresh time of the motion on the DWM clock; None falls back to `start`
    vsync: Option<(f64, f64)>,
    started: Then,
    done: Then,
    finished: Option<Instant>,
    /// restore: when the windows were first seen out of the dock
    shown: Option<Instant>,
    stats: Stats,
}

/// One window in an animation: its snapshot, where it sits, and how long after the start it begins to move.
struct Item {
    h: isize,
    img: Arc<Pixmap>,
    win: R,
    delay: f64,
}

/// Several windows of an app minimizing or restoring together set off one after another by this much, so
/// they pour into the icon (or out of it) in a cascade instead of as one lump.
const STAGGER: f64 = 0.05;

/// Frame timing of one animation, written to genie.log when it ends.
#[derive(Default)]
struct Stats {
    last: Option<Instant>,
    frames: u32,
    gap_sum: f64,
    gap_max: f64,
    raster: f64,
    present: f64,
}

pub struct Genie {
    hwnd: HWND,
    snaps: HashMap<isize, Snap>,
    capture: Option<(isize, R, Arc<Mutex<Option<Pixmap>>>)>,
    pending: Vec<(Instant, isize)>,
    anim: Option<Anim>,
    armed: Option<(isize, Arc<Pixmap>, R)>,
    surface: Option<(Surface, R)>, // CPU path: pixels and where they map on screen
    gpu: Option<Gpu>,
    gpu_area: Option<R>, // GPU path: where the overlay sits on screen
    gpu_tried: bool,
    original_min_animate: Option<bool>,
    paused: bool,
    stores: u32,
    /// (minimize, restore) length in seconds, from the settings
    pub secs: (f64, f64),
}

impl Genie {
    pub fn new() -> Genie {
        let hwnd = create_overlay();
        OVERLAY.store(hwnd.0 as isize, Ordering::Relaxed);
        Genie {
            hwnd, snaps: HashMap::new(), capture: None, pending: Vec::new(), anim: None, armed: None, surface: None,
            gpu: None, gpu_area: None, gpu_tried: false, original_min_animate: None, paused: false, stores: 0,
            secs: (0.56, 0.5),
        }
    }

    pub fn start(&mut self) {
        // Windows' own minimize animation would fight ours
        self.original_min_animate = Some(read_min_animate().unwrap_or(true));
        set_min_animate(false);
        self.clean_persisted();
        start_mouse_hook();
        unsafe {
            SetTimer(self.hwnd, TIMER_CAPTURE, 6000, None);
        }
    }

    pub fn stop(&mut self) {
        stop_mouse_hook();
        if let Some(on) = self.original_min_animate.take() {
            set_min_animate(on);
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    pub fn busy(&self) -> bool {
        self.anim.is_some()
    }

    /// Game mode: no mouse hook in the input path, no screen captures, drop what can be captured again.
    pub fn pause(&mut self, on: bool) {
        if on == self.paused {
            return;
        }
        self.paused = on;
        unsafe {
            if on {
                let _ = KillTimer(self.hwnd, TIMER_CAPTURE);
                stop_mouse_hook();
                self.disarm();
                self.snaps.retain(|&k, _| IsIconic(hwnd_of(k)).as_bool());
                self.surface = None;
                self.gpu = None; // no GPU memory held while a game runs
                self.gpu_area = None;
                self.gpu_tried = false;
            } else {
                start_mouse_hook();
                SetTimer(self.hwnd, TIMER_CAPTURE, 6000, None);
            }
        }
    }

    // -------------------------------------------------------------- snapshots

    /// Off the UI thread unless `sync` (the dock or a minimize button is about to minimize it).
    fn capture(&mut self, h: HWND, sync: bool) {
        unsafe {
            if self.paused || h.0.is_null() || self.anim.is_some() || (self.capture.is_some() && !sync) || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                return;
            }
        }
        if shell::window_pid(h) == std::process::id() {
            return;
        }
        let Some(r) = window_rect(h) else { return };
        let rect = R { x: r.left as f64, y: r.top as f64, w: (r.right - r.left) as f64, h: (r.bottom - r.top) as f64 };
        if sync {
            if let Some(img) = grab(r, SNAP_W) {
                self.store(h.0 as isize, img, rect);
            }
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let ov = self.hwnd.0 as isize;
        let rr = (r.left, r.top, r.right, r.bottom);
        if std::thread::Builder::new()
            .name("snapshot".into())
            .spawn(move || {
                let img = grab(RECT { left: rr.0, top: rr.1, right: rr.2, bottom: rr.3 }, SNAP_W);
                if let Ok(mut g) = out.lock() {
                    *g = img;
                }
                unsafe {
                    let _ = PostMessageW(hwnd_of(ov), WM_CAPTURED, WPARAM(0), LPARAM(0));
                }
            })
            .is_ok()
        {
            self.capture = Some((h.0 as isize, rect, slot));
        }
    }

    fn captured(&mut self) {
        let Some((h, rect, slot)) = self.capture.take() else { return };
        let img = slot.lock().ok().and_then(|mut g| g.take());
        if let Some(img) = img {
            self.store(h, img, rect);
        }
    }

    fn store(&mut self, h: isize, img: Pixmap, rect: R) {
        if self.paused {
            return;
        }
        self.snaps.insert(h, Snap { img: Arc::new(img), rect, at: Instant::now() });
        self.stores += 1;
        self.prune();
    }

    /// A minimized window can't be captured again, so its snapshot is the only way to animate it back: keep
    /// those (the newest in memory, all on disk) and evict only snapshots of windows still on screen.
    fn prune(&mut self) {
        let gone: Vec<isize> = self.snaps.keys().copied().filter(|&k| !unsafe { IsWindow(hwnd_of(k)) }.as_bool()).collect();
        for k in gone {
            self.snaps.remove(&k);
            forget(k);
        }
        for want_iconic in [false, true] {
            let mut v: Vec<(isize, Instant)> = self
                .snaps
                .iter()
                .filter(|(&k, _)| unsafe { IsIconic(hwnd_of(k)) }.as_bool() == want_iconic)
                .map(|(&k, s)| (k, s.at))
                .collect();
            let (max, budget) = if want_iconic { (MAX_MINIMIZED, SNAP_BYTES_MINIMIZED) } else { (MAX_SNAPS, SNAP_BYTES_ON_SCREEN) };
            v.sort_by_key(|x| std::cmp::Reverse(x.1)); // newest first
            let mut used = 0usize;
            for (i, (k, _)) in v.iter().enumerate() {
                used += self.snaps.get(k).map_or(0, |s| s.img.data().len());
                if i > 0 && (i >= max || used > budget) {
                    self.snaps.remove(k);
                }
            }
        }
    }

    // ---------- snapshots on disk, so minimized windows still animate back after a restart (same files as C#)

    fn persist(&self, h: HWND, s: &Snap) {
        let pid = shell::window_pid(h);
        let r = s.rect;
        let hv = h.0 as isize;
        let file = format!("{}\\{hv}_{pid}_{}_{}_{}_{}.png", snap_dir(), r.x as i32, r.y as i32, r.w as i32, r.h as i32);
        let img = s.img.clone();
        let _ = std::thread::Builder::new().name("persist".into()).spawn(move || {
            let _ = std::fs::create_dir_all(snap_dir());
            forget(hv);
            if !crate::gfx::save_png(&file, img.width(), img.height(), img.data()) {
                log(&format!("could not save {file}"));
            }
        });
    }

    /// Snapshot of a minimized window from disk (the hwnd may have been reused: the pid must still match).
    fn load_persisted(&self, h: HWND) -> Option<Snap> {
        let hv = h.0 as isize;
        for f in snap_files(hv) {
            let name = f.file_stem()?.to_string_lossy().into_owned();
            let p: Vec<&str> = name.split('_').collect();
            if p.len() != 6 {
                continue;
            }
            let n: Vec<f64> = p[2..].iter().filter_map(|s| s.parse().ok()).collect();
            if n.len() != 4 || p[1].parse::<u32>().ok() != Some(shell::window_pid(h)) {
                continue;
            }
            let img = crate::gfx::image_file(&f.to_string_lossy(), 4096)?;
            return Some(Snap { img: Arc::new(img), rect: R { x: n[0], y: n[1], w: n[2], h: n[3] }, at: Instant::now() });
        }
        None
    }

    /// On start: delete snapshots of windows that are gone or no longer minimized.
    fn clean_persisted(&self) {
        let Ok(dir) = std::fs::read_dir(snap_dir()) else { return };
        for f in dir.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let p: Vec<&str> = name.trim_end_matches(".png").split('_').collect();
            let keep = p.len() == 6
                && match (p[0].parse::<isize>(), p[1].parse::<u32>()) {
                    (Ok(h), Ok(pid)) => {
                        let live = unsafe { IsWindow(hwnd_of(h)).as_bool() && IsIconic(hwnd_of(h)).as_bool() };
                        live && shell::window_pid(hwnd_of(h)) == pid
                    }
                    _ => false,
                };
            if !keep {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }

    // -------------------------------------------------------------- playing

    /// Make sure the surface covers `area` (screen pixels).
    fn ensure_surface(&mut self, area: R) {
        if !self.gpu_tried {
            self.gpu_tried = true;
            match Gpu::new() {
                Ok(g) => self.gpu = Some(g),
                Err(e) => log(&format!("GPU genie unavailable, using the CPU: {e}")),
            }
        }
        if let Some(g) = self.gpu.as_mut() {
            if let Some(r) = &self.gpu_area {
                if r.x <= area.x && r.y <= area.y && r.right() >= area.right() && r.bottom() >= area.bottom() {
                    return;
                }
            }
            let a = R { x: area.x.floor(), y: area.y.floor(), w: area.w.ceil() + 2.0, h: area.h.ceil() + 2.0 };
            match g.set_area(a.x as i32, a.y as i32, a.w as u32, a.h as u32) {
                Ok(()) => {
                    self.gpu_area = Some(a);
                    return;
                }
                Err(e) => self.gpu_failed(&e),
            }
        }
        if let Some((_, r)) = &self.surface {
            if r.x <= area.x && r.y <= area.y && r.right() >= area.right() && r.bottom() >= area.bottom() {
                return;
            }
        }
        let a = R { x: area.x.floor(), y: area.y.floor(), w: area.w.ceil() + 2.0, h: area.h.ceil() + 2.0 };
        self.surface = Surface::new(a.w as u32, a.h as u32).map(|s| (s, a));
    }

    /// Render the mesh at progress p and push only its bounding box to the overlay.
    /// The GPU path broke (device lost, driver reset…): continue on the CPU from here on.
    fn gpu_failed(&mut self, e: &windows::core::Error) {
        log(&format!("GPU genie failed, falling back to the CPU: {e}"));
        let visible = self.gpu.as_ref().is_some_and(|g| unsafe { IsWindowVisible(g.hwnd).as_bool() });
        self.gpu = None;
        if let Some(a) = self.gpu_area.take() {
            self.surface = Surface::new(a.w as u32, a.h as u32).map(|s| (s, a));
        }
        if visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }

    /// The window that shows the animation: the GPU overlay when that path works, else the layered one.
    fn overlay(&self) -> HWND {
        self.gpu.as_ref().map_or(self.hwnd, |g| g.hwnd)
    }

    /// Returns (raster ms, present ms); on the GPU path both are CPU time spent submitting.
    fn draw(&mut self, img: &Arc<Pixmap>, win: R, icon: R, p: f64) -> (f64, f64) {
        self.draw_set(&[(img, win, p)], icon, 1.0)
    }

    /// Draw several windows at their own progress, back to front, all faded by `alpha`. The CPU fallback only
    /// draws the last (front) one: its rasterizer owns whole lines.
    fn draw_set(&mut self, set: &[(&Arc<Pixmap>, R, f64)], icon: R, alpha: f64) -> (f64, f64) {
        let Some(&(img, win, p)) = set.last() else { return (0.0, 0.0) };
        if let (Some(g), Some(a)) = (self.gpu.as_mut(), self.gpu_area) {
            let t0 = Instant::now();
            let rows: Vec<Vec<(f64, f64, f64)>> = set.iter().map(|&(_, w, p)| shape(w, icon, p)).collect();
            let meshes: Vec<(&Arc<Pixmap>, &[(f64, f64, f64)])> = set.iter().zip(&rows).map(|(&(i, _, _), r)| (i, r.as_slice())).collect();
            match g.draw_many(&meshes, (a.x, a.y), alpha as f32) {
                Ok(()) => return (0.0, t0.elapsed().as_secs_f64() * 1e3),
                Err(e) => self.gpu_failed(&e),
            }
        }
        let rows = shape(win, icon, p);
        let Some((sf, area)) = self.surface.as_mut() else { return (0.0, 0.0) };
        let (mut l, mut r) = (f64::MAX, f64::MIN);
        for &(a, b, _) in &rows {
            l = l.min(a);
            r = r.max(b);
        }
        let (t, b) = (rows[0].2, rows[ROWS].2);
        // bounding box in surface pixels, one pixel of slack for the antialiased edges
        let x0 = ((l - area.x).floor() as i32 - 1).clamp(0, sf.w as i32);
        let y0 = ((t - area.y).floor() as i32 - 1).clamp(0, sf.h as i32);
        let x1 = ((r - area.x).ceil() as i32 + 1).clamp(0, sf.w as i32);
        let y1 = ((b - area.y).ceil() as i32 + 1).clamp(0, sf.h as i32);
        let (bw, bh) = ((x1 - x0).max(1), (y1 - y0).max(1));
        let stride = sf.w as usize;
        let start = (y0 as usize * stride + x0 as usize) * 4;
        let bytes = sf.bytes();
        let t0 = Instant::now();
        if start < bytes.len() {
            raster(&mut bytes[start..], stride, area.x + x0 as f64, area.y + y0 as f64, bw as usize, bh as usize, &rows, img);
        }
        let t1 = Instant::now();
        let a8 = (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        sf.present_part_alpha(self.hwnd, ((area.x as i32) + x0, (area.y as i32) + y0), (x0, y0), bw, bh, a8);
        ((t1 - t0).as_secs_f64() * 1e3, t1.elapsed().as_secs_f64() * 1e3)
    }

    fn hide_overlay(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            if let Some(g) = self.gpu.as_mut() {
                let _ = ShowWindow(g.hwnd, SW_HIDE);
                g.release_texture();
                g.release_buffers();
            }
        }
        self.gpu_area = None; // set_area rebuilds the buffers for the next animation
        self.surface = None;
    }

    /// While a window's minimize button is held, draw its snapshot into the hidden overlay; the release (in
    /// the hook thread) only has to show it, since the frame is already there.
    fn maybe_arm(&mut self, x: i32, y: i32) {
        if self.anim.is_some() || self.armed.is_some() || self.paused {
            return;
        }
        unsafe {
            let pt = windows::Win32::Foundation::POINT { x, y };
            let h = GetAncestor(WindowFromPoint(pt), GA_ROOT);
            if h.0.is_null() || shell::window_pid(h) == std::process::id() {
                return;
            }
            let lp = LPARAM((((y & 0xFFFF) << 16) | (x & 0xFFFF)) as isize);
            let mut hit = 0usize;
            if SendMessageTimeoutW(h, WM_NCHITTEST, WPARAM(0), lp, SMTO_ABORTIFHUNG, 50, Some(&mut hit)).0 == 0 || hit as isize != HTMINBUTTON {
                return;
            }
            self.capture(h, true);
            let Some(s) = self.snaps.get(&(h.0 as isize)) else { return };
            let (img, rect) = (s.img.clone(), s.rect);
            self.ensure_surface(rect);
            self.draw(&img, rect, rect, 0.0);
            self.armed = Some((h.0 as isize, img, rect));
            ARM_X.store(x, Ordering::Relaxed);
            ARM_Y.store(y, Ordering::Relaxed);
            SHOW_ON_UP.store(self.overlay().0 as isize, Ordering::Relaxed);
            ARMED_FOR.store(h.0 as isize, Ordering::Relaxed);
        }
    }

    fn disarm(&mut self) {
        ARMED_FOR.store(0, Ordering::Relaxed);
        if self.armed.take().is_some() && self.anim.is_none() {
            self.hide_overlay();
        }
    }

    /// One window; both rects in screen pixels. False if the overlay is busy.
    fn play(&mut self, img: Arc<Pixmap>, win: R, icon: R, restore: bool, started: Then, done: Then) -> bool {
        self.play_set(vec![Item { h: 0, img, win, delay: 0.0 }], icon, restore, started, done)
    }

    /// Several windows, back to front. Minimizing, the front one leaves first; restoring, it arrives last, so
    /// it ends up on top.
    fn play_set(&mut self, mut items: Vec<Item>, icon: R, restore: bool, started: Then, done: Then) -> bool {
        if items.is_empty() {
            return false;
        }
        if self.anim.as_ref().is_some_and(|a| a.finished.is_some()) {
            // the last one is only dissolving into its window: cut that short rather than skip this one
            self.anim = None;
            self.hide_overlay();
        }
        if self.anim.is_some() {
            return false;
        }
        let n = items.len();
        for (i, it) in items.iter_mut().enumerate() {
            it.delay = STAGGER * if restore { i } else { n - 1 - i } as f64;
        }
        let front = items.last().unwrap();
        let prepared = n == 1 && self.armed.as_ref().is_some_and(|a| Arc::ptr_eq(&a.1, &front.img));
        self.armed = None;
        ARMED_FOR.store(0, Ordering::Relaxed);
        let area = items.iter().fold(icon, |a, it| a.union(&it.win));
        self.ensure_surface(area);
        let p0 = if restore { 1.0 } else { 0.0 };
        let set: Vec<(&Arc<Pixmap>, R, f64)> = items.iter().map(|it| (&it.img, it.win, p0)).collect();
        self.draw_set(&set, icon, 1.0);
        unsafe {
            let _ = ShowWindow(self.overlay(), SW_SHOWNOACTIVATE);
        }
        self.anim = Some(Anim {
            items, icon, restore, duration: if restore { self.secs.1 } else { self.secs.0 }, frame: if prepared { 2 } else { 0 },
            start: Instant::now(), vsync: None, started, done, finished: None, shown: None, stats: Stats::default(),
        });
        true
    }

    /// Called once per display frame while busy. Frame 1: our first frame is on screen; frame 3: it has been
    /// composed, so the real window may now vanish under it and the motion starts.
    pub fn frame(&mut self) {
        let Some(mut a) = self.anim.take() else { return };
        if let Some(f) = a.finished {
            if !a.restore {
                self.hide_overlay(); // already inside the icon
                return;
            }
            // wait (without blocking) until the window is out of the dock, let it paint, then dissolve
            if a.shown.is_none() {
                let out = a.items.iter().all(|it| it.h == 0 || unsafe { !IsIconic(hwnd_of(it.h)).as_bool() });
                if out || f.elapsed().as_secs_f64() > RESTORE_WAIT_MAX {
                    a.shown = Some(Instant::now());
                }
            }
            let fade = a.shown.map_or(-1.0, |s| s.elapsed().as_secs_f64() - RESTORE_PAINT);
            if fade >= RESTORE_FADE {
                self.hide_overlay();
                return;
            }
            if fade > 0.0 {
                let set: Vec<(&Arc<Pixmap>, R, f64)> = a.items.iter().map(|it| (&it.img, it.win, 0.0)).collect();
                self.draw_set(&set, a.icon, 1.0 - smooth(fade / RESTORE_FADE));
            }
            self.anim = Some(a);
            return;
        }
        a.frame += 1;
        if a.frame == 3 {
            LOOP.with(|c| c.set((0.0, 0.0, 0)));
            std::mem::replace(&mut a.started, Then::Nothing).run();
            a.start = Instant::now();
            a.vsync = next_vblank().map(|(v, _)| (v, v));
        }
        let elapsed = match (a.frame >= 3, a.vsync) {
            (false, _) => 0.0,
            (true, Some((first, last))) => match next_vblank() {
                Some((v, per)) => {
                    // never repeat or reverse a refresh step, even if DWM's clock hiccups
                    let v = if a.frame > 3 && v < last + per * 0.5 { last + per } else { v };
                    a.vsync = Some((first, v));
                    v - first
                }
                None => {
                    a.vsync = None;
                    a.start.elapsed().as_secs_f64()
                }
            },
            (true, None) => a.start.elapsed().as_secs_f64(),
        };
        let last = a.items.iter().map(|it| it.delay).fold(0.0, f64::max);
        let t = (elapsed / (a.duration + last)).min(1.0);
        let set: Vec<(&Arc<Pixmap>, R, f64)> = a
            .items
            .iter()
            .map(|it| {
                let ti = ((elapsed - it.delay) / a.duration).clamp(0.0, 1.0);
                (&it.img, it.win, if a.restore { 1.0 - ti } else { ti })
            })
            .collect();
        let (rt, pt) = self.draw_set(&set, a.icon, 1.0);
        if a.frame > 3 {
            let st = &mut a.stats;
            let now = Instant::now();
            if let Some(l) = st.last {
                let g = (now - l).as_secs_f64() * 1e3;
                st.frames += 1;
                st.gap_sum += g;
                st.gap_max = st.gap_max.max(g);
            }
            st.last = Some(now);
            st.raster += rt;
            st.present += pt;
        }
        if t >= 1.0 {
            std::mem::replace(&mut a.done, Then::Nothing).run();
            a.finished = Some(Instant::now());
            let st = &a.stats;
            if st.frames > 0 {
                let n = st.frames as f64;
                let avg = st.gap_sum / n;
                log(&format!(
                    "{} {}x {:.0}x{:.0}: {} frames, interval avg {avg:.1} ms max {:.1} ms, raster {:.2} ms, present {:.2} ms, dock {:.2} ms, flush {:.2} ms, on battery {}",
                    if a.restore { "restore" } else { "minimize" }, a.items.len(), a.items.last().map_or(0.0, |i| i.win.w), a.items.last().map_or(0.0, |i| i.win.h), st.frames, st.gap_max,
                    st.raster / (n + 1.0), st.present / (n + 1.0), LOOP.with(|c| { let (d, _, k) = c.get(); d / k.max(1) as f64 }), LOOP.with(|c| { let (_, f, k) = c.get(); f / k.max(1) as f64 }), on_battery()
                ));
            }
        }
        self.anim = Some(a);
    }

    /// A window started minimizing (title bar, keyboard, anything): fly its snapshot into the dock.
    pub fn on_minimize_start(&mut self, h: HWND, icon: R) {
        ARMED_FOR.store(0, Ordering::Relaxed);
        let key = h.0 as isize;
        if self.anim.as_ref().is_some_and(|a| a.items.iter().any(|it| it.h == key)) {
            return; // the dock minimized it as part of a set that is already flying (and saved)
        }
        let Some(s) = self.snaps.get_mut(&key) else {
            log(&format!("minimize without snapshot: {}", describe(h)));
            self.disarm();
            return;
        };
        s.at = Instant::now();
        let (img, rect) = (s.img.clone(), s.rect);
        if let Some(s) = self.snaps.get(&key) {
            self.persist(h, s);
        }
        if !self.play(img, rect, icon, false, Then::Nothing, Then::Nothing) {
            self.disarm();
        }
    }

    /// Minimize started by the dock: put the fresh snapshot on screen first, then minimize under it.
    /// Every open window of the app gets its own genie: the front one from the screen, the others (covered,
    /// at least in part) read from DWM with PrintWindow. A window that cannot be read just minimizes.
    pub fn minimize_from_dock(&mut self, h: HWND, wins: Vec<HWND>, icon: R) {
        self.capture(h, true);
        let open: Vec<HWND> = wins.iter().copied().filter(|&w| unsafe { IsWindowVisible(w).as_bool() && !IsIconic(w).as_bool() }).collect();
        let others: Vec<isize> = open.iter().filter(|&&w| w != h).map(|w| w.0 as isize).collect();
        let mut got: HashMap<isize, (Arc<Pixmap>, R)> = HashMap::new();
        if !self.paused {
            for (k, img, rect) in grab_windows(&others) {
                got.insert(k, (Arc::new(img), rect));
            }
        }
        if let Some(s) = self.snaps.get(&(h.0 as isize)) {
            got.insert(h.0 as isize, (s.img.clone(), s.rect));
        }
        // back to front, the window in front last
        let mut order = front_first(&open);
        order.retain(|&w| w != h);
        order.reverse();
        order.push(h);
        let mut items = Vec::new();
        for w in order {
            let k = w.0 as isize;
            if let Some((img, rect)) = got.remove(&k) {
                let snap = Snap { img: img.clone(), rect, at: Instant::now() };
                self.persist(w, &snap); // the restore reads it back if memory has let it go
                self.snaps.insert(k, snap);
                items.push(Item { h: k, img, win: rect, delay: 0.0 });
            }
        }
        self.prune();
        if !self.play_set(items, icon, false, Then::Minimize(wins.clone()), Then::Nothing) {
            minimize_all(&wins);
        }
    }

    /// Plays the window coming out of the dock, then restores it. Restores at once if there is no snapshot.
    /// Every minimized window of the app comes out with its own genie, the front one last (on top).
    pub fn restore(&mut self, front: HWND, minimized: Vec<HWND>, icon: R) {
        let then = Then::Restore(front, minimized.clone());
        let mut order: Vec<HWND> = minimized.into_iter().filter(|&w| w != front).collect();
        order.push(front);
        let mut items = Vec::new();
        for w in order {
            let key = w.0 as isize;
            let snap = match self.snaps.remove(&key) {
                Some(s) => Some(s),
                None => self.load_persisted(w),
            };
            forget(key);
            match snap {
                Some(s) => items.push(Item { h: key, img: s.img, win: s.rect, delay: 0.0 }),
                None => log(&format!("restore without snapshot: {}", describe(w))),
            }
        }
        if items.is_empty() {
            then.run();
            return;
        }
        if !self.play_set(items, icon, true, Then::Nothing, then.clone()) {
            log(&format!("restore skipped, overlay busy: {}", describe(front)));
            then.run();
        }
    }

    pub fn on_foreground_or_moved(&mut self, h: HWND) {
        // let the window finish painting after it comes to front or stops moving
        self.pending.retain(|p| p.1 != h.0 as isize);
        self.pending.push((Instant::now() + Duration::from_millis(300), h.0 as isize));
        unsafe {
            SetTimer(self.hwnd, TIMER_PENDING, 100, None);
        }
    }

    pub fn handle(&mut self, e: GenieEv) {
        match e {
            GenieEv::Timer(TIMER_CAPTURE) => self.capture(unsafe { GetForegroundWindow() }, false),
            GenieEv::Timer(TIMER_PENDING) => {
                let now = Instant::now();
                let fg = unsafe { GetForegroundWindow() }.0 as isize;
                let due: Vec<isize> = self.pending.iter().filter(|p| p.0 <= now).map(|p| p.1).collect();
                self.pending.retain(|p| p.0 > now);
                for h in due {
                    if h == fg {
                        self.capture(hwnd_of(h), false);
                    }
                }
                if self.pending.is_empty() {
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_PENDING);
                    }
                }
            }
            GenieEv::Timer(TIMER_DISARM) => {
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_DISARM);
                }
                // no minimize after the release: take the cover away again
                if self.anim.is_none() {
                    self.disarm();
                }
            }
            GenieEv::Timer(_) => {}
            GenieEv::Captured => self.captured(),
            GenieEv::Down(x, y) => self.maybe_arm(x, y),
            GenieEv::Up => unsafe {
                SetTimer(self.hwnd, TIMER_DISARM, 220, None);
            },
            GenieEv::Display => {
                self.surface = None;
                self.gpu = None; // the adapter may have changed with the displays
                self.gpu_area = None;
                self.gpu_tried = false;
            }
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test --release genie_bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn genie_bench() {
        let tex = Pixmap::new(1024, 640).unwrap();
        let win = R { x: 200.0, y: 100.0, w: 1800.0, h: 1100.0 };
        let icon = R { x: 1200.0, y: 1500.0, w: 64.0, h: 64.0 };
        let area = win.union(&icon);
        let (w, h) = (area.w as usize + 2, area.h as usize + 2);
        let mut buf = vec![0u8; w * h * 4];
        let frames = 60;
        let mut worst = 0.0f64;
        let t0 = Instant::now();
        for f in 0..frames {
            let p = f as f64 / (frames - 1) as f64;
            let rows = shape(win, icon, p);
            let s = Instant::now();
            raster(&mut buf, w, area.x, area.y, w, h, &rows, &tex);
            worst = worst.max(s.elapsed().as_secs_f64() * 1000.0);
        }
        let avg = t0.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        println!("raster avg {avg:.2} ms, worst {worst:.2} ms");
    }

    /// Cost of a shrunk and a full snapshot of a 1920x1040 area. cargo test --release grab_bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn grab_bench() {
        let r = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
        for w in [1024, SNAP_W] {
            let _ = grab(r, w);
            let t = Instant::now();
            let n = 10;
            let mut px = 0;
            for _ in 0..n {
                px = grab(r, w).unwrap().width();
            }
            println!("max {w}: {px} px wide, {:.1} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
        }
    }

    /// PrintWindow snapshots of every Windows Terminal window, covered or not; saves them to $OUT_DIR_SNAPS.
    /// cargo test --release window_grab -- --ignored --nocapture
    #[test]
    #[ignore]
    fn window_grab() {
        let out = std::env::var("OUT_DIR_SNAPS").unwrap_or(".".into());
        let mut wins: Vec<HWND> = Vec::new();
        unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::Win32::Foundation::BOOL {
            let v = &mut *(l.0 as *mut Vec<HWND>);
            let mut c = [0u16; 64];
            let n = GetClassNameW(h, &mut c);
            if IsWindowVisible(h).as_bool() && String::from_utf16_lossy(&c[..n as usize]) == "CASCADIA_HOSTING_WINDOW_CLASS" {
                v.push(h);
            }
            true.into()
        }
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut wins as *mut _ as isize));
        }
        for (i, h) in wins.iter().enumerate() {
            let Some(r) = window_rect(*h) else { continue };
            let t = Instant::now();
            let img = grab_window(*h, r);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            match img {
                Some(p) => {
                    let f = format!("{out}/term{i}.png");
                    crate::gfx::save_png(&f, p.width(), p.height(), p.data());
                    println!("{i}: {}x{} in {ms:.1} ms -> {f}", p.width(), p.height());
                }
                None => println!("{i}: no image ({ms:.1} ms)"),
            }
        }
    }

    /// Several real Windows Terminal windows mid-genie in one GPU frame (as a dock minimize of the app draws
    /// them), shown for a moment and grabbed off the screen to $SHOT.
    /// cargo test --release multi_shot -- --ignored --nocapture
    #[test]
    #[ignore]
    fn multi_shot() {
        let mut wins: Vec<HWND> = Vec::new();
        unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::Win32::Foundation::BOOL {
            let v = &mut *(l.0 as *mut Vec<HWND>);
            let mut c = [0u16; 64];
            let n = GetClassNameW(h, &mut c);
            if IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() && String::from_utf16_lossy(&c[..n as usize]) == "CASCADIA_HOSTING_WINDOW_CLASS" {
                v.push(h);
            }
            true.into()
        }
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut wins as *mut _ as isize));
        }
        let order: Vec<isize> = front_first(&wins).iter().rev().map(|h| h.0 as isize).collect();
        let got = grab_windows(&order);
        let items: Vec<(Arc<Pixmap>, R)> = order
            .iter()
            .filter_map(|k| got.iter().find(|g| g.0 == *k).map(|g| (Arc::new(g.1.clone()), g.2)))
            .collect();
        println!("{} windows grabbed", items.len());
        let icon = R { x: 900.0, y: 1000.0, w: 64.0, h: 64.0 };
        let area = items.iter().fold(icon, |a, it| a.union(&it.1));
        let mut g = Gpu::new().expect("gpu");
        g.set_area(area.x as i32, area.y as i32, area.w as u32 + 2, area.h as u32 + 2).unwrap();
        let ps = [0.15, 0.4, 0.65];
        let rows: Vec<Vec<(f64, f64, f64)>> = items.iter().enumerate().map(|(i, it)| shape(it.1, icon, ps[i % 3])).collect();
        let meshes: Vec<(&Arc<Pixmap>, &[(f64, f64, f64)])> = items.iter().zip(&rows).map(|(it, r)| (&it.0, r.as_slice())).collect();
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(400) {
            g.draw_many(&meshes, (area.x, area.y), 1.0).unwrap();
            unsafe {
                let _ = ShowWindow(g.hwnd, SW_SHOWNOACTIVATE);
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
        }
        let rc = RECT { left: area.x as i32, top: area.y as i32, right: area.right() as i32, bottom: area.bottom() as i32 };
        let shot = grab(rc, 4096).unwrap();
        let out = std::env::var("SHOT").unwrap_or("multi_shot.png".into());
        crate::gfx::save_png(&out, shot.width(), shot.height(), shot.data());
        println!("{}x{} -> {out}", shot.width(), shot.height());
    }

    /// Frame steps of the DWM refresh clock against Instant::now(), both sampled after DwmFlush plus some
    /// uneven work, as the main loop does. cargo test --release vblank_steps -- --ignored --nocapture
    #[test]
    #[ignore]
    fn vblank_steps() {
        let (mut vs, mut is) = (Vec::new(), Vec::new());
        for i in 0..240 {
            unsafe {
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
            let spin = Instant::now();
            while spin.elapsed() < Duration::from_micros((i * 7919 % 2500) as u64) {}
            vs.push(next_vblank().unwrap().0);
            is.push(Instant::now());
        }
        let per = next_vblank().unwrap().1 * 1e3;
        let dev = |d: Vec<f64>| {
            let m = d.iter().map(|x| (x - per).abs()).fold(0.0, f64::max);
            let skips = d.iter().filter(|x| **x > per * 1.5).count();
            (m, skips)
        };
        let v = dev(vs.windows(2).map(|w| (w[1] - w[0]) * 1e3).collect());
        let i = dev(is.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1e3).collect());
        println!("refresh {per:.3} ms; vblank clock: max step error {:.3} ms ({} skips); Instant: {:.3} ms ({} skips)", v.0, v.1, i.0, i.1);
    }

    /// Puts a GPU genie frame (p = 0.35) on screen for a moment and saves a screen grab of it.
    /// cargo test --release gpu_shot -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_shot() {
        let mut tex = Pixmap::new(512, 320).unwrap();
        for (i, px) in tex.data_mut().chunks_mut(4).enumerate() {
            let (x, y) = (i % 512, i / 512);
            let check = ((x / 32 + y / 32) % 2) as u8 * 120;
            px.copy_from_slice(&[(x / 2) as u8, check + 60, (y * 255 / 320) as u8, 255]);
        }
        let img = Arc::new(match std::env::var("SNAP") {
            Ok(f) => unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
                crate::gfx::image_file(&f, 4096).unwrap()
            },
            Err(_) => tex,
        });
        let win = R { x: 0.0, y: 29.0, w: 2560.0, h: 1473.0 };
        let icon = R { x: 1100.0, y: 1500.0, w: 80.0, h: 80.0 };
        let area = win.union(&icon);
        let mut g = Gpu::new().expect("gpu");
        g.set_area(area.x as i32, area.y as i32, area.w as u32 + 2, area.h as u32 + 2).unwrap();
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed() < Duration::from_millis(400) {
            g.draw(&img, &shape(win, icon, 0.35), (area.x, area.y), 1.0).unwrap();
            unsafe {
                let _ = ShowWindow(g.hwnd, SW_SHOWNOACTIVATE);
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
            n += 1;
        }
        println!("{n} frames in 400 ms");
        let rc = RECT { left: area.x as i32, top: area.y as i32, right: area.right() as i32, bottom: area.bottom() as i32 };
        let shot = grab(rc, 4096).unwrap();
        let out = std::env::var("SHOT").unwrap_or("gpu_shot.png".into());
        std::fs::write(&out, shot.data()).unwrap();
        println!("{}x{}", shot.width(), shot.height());
        println!("saved {out}");
    }

    /// At p = 0 the mesh is the window itself: output must match the texture scaled 1:1.
    #[test]
    fn identity_matches_texture() {
        let mut tex = Pixmap::new(64, 32).unwrap();
        for (i, px) in tex.data_mut().chunks_mut(4).enumerate() {
            px.copy_from_slice(&[(i % 64 * 4) as u8, (i / 64 * 8) as u8, 77, 255]);
        }
        let win = R { x: 10.0, y: 20.0, w: 64.0, h: 32.0 };
        let icon = R { x: 30.0, y: 200.0, w: 16.0, h: 16.0 };
        let rows = shape(win, icon, 0.0);
        let (w, h) = (100usize, 80usize);
        let mut buf = vec![0u8; w * h * 4];
        raster(&mut buf, w, 0.0, 0.0, w, h, &rows, &tex);
        for (x, y) in [(10usize, 20usize), (40, 30), (73, 51), (20, 45)] {
            let o = (y * w + x) * 4;
            let t = ((y - 20) * 64 + (x - 10)) * 4;
            for c in 0..4 {
                assert!((buf[o + c] as i32 - tex.data()[t + c] as i32).abs() <= 1, "({x},{y}) c{c}: {} vs {}", buf[o + c], tex.data()[t + c]);
            }
        }
        assert_eq!(&buf[(19 * w + 40) * 4..(19 * w + 40) * 4 + 4], &[0, 0, 0, 0]);
    }
}
