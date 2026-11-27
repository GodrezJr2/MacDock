//! Drawing helpers: the layered-window surface, rounded rects, the wallpaper frost and GDI text masks.
//! Everything is premultiplied BGRA, the layout UpdateLayeredWindow wants, so tiny-skia renders straight
//! into the DIB with no copy. Colours are given as (b, g, r, a) for that reason.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use tiny_skia::{IntSize, Path, PathBuilder, Pixmap, PixmapMut};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, GENERIC_READ, HWND, LPARAM, POINT, RECT, SIZE};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowRect, IsWindowVisible, SystemParametersInfoW, UpdateLayeredWindow, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA,
};

use crate::shell::wide;

/// A top-down 32 bpp DIB selected into a memory DC.
pub struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pub w: u32,
    pub h: u32,
}

impl Surface {
    pub fn new(w: u32, h: u32) -> Option<Surface> {
        unsafe {
            let dc = CreateCompatibleDC(HDC::default());
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = null_mut();
            let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp);
            Some(Surface { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

    pub fn pixmap(&mut self) -> PixmapMut<'_> {
        let len = (self.w * self.h * 4) as usize;
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.bits, len) };
        PixmapMut::from_bytes(bytes, self.w, self.h).unwrap()
    }

    /// Push the pixels to the layered window and move/resize it in the same call.
    pub fn present(&self, hwnd: HWND, x: i32, y: i32) {
        self.present_alpha(hwnd, x, y, 255);
    }

    /// [`present`](Self::present) with the whole window at `alpha` (fading it costs nothing this way).
    pub fn present_alpha(&self, hwnd: HWND, x: i32, y: i32, alpha: u8) {
        unsafe {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
            let pos = POINT { x, y };
            let size = SIZE { cx: self.w as i32, cy: self.h as i32 };
            let src = POINT::default();
            let _ = UpdateLayeredWindow(hwnd, HDC::default(), Some(&pos), Some(&size), self.dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    /// Push only part of the surface (`src` origin, `w` x `h`) and put the window at `dst` with that size,
    /// the whole layer faded by `alpha` (0..=255).
    pub fn present_part_alpha(&self, hwnd: HWND, dst: (i32, i32), src: (i32, i32), w: i32, h: i32, alpha: u8) {
        unsafe {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
            let pos = POINT { x: dst.0, y: dst.1 };
            let size = SIZE { cx: w.max(1), cy: h.max(1) };
            let sp = POINT { x: src.0, y: src.1 };
            let _ = UpdateLayeredWindow(hwnd, HDC::default(), Some(&pos), Some(&size), self.dc, Some(&sp), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    pub fn bytes(&mut self) -> &mut [u8] {
        let len = (self.w * self.h * 4) as usize;
        unsafe { std::slice::from_raw_parts_mut(self.bits, len) }
    }
}

/// Change only a layered window's overall opacity, keeping its pixels (safe from any thread).
pub fn set_alpha(hwnd: HWND, alpha: u8) {
    unsafe {
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
        let _ = UpdateLayeredWindow(hwnd, HDC::default(), None, None, HDC::default(), None, COLORREF(0), Some(&blend), ULW_ALPHA);
    }
}

/// What is on screen at (x, y, w, h) right now (screen px), opaque.
pub fn capture_screen(x: i32, y: i32, w: u32, h: u32) -> Option<Pixmap> {
    let mut sf = Surface::new(w, h)?;
    unsafe {
        let sdc = GetDC(HWND::default());
        let ok = BitBlt(sf.dc, 0, 0, w as i32, h as i32, sdc, x, y, SRCCOPY | CAPTUREBLT);
        ReleaseDC(HWND::default(), sdc);
        ok.ok()?;
    }
    let mut v = sf.bytes().to_vec();
    for p in v.chunks_exact_mut(4) {
        p[3] = 255;
    }
    Pixmap::from_vec(v, IntSize::from_wh(w, h)?)
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

pub fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = r * 0.447_715; // r * (1 - 0.5523), cubic control offset from the corner
    let mut p = PathBuilder::new();
    p.move_to(x + r, y);
    p.line_to(x + w - r, y);
    p.cubic_to(x + w - k, y, x + w, y + k, x + w, y + r);
    p.line_to(x + w, y + h - r);
    p.cubic_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
    p.line_to(x + r, y + h);
    p.cubic_to(x + k, y + h, x, y + h - k, x, y + h - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + k, x + k, y, x + r, y);
    p.close();
    p.finish()
}

/// What the primary screen's desktop shows, "fill" fitted and shrunk hard to `fw` x `fh` (the shrink is the blur).
pub fn wallpaper(fw: u32, fh: u32) -> Option<Pixmap> {
    wallpaper_on(None, fw, fh)
}

/// What the desktop shows on the monitor at `mon` (screen px; None = the primary one), fitted to `fw` x `fh`.
/// The desktop window is rendered as it is on screen (PrintWindow), so every kind of background works: a picture,
/// a slideshow, Spotlight, a solid colour, a different picture per monitor, or a live wallpaper app hosted under it.
/// If that fails, the wallpaper file is read as before.
pub fn wallpaper_on(mon: Option<RECT>, fw: u32, fh: u32) -> Option<Pixmap> {
    let mon = mon.unwrap_or_else(|| crate::display::Display::primary().rect);
    if let Some(p) = desktop_shot().and_then(|(src, at)| {
        let r = RECT { left: mon.left - at.left, top: mon.top - at.top, right: mon.right - at.left, bottom: mon.bottom - at.top };
        fill_from(&src, Some(r), fw, fh)
    }) {
        return Some(p);
    }
    let path = spi_wallpaper()?;
    if path.is_empty() {
        return solid_background(fw, fh);
    }
    let path = if std::path::Path::new(&path).is_file() {
        path
    } else {
        // a slideshow or Spotlight picture that is gone: Windows keeps its own copy of what it shows
        format!("{}\\Microsoft\\Windows\\Themes\\TranscodedWallpaper", std::env::var("APPDATA").ok()?)
    };
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wp = wide(&path);
        let dec = f.CreateDecoderFromFilename(PCWSTR(wp.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        fill_from(&frame.cast().ok()?, None, fw, fh)
    }
}

/// The last desktop render, reused for a moment: one wallpaper change sends several windows asking at once
/// (dock, menu bar, widgets, each in two sizes), and a render of a big screen takes ~40 ms.
static DESKTOP_SHOT: std::sync::Mutex<Option<(std::time::Instant, SendBitmap, RECT)>> = std::sync::Mutex::new(None);

struct SendBitmap(IWICBitmap);
// WIC bitmaps are free-threaded
unsafe impl Send for SendBitmap {}

/// Drop the shared render: the desktop is changing (a new wallpaper fades in over about a second).
pub fn forget_desktop() {
    if let Ok(mut c) = DESKTOP_SHOT.lock() {
        *c = None;
    }
}

/// The desktop window (the wallpaper and anything drawn on it) as WIC pixels, and where it sits on screen.
fn desktop_shot() -> Option<(IWICBitmapSource, RECT)> {
    let mut cache = DESKTOP_SHOT.lock().ok()?;
    if let Some((t, b, r)) = cache.as_ref() {
        if t.elapsed() < std::time::Duration::from_secs(2) {
            return Some((b.0.cast().ok()?, *r));
        }
    }
    let (bmp, r) = render_desktop()?;
    *cache = Some((std::time::Instant::now(), SendBitmap(bmp.clone()), r));
    Some((bmp.cast().ok()?, r))
}

/// The window the wallpaper is drawn in: Progman, or with an older live-wallpaper layout the bottom-most
/// visible WorkerW (Progman is hidden then).
fn desktop_host() -> Option<(HWND, RECT)> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<HWND>);
        let mut c = [0u16; 16];
        let n = GetClassNameW(h, &mut c) as usize;
        let c = String::from_utf16_lossy(&c[..n]);
        if (c == "Progman" || c == "WorkerW") && IsWindowVisible(h).as_bool() {
            v.push(h);
        }
        true.into()
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut v as *mut _ as isize));
    }
    // top to bottom: the wallpaper is under everything else
    v.into_iter().rev().find_map(|h| {
        let mut r = RECT::default();
        unsafe { GetWindowRect(h, &mut r).ok()? };
        (r.right - r.left > 100 && r.bottom - r.top > 100).then_some((h, r))
    })
}

fn render_desktop() -> Option<(IWICBitmap, RECT)> {
    let (host, r) = desktop_host()?;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    unsafe {
        let sdc = GetDC(None);
        let dc = CreateCompatibleDC(sdc);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let out = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok().and_then(|bm| {
            let old = SelectObject(dc, bm);
            // PW_RENDERFULLCONTENT: what DWM shows, DirectComposition content and child windows included
            let ok = PrintWindow(host, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
            let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            let res = ok.then(|| {
                // the desktop is opaque; GDI leaves alpha at 0
                px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
                let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
                f.CreateBitmapFromMemory(w as u32, h as u32, &GUID_WICPixelFormat32bppBGRA, (w * 4) as u32, px).ok()
            }).flatten();
            SelectObject(dc, old);
            let _ = DeleteObject(bm);
            res
        });
        let _ = DeleteDC(dc);
        ReleaseDC(None, sdc);
        Some((out?, r))
    }
}

/// The desktop's background colour, as a pixmap.
fn solid_background(fw: u32, fh: u32) -> Option<Pixmap> {
    let c = unsafe { GetSysColor(COLOR_BACKGROUND) };
    let mut p = Pixmap::new(fw.max(1), fh.max(1))?;
    p.fill(tiny_skia::Color::from_rgba8((c & 0xFF) as u8, (c >> 8 & 0xFF) as u8, (c >> 16 & 0xFF) as u8, 255));
    Some(p)
}

fn spi_wallpaper() -> Option<String> {
    unsafe {
        let mut buf = [0u16; 520];
        SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// `src` (cut to `crop` first, if given), "fill" fitted (scaled to cover, centre cut) to `fw` x `fh` with WIC's
/// Fant filter, which averages every source pixel: a smooth blur when shrinking hard.
fn fill_from(src: &IWICBitmapSource, crop: Option<RECT>, fw: u32, fh: u32) -> Option<Pixmap> {
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let src: IWICBitmapSource = match crop {
            Some(c) => {
                let (mut sw, mut sh) = (0u32, 0u32);
                src.GetSize(&mut sw, &mut sh).ok()?;
                let x = c.left.clamp(0, sw as i32);
                let y = c.top.clamp(0, sh as i32);
                let r = WICRect { X: x, Y: y, Width: c.right.clamp(x, sw as i32) - x, Height: c.bottom.clamp(y, sh as i32) - y };
                if r.Width < 1 || r.Height < 1 {
                    return None;
                }
                let cl = f.CreateBitmapClipper().ok()?;
                cl.Initialize(src, &r).ok()?;
                cl.cast().ok()?
            }
            None => src.clone(),
        };
        let (mut w, mut h) = (0u32, 0u32);
        src.GetSize(&mut w, &mut h).ok()?;
        let s = (fw as f64 / w as f64).max(fh as f64 / h as f64);
        let dw = ((w as f64 * s).ceil() as u32).max(fw);
        let dh = ((h as f64 * s).ceil() as u32).max(fh);
        let sc = f.CreateBitmapScaler().ok()?;
        sc.Initialize(&src, dw, dh, WICBitmapInterpolationModeFant).ok()?;
        let cv = f.CreateFormatConverter().ok()?;
        cv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let rc = WICRect { X: ((dw - fw) / 2) as i32, Y: ((dh - fh) / 2) as i32, Width: fw as i32, Height: fh as i32 };
        let mut v = vec![0u8; (fw * fh * 4) as usize];
        cv.CopyPixels(&rc, fw * 4, &mut v).ok()?;
        Pixmap::from_vec(v, IntSize::from_wh(fw, fh)?)
    }
}

/// Text style for [`text_ex`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Font {
    pub face: &'static str,
    pub px: i32,
    pub weight: i32,
}

const UI: &str = "Segoe UI Variable Text";
static INTERFACE_FACE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn interface_fonts() -> &'static [String] {
    static FAMILIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FAMILIES.get_or_init(|| {
        unsafe extern "system" fn collect(font: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, data: LPARAM) -> i32 {
            let face = &(*font).lfFaceName;
            let end = face.iter().position(|&c| c == 0).unwrap_or(face.len());
            let name = String::from_utf16_lossy(&face[..end]);
            if !name.is_empty() && !name.starts_with('@') {
                (*(data.0 as *mut Vec<String>)).push(name);
            }
            1
        }
        let mut fonts = Vec::<String>::new();
        unsafe {
            let dc = GetDC(HWND::default());
            if !dc.0.is_null() {
                let query = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
                EnumFontFamiliesExW(dc, &query, Some(collect), LPARAM((&mut fonts as *mut Vec<String>) as isize), 0);
                ReleaseDC(HWND::default(), dc);
            }
        }
        fonts.sort_unstable();
        fonts.dedup();
        fonts.insert(0, "auto".into());
        fonts
    })
}

pub fn interface_font_index(face: &str) -> usize {
    interface_fonts().iter().position(|name| name == face).unwrap_or(0)
}

pub fn set_interface_font(face: &str) {
    let index = if face == "auto" { 0 } else { interface_font_index(face) };
    INTERFACE_FACE.store(index, std::sync::atomic::Ordering::Relaxed);
}

pub fn ui_face() -> &'static str {
    match INTERFACE_FACE.load(std::sync::atomic::Ordering::Relaxed) {
        0 if crate::i18n::lang() == crate::i18n::Lang::ZhCn => "Microsoft YaHei UI",
        0 => UI,
        i => &interface_fonts()[i],
    }
}
pub const ICONS: &str = "Segoe Fluent Icons";

/// White text as a premultiplied mask (Segoe UI Variable, grayscale antialiasing), `px` tall.
pub fn text(s: &str, px: i32) -> Option<Pixmap> {
    text_ex(s, Font { face: ui_face(), px, weight: 400 }, (255, 255, 255))
}

/// Measure text in pixels without rasterizing it.
/// Text is drawn and measured over and over with the same strings (every frame of an animation, every
/// redraw of a panel), and each GDI round trip makes a font, a DC and a bitmap; so both are remembered.
/// Per thread, and dropped wholesale when they grow past their cap (rendered text is kept smaller: RAM matters).
const TEXT_CACHE_MAX: usize = 3000;
const TEXT_PIX_MAX: usize = 400;

thread_local! {
    static MEASURED: std::cell::RefCell<std::collections::HashMap<(String, Font), (i32, i32)>> = Default::default();
    static RENDERED: std::cell::RefCell<std::collections::HashMap<(String, Font, (u8, u8, u8)), Option<Pixmap>>> = Default::default();
}

pub fn measure(s: &str, f: Font) -> (i32, i32) {
    let key = (s.to_string(), f);
    if let Some(v) = MEASURED.with(|m| m.borrow().get(&key).copied()) {
        return v;
    }
    let v = measure_gdi(s, f);
    MEASURED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > TEXT_CACHE_MAX {
            m.clear();
        }
        m.insert(key, v);
    });
    v
}

fn measure_gdi(s: &str, f: Font) -> (i32, i32) {
    unsafe {
        let dc = CreateCompatibleDC(HDC::default());
        let font = make_font(f);
        let old = SelectObject(dc, font);
        let ws: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &ws, &mut sz);
        SelectObject(dc, old);
        let _ = DeleteObject(font);
        let _ = DeleteDC(dc);
        (sz.cx, sz.cy)
    }
}

unsafe fn make_font(f: Font) -> HFONT {
    let face = wide(f.face);
    CreateFontW(
        -f.px, 0, 0, 0, f.weight, 0, 0, 0,
        DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
        PCWSTR(face.as_ptr()),
    )
}

/// Text in a colour (r, g, b) as a premultiplied BGRA pixmap, sized to the text's cell.
pub fn text_ex(s: &str, f: Font, rgb: (u8, u8, u8)) -> Option<Pixmap> {
    if s.is_empty() {
        return None;
    }
    let key = (s.to_string(), f, rgb);
    if let Some(v) = RENDERED.with(|m| m.borrow().get(&key).cloned()) {
        return v;
    }
    let v = text_gdi(s, f, rgb);
    RENDERED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > TEXT_PIX_MAX {
            m.clear();
        }
        m.insert(key, v.clone());
    });
    v
}

fn text_gdi(s: &str, f: Font, rgb: (u8, u8, u8)) -> Option<Pixmap> {
    unsafe {
        let dc = CreateCompatibleDC(HDC::default());
        let font = make_font(f);
        let old_font = SelectObject(dc, font);
        let ws: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &ws, &mut sz);
        let (w, h) = (sz.cx.max(1) as u32 + 2, sz.cy.max(1) as u32);
        let mut result = None;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        if let Ok(bmp) = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            let old_bmp = SelectObject(dc, bmp);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            SetBkMode(dc, TRANSPARENT);
            let _ = TextOutW(dc, 0, 0, &ws);
            let _ = GdiFlush();
            let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let mut v = vec![0u8; src.len()];
            let (r, g, b) = (rgb.0 as u32, rgb.1 as u32, rgb.2 as u32);
            for (d, s) in v.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                let a = s[0].max(s[1]).max(s[2]) as u32;
                d.copy_from_slice(&[(b * a / 255) as u8, (g * a / 255) as u8, (r * a / 255) as u8, a as u8]);
            }
            result = Pixmap::from_vec(v, IntSize::from_wh(w, h).unwrap());
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(bmp);
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(font);
        let _ = DeleteDC(dc);
        result
    }
}

/// Soft shadow of a path: the shape filled in `rgba` (straight r, g, b, a), blurred by `radius` px.
/// Returns the shadow pixmap and its top-left in the path's coordinate space (both in pixels).
pub fn shadow(path: &Path, radius: f32, rgba: (u8, u8, u8, u8)) -> Option<(Pixmap, f32, f32)> {
    let b = path.bounds();
    let pad = (radius * 2.0).ceil() + 2.0;
    let (ox, oy) = (b.left() - pad, b.top() - pad);
    let (w, h) = ((b.width() + pad * 2.0).ceil() as u32, (b.height() + pad * 2.0).ceil() as u32);
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let mut paint = tiny_skia::Paint::default();
    paint.anti_alias = true;
    paint.set_color(tiny_skia::Color::from_rgba8(rgba.2, rgba.1, rgba.0, rgba.3));
    pm.fill_path(path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::from_translate(-ox, -oy), None);
    blur(&mut pm, radius);
    Some((pm, ox, oy))
}

type ShadowKey = (u32, u32, u32, (u8, u8, u8, u8));
thread_local! {
    static RR_SHADOW: std::cell::RefCell<Option<(ShadowKey, Pixmap, f32, f32)>> = const { std::cell::RefCell::new(None) };
}

/// A rounded rect's soft shadow at any height, cheaply: a short one is blurred once per width and corner, and a
/// taller one is its top half, its middle row repeated, then its bottom half (rows clear of the corners and the
/// blur are all alike). Spotlight's glass grows every frame; blurring the whole shadow each time took ~20 ms.
pub fn rrect_shadow(x: f32, y: f32, w: f32, h: f32, r: f32, radius: f32, rgba: (u8, u8, u8, u8)) -> Option<(Pixmap, f32, f32)> {
    let pad = (radius * 2.0).ceil() + 2.0;
    let h0 = (2.0 * (r + pad) + 4.0).ceil();
    if h <= h0 {
        return shadow(&rrect(x, y, w, h, r)?, radius, rgba);
    }
    let key = (w.to_bits(), r.to_bits(), radius.to_bits(), rgba);
    RR_SHADOW.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|e| e.0 != key) {
            let (pm, ox, oy) = shadow(&rrect(0.0, 0.0, w, h0, r)?, radius, rgba)?;
            *c = Some((key, pm, ox, oy));
        }
        let (_, base, ox, oy) = c.as_ref()?;
        let extra = (h - h0).round() as usize;
        let (bw, bh) = (base.width() as usize, base.height() as usize);
        let mut out = Pixmap::new(bw as u32, (bh + extra) as u32)?;
        let (src, dst) = (base.data(), out.data_mut());
        let row = bw * 4;
        let m = bh / 2;
        dst[..m * row].copy_from_slice(&src[..m * row]);
        for k in 0..extra {
            dst[(m + k) * row..(m + k + 1) * row].copy_from_slice(&src[m * row..(m + 1) * row]);
        }
        dst[(m + extra) * row..].copy_from_slice(&src[m * row..]);
        Some((out, x + ox, y + oy))
    })
}

/// Three box blurs ≈ a gaussian with sigma ≈ radius / 2. Works on premultiplied data, all four channels.
/// `src` over `dst` at (x, y), premultiplied; opaque pixels are copied straight.
pub fn blit_over(dst: &mut tiny_skia::PixmapMut, src: &Pixmap, x: i32, y: i32) {
    let (dw, dh, sw, sh) = (dst.width() as i32, dst.height() as i32, src.width() as i32, src.height() as i32);
    let (x0, x1) = (x.max(0), (x + sw).min(dw));
    if x0 >= x1 {
        return;
    }
    let sd = src.data();
    let dd = dst.data_mut();
    for sy in 0..sh {
        let ty = y + sy;
        if ty < 0 || ty >= dh {
            continue;
        }
        let s = &sd[((sy * sw + (x0 - x)) * 4) as usize..((sy * sw + (x1 - x)) * 4) as usize];
        let d = &mut dd[((ty * dw + x0) * 4) as usize..((ty * dw + x1) * 4) as usize];
        over_sparse(d, s);
    }
}

/// A white wash over `pm`, fading from alpha `a0` at device row `top` to `a1` at `top + h` (rows relative to
/// the pixmap), only where `pm` is already covered, so the shape's edge stays put.
pub fn white_wash(pm: &mut Pixmap, top: f32, h: f32, a0: u8, a1: u8) {
    let w = pm.width() as usize;
    for (row, line) in pm.data_mut().chunks_exact_mut(w * 4).enumerate() {
        let u = ((row as f32 + 0.5 - top) / h.max(1.0)).clamp(0.0, 1.0);
        let a = (a0 as f32 + (a1 as f32 - a0 as f32) * u).round() as u32;
        if a == 0 {
            continue;
        }
        for px in line.chunks_exact_mut(4) {
            let cov = px[3] as u32;
            if cov == 0 {
                continue;
            }
            let s = a * cov / 255; // white at alpha a, clipped to this pixel's coverage
            let k = 255 - s;
            for c in &mut px[..3] {
                *c = (s + (*c as u32 * k + 127) / 255).min(cov) as u8;
            }
        }
    }
}

/// `white_wash` with a shade laid down first (`shade` = colour and alpha), in the same single pass: a glass
/// window's tint and wash, which as two tiny-skia fills cost a few ms a frame while Spotlight grows.
pub fn shade_wash(pm: &mut Pixmap, top: f32, h: f32, shade: (u8, u8, u8, u8), a0: u8, a1: u8) {
    let w = pm.width() as usize;
    let sa = shade.3 as u32;
    for (row, line) in pm.data_mut().chunks_exact_mut(w * 4).enumerate() {
        let u = ((row as f32 + 0.5 - top) / h.max(1.0)).clamp(0.0, 1.0);
        let a = (a0 as f32 + (a1 as f32 - a0 as f32) * u).round() as u32;
        for px in line.chunks_exact_mut(4) {
            let cov = px[3] as u32;
            if cov == 0 {
                continue;
            }
            let t = sa * cov / 255;
            let s = a * cov / 255;
            // pixels are BGRA: the shade's blue goes first
            for (c, sc) in px[..3].iter_mut().zip([shade.2, shade.1, shade.0]) {
                let v = (sc as u32 * t + *c as u32 * (255 - t) + 127) / 255;
                *c = (s + (v * (255 - s) + 127) / 255).min(cov) as u8;
            }
        }
    }
}

/// `src` over `dst` (same size, premultiplied): fast when `src` is mostly empty, like the layer of icons over the dock glass.
pub fn over_sparse(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let a = s[3] as u32;
        if a == 0 {
            continue;
        }
        if a == 255 {
            d.copy_from_slice(s);
            continue;
        }
        let k = 255 - a;
        for i in 0..4 {
            d[i] = (s[i] as u32 + (d[i] as u32 * k + 127) / 255) as u8;
        }
    }
}

/// A light blur (about 1 px): two [1 2 1] passes each way, in integers. Cheap enough to run on every
/// frame of video behind the dock.
pub fn soften(pm: &mut Pixmap) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    if w < 3 || h < 3 {
        return;
    }
    let data = pm.data_mut();
    let mut tmp = vec![0u8; data.len()];
    let row = w * 4;
    for _ in 0..2 {
        // horizontal: data -> tmp
        for y in 0..h {
            let (s, d) = (&data[y * row..][..row], &mut tmp[y * row..][..row]);
            d[..4].copy_from_slice(&s[..4]);
            d[row - 4..].copy_from_slice(&s[row - 4..]);
            for i in 4..row - 4 {
                d[i] = ((s[i - 4] as u16 + 2 * s[i] as u16 + s[i + 4] as u16 + 2) >> 2) as u8;
            }
        }
        // vertical: tmp -> data
        data[..row].copy_from_slice(&tmp[..row]);
        data[(h - 1) * row..].copy_from_slice(&tmp[(h - 1) * row..]);
        for y in 1..h - 1 {
            let (a, b, c) = (&tmp[(y - 1) * row..][..row], &tmp[y * row..][..row], &tmp[(y + 1) * row..][..row]);
            let d = &mut data[y * row..][..row];
            for i in 0..row {
                d[i] = ((a[i] as u16 + 2 * b[i] as u16 + c[i] as u16 + 2) >> 2) as u8;
            }
        }
    }
}

pub fn blur(pm: &mut Pixmap, radius: f32) {
    let r = ((radius / 2.0) * 0.9).round().max(1.0) as usize;
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let data = pm.data_mut();
    let mut tmp = vec![0u8; data.len()];
    for _ in 0..3 {
        box_pass(data, &mut tmp, w, h, r, true);
        box_pass(&tmp, data, w, h, r, false);
    }
}

fn box_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool) {
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| if horizontal { (line * w + i) * 4 } else { (i * w + line) * 4 };
    let div = (2 * r + 1) as u32;
    for line in 0..lines {
        let mut acc = [0u32; 4];
        // window [-r, r] around 0, clamped (outside counts as transparent)
        for i in 0..=r.min(n - 1) {
            let p = idx(line, i);
            for c in 0..4 {
                acc[c] += src[p + c] as u32;
            }
        }
        for i in 0..n {
            let p = idx(line, i);
            for c in 0..4 {
                dst[p + c] = (acc[c] / div) as u8;
            }
            let add = i + r + 1;
            if add < n {
                let q = idx(line, add);
                for c in 0..4 {
                    acc[c] += src[q + c] as u32;
                }
            }
            if i >= r {
                let q = idx(line, i - r);
                for c in 0..4 {
                    acc[c] -= src[q + c] as u32;
                }
            }
        }
    }
}

/// Decode an image file (PNG etc.) with WIC into premultiplied BGRA, shrunk to fit `max_px` if larger.
pub fn image_file(path: &str, max_px: u32) -> Option<Pixmap> {
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wp = wide(path);
        let dec = f.CreateDecoderFromFilename(PCWSTR(wp.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        decode(&f, &dec, max_px)
    }
}

/// Decode an encoded image held in memory (album art).
pub fn image_bytes(bytes: &[u8], max_px: u32) -> Option<Pixmap> {
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let st = f.CreateStream().ok()?;
        st.InitializeFromMemory(bytes).ok()?;
        let dec = f.CreateDecoderFromStream(&st, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        decode(&f, &dec, max_px)
    }
}

unsafe fn decode(f: &IWICImagingFactory, dec: &IWICBitmapDecoder, max_px: u32) -> Option<Pixmap> {
    {
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let s = (max_px as f64 / w.max(h) as f64).min(1.0);
        let (dw, dh) = (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1));
        let sc = f.CreateBitmapScaler().ok()?;
        sc.Initialize(&frame, dw, dh, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let cv = f.CreateFormatConverter().ok()?;
        cv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let mut v = vec![0u8; (dw * dh * 4) as usize];
        cv.CopyPixels(std::ptr::null(), dw * 4, &mut v).ok()?;
        Pixmap::from_vec(v, IntSize::from_wh(dw, dh)?)
    }
}

/// Write opaque BGRA pixels as a PNG with WIC. Callable from any thread (initializes COM for it).
pub fn save_png(path: &str, w: u32, h: u32, bgra: &[u8]) -> bool {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        let run = || -> windows::core::Result<()> {
            let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = f.CreateStream()?;
            let wp = wide(path);
            stream.InitializeFromFilename(PCWSTR(wp.as_ptr()), windows::Win32::Foundation::GENERIC_WRITE.0)?;
            let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(windows::core::Error::empty)?;
            frame.Initialize(None)?;
            frame.SetSize(w, h)?;
            // PNG has no 32bppBGR: asking for it makes the encoder pick 24 bpp, and our 4-byte pixels would
            // then be read as 3-byte ones (colour stripes). BGRA it takes as is; the encoder may still change
            // the format, so check what it settled on.
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut fmt)?;
            if fmt != GUID_WICPixelFormat32bppBGRA {
                return Err(windows::core::Error::empty());
            }
            frame.WritePixels(h, w * 4, bgra)?;
            frame.Commit()?;
            enc.Commit()?;
            Ok(())
        };
        run().is_ok()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn solid_background_is_the_desktop_colour() {
        let c = unsafe { windows::Win32::Graphics::Gdi::GetSysColor(windows::Win32::Graphics::Gdi::COLOR_BACKGROUND) };
        let p = super::solid_background(4, 2).unwrap();
        let px = p.pixel(3, 1).unwrap();
        assert_eq!((px.red(), px.green(), px.blue(), px.alpha()), ((c & 0xFF) as u8, (c >> 8 & 0xFF) as u8, (c >> 16 & 0xFF) as u8, 255));
    }

    #[test]
    fn wallpaper_loads_here() {
        unsafe { let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED); }
        assert!(super::wallpaper(64, 36).is_some());
    }

    /// A snapshot written to disk must come back with the same pixels (it used to come back striped).
    #[test]
    fn png_roundtrip() {
        let (w, h) = (37u32, 11u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for (i, p) in px.chunks_mut(4).enumerate() {
            p.copy_from_slice(&[(i * 7) as u8, (i * 3) as u8, (i * 11) as u8, 255]);
        }
        let path = format!(r"{}\target\png_roundtrip.png", env!("CARGO_MANIFEST_DIR"));
        assert!(super::save_png(&path, w, h, &px));
        let back = super::image_file(&path, 4096).expect("decode");
        let _ = std::fs::remove_file(&path);
        assert_eq!((back.width(), back.height()), (w, h));
        assert_eq!(back.data(), &px[..]);
    }
}

/// macOS 26 "Liquid Glass" body for a rounded rect at device px (`x`, `y`, `w`, `h`, corner `r`): the wallpaper
/// (`src`, in screen px with its top-left at screen px `src_at`) seen through it, almost sharp, bent inward
/// within `band` px of the rim like a lens, with a light specular edge brightest along the top.
/// `origin` is the screen position of device (0, 0). Returns the layer and where it goes.
pub fn liquid_glass(
    src: &Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, band: f32, origin: (f32, f32), src_at: (f32, f32),
) -> Option<(Pixmap, i32, i32)> {
    let (x0, y0) = (x.floor() as i32, y.floor() as i32);
    let (pw, ph) = ((x + w).ceil() as i32 - x0, (y + h).ceil() as i32 - y0);
    if pw <= 0 || ph <= 0 {
        return None;
    }
    let mut out = Pixmap::new(pw as u32, ph as u32)?;
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    let sd = src.data();
    let (cx, cy, hx, hy) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
    let od = out.data_mut();
    let px_at = |sx: i32, sy: i32| -> [f32; 4] {
        let i = ((sy.clamp(0, sh - 1) * sw + sx.clamp(0, sw - 1)) * 4) as usize;
        [sd[i] as f32, sd[i + 1] as f32, sd[i + 2] as f32, sd[i + 3] as f32]
    };
    // well inside the rim the lens does nothing: those spans are a straight copy of the source
    let inner = band.max(r);
    let (ix0, ix1) = (((x + inner) - x0 as f32).ceil().max(0.0) as i32, ((x + w - inner) - x0 as f32).floor().min(pw as f32) as i32);
    let (iy0, iy1) = ((y + band - y0 as f32).ceil() as i32, (y + h - band - y0 as f32).floor() as i32);
    let (offx, offy) = ((origin.0 - src_at.0).round() as i32 + x0, (origin.1 - src_at.1).round() as i32 + y0);
    for j in 0..ph {
        let copy_row = j >= iy0 && j < iy1 && ix1 > ix0;
        if copy_row {
            let (sy, sx0) = (offy + j, offx + ix0);
            if sy >= 0 && sy < sh && sx0 >= 0 && sx0 + (ix1 - ix0) <= sw {
                let s = ((sy * sw + sx0) * 4) as usize;
                let d = ((j * pw + ix0) * 4) as usize;
                let n = ((ix1 - ix0) * 4) as usize;
                od[d..d + n].copy_from_slice(&sd[s..s + n]);
            } else {
                for i in ix0..ix1 {
                    let p = px_at(offx + i, sy);
                    let d = ((j * pw + i) * 4) as usize;
                    for k in 0..4 {
                        od[d + k] = p[k] as u8;
                    }
                }
            }
        }
        // straight top and bottom edges: the lens only moves rows, the same for the whole span, so blend two source rows
        let band_row = !copy_row && ix1 > ix0 && (j < iy0 || j >= iy1);
        if band_row {
            let py = y0 as f32 + j as f32 + 0.5;
            let top = py - y < y + h - py;
            let depth = if top { py - y } else { y + h - py };
            let cov = (0.5 + depth).clamp(0.0, 1.0);
            if cov > 0.0 {
                let ny: f32 = if top { -1.0 } else { 1.0 };
                let e = (1.0 - depth / band).max(0.0);
                let pull = band * 0.9 * e * e;
                let scy = origin.1 + py - ny * pull - src_at.1 - 0.5;
                let fy = scy.floor();
                let ty = scy - fy;
                let (sy0, sy1) = ((fy as i32).clamp(0, sh - 1), (fy as i32 + 1).clamp(0, sh - 1));
                let lit = (0.35 + 0.65 * (-ny * 0.85).max(0.0)).min(1.0);
                let s = (1.0 - depth / (band * 0.35)).max(0.0);
                let spec = s * s * s * lit * 150.0;
                let (r0, r1) = ((sy0 * sw) as usize * 4, (sy1 * sw) as usize * 4);
                for i in ix0..ix1 {
                    let sx = (offx + i).clamp(0, sw - 1) as usize * 4;
                    let o = ((j * pw + i) * 4) as usize;
                    for k in 0..3 {
                        let (a, b) = (sd[r0 + sx + k] as f32, sd[r1 + sx + k] as f32);
                        od[o + k] = ((a + (b - a) * ty + spec).min(255.0) * cov) as u8;
                    }
                    od[o + 3] = (255.0 * cov) as u8;
                }
            }
        }
        for i in 0..pw {
            if (copy_row || band_row) && i >= ix0 && i < ix1 {
                continue;
            }
            let (px, py) = (x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5);
            // signed distance to the rounded rect, and the outward normal
            let (dx, dy) = (px - cx, py - cy);
            let (qx, qy) = (dx.abs() - hx, dy.abs() - hy);
            let (mx, my) = (qx.max(0.0), qy.max(0.0));
            let outer = (mx * mx + my * my).sqrt();
            let d = outer + qx.max(qy).min(0.0) - r;
            if d > 0.5 {
                continue;
            }
            let (mut nx, mut ny) = if outer > 0.0 { (mx / outer, my / outer) } else if qx > qy { (1.0, 0.0) } else { (0.0, 1.0) };
            nx *= dx.signum();
            ny *= dy.signum();
            let depth = -d; // px inside the rim
            // lens: near the rim, look further in (the wallpaper is pulled toward the edge and stretched)
            let e = (1.0 - depth / band).max(0.0);
            let pull = band * 0.9 * e * e;
            let (scx, scy) = (origin.0 + px - nx * pull - src_at.0 - 0.5, origin.1 + py - ny * pull - src_at.1 - 0.5);
            let (fx, fy) = (scx.floor(), scy.floor());
            let (tx, ty) = (scx - fx, scy - fy);
            let (ix, iy) = (fx as i32, fy as i32);
            let (a, b, c, dd) = (px_at(ix, iy), px_at(ix + 1, iy), px_at(ix, iy + 1), px_at(ix + 1, iy + 1));
            let mut col = [0f32; 4];
            for k in 0..4 {
                col[k] = (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty) + (c[k] * (1.0 - tx) + dd[k] * tx) * ty;
            }
            // specular rim: light from above, a little from the left
            let lit = (0.35 + 0.65 * (-ny * 0.85 - nx * 0.15).max(0.0)).min(1.0);
            let s = (1.0 - depth / (band * 0.35)).max(0.0);
            let spec = s * s * s * lit * 150.0;
            let cov = (0.5 - d).clamp(0.0, 1.0); // anti-aliased edge
            let o = ((j * pw + i) * 4) as usize;
            for k in 0..3 {
                od[o + k] = ((col[k] + spec).min(255.0) * cov) as u8;
            }
            od[o + 3] = (255.0 * cov) as u8;
        }
    }
    Some((out, x0, y0))
}

thread_local! {
    static D3D: std::cell::RefCell<Option<(windows::Win32::Graphics::Direct3D11::ID3D11Device, windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext)>> =
        const { std::cell::RefCell::new(None) };
}

/// The one D3D11 device the process uses (live glass backdrop and GPU genie). Each device costs the Intel iGPU
/// driver ~50 MB and ~50 worker threads, so two of them doubled the idle footprint. BGRA support (DirectComposition
/// swap chains) and multithread protection (capture frames arrive on a pool thread) are on. Recreated after a
/// device loss. Call from the UI thread.
pub fn d3d() -> Option<(windows::Win32::Graphics::Direct3D11::ID3D11Device, windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext)> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::*;
    D3D.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((dev, _)) = c.as_ref() {
            if unsafe { dev.GetDeviceRemovedReason() }.is_err() {
                *c = None;
            }
        }
        if c.is_none() {
            let (mut dev, mut ctx) = (None, None);
            unsafe {
                D3D11CreateDevice(
                    None, D3D_DRIVER_TYPE_HARDWARE, windows::Win32::Foundation::HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx),
                )
                .ok()?;
            }
            let (dev, ctx): (ID3D11Device, ID3D11DeviceContext) = (dev?, ctx?);
            if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                let _ = unsafe { mt.SetMultithreadProtected(true) };
            }
            *c = Some((dev, ctx));
        }
        c.clone()
    })
}
