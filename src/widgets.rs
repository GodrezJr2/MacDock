//! Desktop widgets (macOS Sonoma / Tahoe style): four small tiles in a 2x2 grid at the top right of the desktop
//! — analog clock, today's date, battery ring, CPU / memory / disk bars. One layered window kept at the bottom
//! of the z-order, so it sits on the wallpaper under every app. The tiles are frosted Liquid Glass over the
//! wallpaper (built once, rebuilt when the wallpaper or display changes); only the contents redraw, every 5 s and only when something shown changed.
//! A click opens the matching app (Clock, Calendar, battery settings, Task Manager).

use std::cell::RefCell;
use std::f32::consts::PI;

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};

const TILE: f32 = 164.0; // DIPs
const GAP: f32 = 14.0;
const RADIUS: f32 = 26.0;
const MARGIN: f32 = 22.0; // shadow room
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 5000;

const RED: (u8, u8, u8) = (0xFF, 0x45, 0x3A);
const GREEN: (u8, u8, u8) = (0x30, 0xD1, 0x58);
const YELLOW: (u8, u8, u8) = (0xFF, 0xD6, 0x0A);
const BLUE: (u8, u8, u8) = (0x0A, 0x84, 0xFF);
const PURPLE: (u8, u8, u8) = (0xBF, 0x5A, 0xF2);
const ORANGE: (u8, u8, u8) = (0xFF, 0x9F, 0x0A);

const DAYS: [&str; 7] = ["SUNDAY", "MONDAY", "TUESDAY", "WEDNESDAY", "THURSDAY", "FRIDAY", "SATURDAY"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
];

thread_local! {
    static STATE: RefCell<Option<Widgets>> = const { RefCell::new(None) };
}

struct Widgets {
    hwnd: HWND,
    s: f32,        // device px per DIP
    pos: (i32, i32), // window top-left, screen px
    size: u32,     // window is square
    glass: Option<Pixmap>, // shadows + frosted tiles, no content
    face: Option<Pixmap>,  // clock dial without hands
    surface: Option<Surface>,
    cpu_prev: Option<(u64, u64)>, // (idle, total) 100 ns units
    cpu: f32,
    shown: String, // what the last frame displayed; same again = no repaint
}

fn col(c: (u8, u8, u8), a: u8) -> Color {
    // surfaces are BGRA
    Color::from_rgba8(c.2, c.1, c.0, a)
}

fn paint(c: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(col(c, a));
    p.anti_alias = true;
    p
}

fn ft(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// (percent, plugged in, charging); None without a battery.
fn power() -> Option<(u8, bool, bool)> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).ok()? };
    if ps.BatteryFlag == 128 || ps.BatteryLifePercent > 100 {
        return None;
    }
    let ac = ps.ACLineStatus == 1;
    Some((ps.BatteryLifePercent, ac, ac && ps.BatteryFlag & 8 != 0))
}

fn memory() -> f32 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe {
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            return m.dwMemoryLoad as f32 / 100.0;
        }
    }
    0.0
}

fn disk() -> f32 {
    let (mut free, mut total) = (0u64, 0u64);
    unsafe {
        if GetDiskFreeSpaceExW(w!("C:\\"), None, Some(&mut total), Some(&mut free)).is_ok() && total > 0 {
            return 1.0 - free as f32 / total as f32;
        }
    }
    0.0
}

/// Work area (screen px): the menu bar and dock reserve their strips.
fn work_area() -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    r
}

impl Widgets {
    fn tile_origin(&self, i: usize) -> (f32, f32) {
        let s = self.s;
        ((MARGIN + (i % 2) as f32 * (TILE + GAP)) * s, (MARGIN + (i / 2) as f32 * (TILE + GAP)) * s)
    }

    fn place(&mut self) {
        self.s = unsafe { GetDpiForSystem() } as f32 / 96.0;
        let s = self.s;
        self.size = ((2.0 * MARGIN + 2.0 * TILE + GAP) * s).ceil() as u32;
        let wa = work_area();
        // tiles 20 DIPs from the right edge and 16 below the menu bar, the shadow margin outside that
        self.pos = (wa.right - ((20.0 - MARGIN) * s) as i32 - self.size as i32, wa.top + ((16.0 - MARGIN) * s) as i32);
        self.glass = None;
        self.face = None;
        self.surface = None;
        self.shown.clear();
    }

    /// Shadows + frosted glass for all four tiles: the wallpaper under the window, blurred, through the lens.
    fn build_glass(&self) -> Option<Pixmap> {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) };
        let n = self.size;
        let mut pm = Pixmap::new(n, n)?;
        // only the part of the wallpaper under us, blurred hard (frosted like macOS widgets)
        let full = gfx::wallpaper(sw, sh);
        let mut under = Pixmap::new(n, n)?;
        if let Some(full) = &full {
            under.draw_pixmap(-self.pos.0, -self.pos.1, full.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        } else {
            under.fill(Color::from_rgba8(0x40, 0x30, 0x28, 0xFF));
        }
        gfx::blur(&mut under, 26.0 * self.s);
        let (tw, r) = (TILE * self.s, RADIUS * self.s);
        for i in 0..4 {
            let (x, y) = self.tile_origin(i);
            if let Some(path) = gfx::rrect(x, y + 4.0 * self.s, tw, tw, r) {
                if let Some((sh_pm, ox, oy)) = gfx::shadow(&path, 14.0 * self.s, (0, 0, 0, 0x50)) {
                    pm.draw_pixmap(ox as i32, oy as i32, sh_pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
            }
            if let Some((mut g, gx, gy)) = gfx::liquid_glass(&under, x, y, tw, tw, r, 14.0 * self.s, (0.0, 0.0), (0.0, 0.0)) {
                // a dark veil keeps white text readable on any wallpaper
                let veil = paint((0, 0, 0), 0x46);
                if let Some(p) = gfx::rrect(x - gx as f32, y - gy as f32, tw, tw, r) {
                    let mut veil_pm = Pixmap::new(g.width(), g.height())?;
                    veil_pm.fill_path(&p, &veil, FillRule::Winding, Transform::identity(), None);
                    g.draw_pixmap(0, 0, veil_pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
                pm.draw_pixmap(gx, gy, g.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
        Some(pm)
    }

    /// Text with its top-left (anchor 0), top-centre (1) or top-right (2) at device (x, y).
    fn text(&self, pm: &mut Pixmap, s: &str, dip: f32, weight: i32, c: (u8, u8, u8), a: u8, x: f32, y: f32, anchor: u8) -> f32 {
        let Some(t) = gfx::text_ex(s, Font { face: gfx::UI, px: (dip * self.s).round() as i32, weight }, c) else { return 0.0 };
        let w = t.width() as f32;
        let x = match anchor {
            1 => x - w / 2.0,
            2 => x - w,
            _ => x,
        };
        let p = PixmapPaint { opacity: a as f32 / 255.0, ..Default::default() };
        pm.draw_pixmap(x.round() as i32, y.round() as i32, t.as_ref(), &p, Transform::identity(), None);
        w
    }

    /// The white clock dial with its 12 numbers, cached (hands are drawn per tick).
    fn build_face(&self) -> Option<Pixmap> {
        let s = self.s;
        let n = (TILE * s).ceil() as u32;
        let mut pm = Pixmap::new(n, n)?;
        let c = TILE * s / 2.0;
        let r = (TILE / 2.0 - 12.0) * s;
        let dial = PathBuilder::from_circle(c, c, r)?;
        pm.fill_path(&dial, &paint((0xF5, 0xF5, 0xF7), 0xFF), FillRule::Winding, Transform::identity(), None);
        for h in 1..=12 {
            let a = h as f32 / 12.0 * 2.0 * PI;
            let rr = r - 15.0 * s;
            let (x, y) = (c + a.sin() * rr, c - a.cos() * rr);
            let fh = gfx::measure("8", Font { face: gfx::UI, px: (13.0 * s).round() as i32, weight: 600 }).1 as f32;
            self.text(&mut pm, &h.to_string(), 13.0, 600, (0x1C, 0x1C, 0x1E), 0xFF, x, y - fh / 2.0, 1);
        }
        Some(pm)
    }

    fn hand(pm: &mut Pixmap, c: (f32, f32), angle: f32, len: f32, tail: f32, width: f32, color: (u8, u8, u8)) {
        let (dx, dy) = (angle.sin(), -angle.cos());
        let mut pb = PathBuilder::new();
        pb.move_to(c.0 - dx * tail, c.1 - dy * tail);
        pb.line_to(c.0 + dx * len, c.1 + dy * len);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &paint(color, 0xFF), &Stroke { width, line_cap: LineCap::Round, ..Default::default() }, Transform::identity(), None);
        }
    }

    fn arc(pm: &mut Pixmap, c: (f32, f32), r: f32, from: f32, to: f32, width: f32, p: &Paint) {
        let steps = (((to - from).abs() / (2.0 * PI)) * 96.0).ceil().max(2.0) as i32;
        let mut pb = PathBuilder::new();
        for i in 0..=steps {
            let a = from + (to - from) * i as f32 / steps as f32;
            let (x, y) = (c.0 + a.sin() * r, c.1 - a.cos() * r);
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        if let Some(path) = pb.finish() {
            pm.stroke_path(&path, p, &Stroke { width, line_cap: LineCap::Round, ..Default::default() }, Transform::identity(), None);
        }
    }

    fn sample_cpu(&mut self) {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
            return;
        }
        let (i, t) = (ft(idle), ft(kernel) + ft(user)); // kernel time includes idle
        if let Some((pi, pt)) = self.cpu_prev {
            let dt = t.saturating_sub(pt);
            if dt > 0 {
                self.cpu = 1.0 - i.saturating_sub(pi) as f32 / dt as f32;
            }
        }
        self.cpu_prev = Some((i, t));
    }

    fn render(&mut self) {
        if self.glass.is_none() {
            self.glass = self.build_glass();
        }
        if self.face.is_none() {
            self.face = self.build_face();
        }
        self.sample_cpu();
        let t = unsafe { GetLocalTime() };
        let key = format!("{:?}{} {} {:?} {:.0} {:.0} {:.0}", self.glass.is_some(), t.wMinute, t.wDay, power(), self.cpu * 100.0, memory() * 100.0, disk() * 100.0);
        if key == self.shown {
            return;
        }
        self.shown = key;
        let Some(glass) = &self.glass else { return };
        let mut pm = glass.clone();
        let s = self.s;
        let tw = TILE * s;
        let pad = 16.0 * s;

        // 1. clock
        let (x, y) = self.tile_origin(0);
        if let Some(face) = &self.face {
            pm.draw_pixmap(x as i32, y as i32, face.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
        let c = (x + tw / 2.0, y + tw / 2.0);
        let r = (TILE / 2.0 - 12.0) * s;
        let mins = t.wMinute as f32 + t.wSecond as f32 / 60.0;
        let hours = (t.wHour % 12) as f32 + mins / 60.0;
        let dark = (0x1C, 0x1C, 0x1E);
        Self::hand(&mut pm, c, hours / 12.0 * 2.0 * PI, r * 0.5, 0.0, 4.5 * s, dark);
        Self::hand(&mut pm, c, mins / 60.0 * 2.0 * PI, r * 0.78, 0.0, 3.0 * s, dark);
        if let Some(dot) = PathBuilder::from_circle(c.0, c.1, 3.6 * s) {
            pm.fill_path(&dot, &paint(ORANGE, 0xFF), FillRule::Winding, Transform::identity(), None);
        }

        // 2. date
        let (x, y) = self.tile_origin(1);
        self.text(&mut pm, DAYS[t.wDayOfWeek as usize % 7], 12.0, 700, RED, 0xFF, x + pad, y + pad, 0);
        self.text(&mut pm, &t.wDay.to_string(), 58.0, 300, (255, 255, 255), 0xFF, x + pad - 3.0 * s, y + pad + 12.0 * s, 0);
        let month = format!("{} {}", MONTHS[(t.wMonth as usize + 11) % 12], t.wYear);
        self.text(&mut pm, &month, 13.0, 600, (255, 255, 255), 0xB0, x + pad, y + tw - pad - 18.0 * s, 0);

        // 3. battery ring
        let (x, y) = self.tile_origin(2);
        let c = (x + tw / 2.0, y + tw / 2.0 - 10.0 * s);
        let rr = 44.0 * s;
        Self::arc(&mut pm, c, rr, 0.0, 2.0 * PI, 9.0 * s, &paint((255, 255, 255), 0x2C));
        let (label, pct) = match power() {
            Some((p, ac, charging)) => {
                let colr = if charging || ac { GREEN } else if p <= 20 { RED } else if p <= 40 { YELLOW } else { GREEN };
                if p > 0 {
                    Self::arc(&mut pm, c, rr, 0.0, p as f32 / 100.0 * 2.0 * PI, 9.0 * s, &paint(colr, 0xFF));
                }
                (if charging { "Charging" } else if ac { "Plugged In" } else { "Battery" }, format!("{p}%"))
            }
            None => ("AC Power", "—".to_string()),
        };
        let fh = gfx::measure("8", Font { face: gfx::UI, px: (24.0 * s).round() as i32, weight: 600 }).1 as f32;
        self.text(&mut pm, &pct, 24.0, 600, (255, 255, 255), 0xFF, c.0, c.1 - fh / 2.0, 1);
        self.text(&mut pm, label, 12.5, 600, (255, 255, 255), 0xB0, c.0, y + tw - pad - 16.0 * s, 1);

        // 4. system
        let (x, y) = self.tile_origin(3);
        self.text(&mut pm, "System", 13.0, 700, (255, 255, 255), 0xF0, x + pad, y + pad - 2.0 * s, 0);
        let rows = [("CPU", self.cpu, BLUE), ("Memory", memory(), PURPLE), ("Disk C:", disk(), ORANGE)];
        let bw = tw - 2.0 * pad;
        let mut ry = y + pad + 26.0 * s;
        for (name, v, colr) in rows {
            let v = v.clamp(0.0, 1.0);
            self.text(&mut pm, name, 11.5, 600, (255, 255, 255), 0xC0, x + pad, ry, 0);
            self.text(&mut pm, &format!("{:.0}%", v * 100.0), 11.5, 600, (255, 255, 255), 0xF0, x + tw - pad, ry, 2);
            let by = ry + 18.0 * s;
            if let Some(p) = gfx::rrect(x + pad, by, bw, 6.0 * s, 3.0 * s) {
                pm.fill_path(&p, &paint((255, 255, 255), 0x2C), FillRule::Winding, Transform::identity(), None);
            }
            if let Some(p) = gfx::rrect(x + pad, by, (bw * v).max(6.0 * s), 6.0 * s, 3.0 * s) {
                pm.fill_path(&p, &paint(colr, 0xFF), FillRule::Winding, Transform::identity(), None);
            }
            ry += 36.0 * s;
        }

        if self.surface.as_ref().is_none_or(|sf| sf.w != self.size) {
            self.surface = Surface::new(self.size, self.size);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.bytes().copy_from_slice(pm.data());
            sf.present(self.hwnd, self.pos.0, self.pos.1);
        }
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Which tile is under a client point (device px).
    fn tile_at(&self, px: f32, py: f32) -> Option<usize> {
        let tw = TILE * self.s;
        (0..4).find(|&i| {
            let (x, y) = self.tile_origin(i);
            px >= x && px < x + tw && py >= y && py < y + tw
        })
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_TICK => {
            STATE.with(|st| {
                if let Some(w) = st.borrow_mut().as_mut() {
                    w.render();
                }
            });
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            // wallpaper, work area (menu bar / dock reserve), resolution or scale changed
            STATE.with(|st| {
                if let Some(w) = st.borrow_mut().as_mut() {
                    w.place();
                    w.render();
                }
            });
        }
        WM_WINDOWPOSCHANGING => {
            // stay on the desktop: never above another window
            let p = &mut *(lp.0 as *mut WINDOWPOS);
            p.hwndInsertAfter = HWND_BOTTOM;
            p.flags &= !SWP_NOZORDER;
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONUP => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            let tile = STATE.with(|st| st.borrow().as_ref().and_then(|w| w.tile_at(x, y)));
            let target = match tile {
                Some(0) => "ms-clock:",
                Some(1) => "outlookcal:",
                Some(2) => "ms-settings:batterysaver",
                Some(3) => "taskmgr.exe",
                _ => "",
            };
            if !target.is_empty() {
                crate::shell::launch(target);
            }
            return LRESULT(0);
        }
        WM_NCHITTEST => {
            // only the tiles take clicks; the shadow margin and gap fall through to the desktop
            let (sx, sy) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            let hit = STATE.with(|st| st.borrow().as_ref().and_then(|w| w.tile_at(sx - w.pos.0 as f32, sy - w.pos.1 as f32)));
            return LRESULT(if hit.is_some() { HTCLIENT as isize } else { HTTRANSPARENT as isize });
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Create the widget window and start its 2 s tick.
pub fn start() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockWidgets"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("MacDockWidgets"), w!("MacDock Widgets"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return;
        };
        let mut wd = Widgets { hwnd, s: 1.0, pos: (0, 0), size: 1, glass: None, face: None, surface: None, cpu_prev: None, cpu: 0.0, shown: String::new() };
        wd.place();
        wd.render();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        STATE.with(|st| *st.borrow_mut() = Some(wd));
        SetTimer(hwnd, TIMER_TICK, TICK_MS, None);
    }
}
