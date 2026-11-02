//! Drawing helpers: the layered-window surface, rounded rects, the wallpaper frost and GDI text masks.
//! Everything is premultiplied BGRA, the layout UpdateLayeredWindow wants, so tiny-skia renders straight
//! into the DIB with no copy. Colours are given as (b, g, r, a) for that reason.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use tiny_skia::{IntSize, Path, PathBuilder, Pixmap, PixmapMut};
use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{COLORREF, GENERIC_READ, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, UpdateLayeredWindow, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA,
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
        unsafe {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
            let pos = POINT { x, y };
            let size = SIZE { cx: self.w as i32, cy: self.h as i32 };
            let src = POINT::default();
            let _ = UpdateLayeredWindow(hwnd, HDC::default(), Some(&pos), Some(&size), self.dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    /// Push only part of the surface (`src` origin, `w` x `h`) and put the window at `dst` with that size.
    pub fn present_part(&self, hwnd: HWND, dst: (i32, i32), src: (i32, i32), w: i32, h: i32) {
        unsafe {
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
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

/// The wallpaper, "fill" fitted and shrunk hard to `fw` x `fh` (the shrink is the blur), via WIC.
pub fn wallpaper(fw: u32, fh: u32) -> Option<Pixmap> {
    unsafe {
        let mut buf = [0u16; 520];
        SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let mut path = String::from_utf16_lossy(&buf[..n]);
        if path.is_empty() || !std::path::Path::new(&path).exists() {
            path = format!("{}\\Microsoft\\Windows\\Themes\\TranscodedWallpaper", std::env::var("APPDATA").ok()?);
        }
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wp = wide(&path);
        let dec = f.CreateDecoderFromFilename(PCWSTR(wp.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        let s = (fw as f64 / w as f64).max(fh as f64 / h as f64);
        let dw = ((w as f64 * s).ceil() as u32).max(fw);
        let dh = ((h as f64 * s).ceil() as u32).max(fh);
        let sc = f.CreateBitmapScaler().ok()?;
        sc.Initialize(&frame, dw, dh, WICBitmapInterpolationModeFant).ok()?;
        let cv = f.CreateFormatConverter().ok()?;
        cv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let rc = WICRect { X: ((dw - fw) / 2) as i32, Y: ((dh - fh) / 2) as i32, Width: fw as i32, Height: fh as i32 };
        let mut v = vec![0u8; (fw * fh * 4) as usize];
        cv.CopyPixels(&rc, fw * 4, &mut v).ok()?;
        Pixmap::from_vec(v, IntSize::from_wh(fw, fh)?)
    }
}

/// Text style for [`text_ex`].
#[derive(Clone, Copy)]
pub struct Font {
    pub face: &'static str,
    pub px: i32,
    pub weight: i32,
}

pub const UI: &str = "Segoe UI Variable Text";
pub const ICONS: &str = "Segoe Fluent Icons";

/// White text as a premultiplied mask (Segoe UI Variable, grayscale antialiasing), `px` tall.
pub fn text(s: &str, px: i32) -> Option<Pixmap> {
    text_ex(s, Font { face: UI, px, weight: 400 }, (255, 255, 255))
}

/// Measure text in pixels without rasterizing it.
pub fn measure(s: &str, f: Font) -> (i32, i32) {
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

/// Three box blurs ≈ a gaussian with sigma ≈ radius / 2. Works on premultiplied data, all four channels.
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
            let mut fmt = GUID_WICPixelFormat32bppBGR;
            frame.SetPixelFormat(&mut fmt)?;
            frame.WritePixels(h, w * 4, bgra)?;
            frame.Commit()?;
            enc.Commit()?;
            Ok(())
        };
        run().is_ok()
    }
}
