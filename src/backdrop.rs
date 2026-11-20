//! What is really behind the dock. Each app window under the plate is captured live with Windows.Graphics.Capture,
//! one capture per window. A window capture never contains the dock, so there is no feedback loop and nothing is
//! hidden from screenshots. Frames land on a thread-pool thread, which only copies them into a GPU texture of our own
//! and pokes the dock. The dock reads back just the strip under its plate when it renders, and also re-reads it when
//! a window moves (a moved window sends no new frame).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tiny_skia::{Pixmap, PixmapPaint, Transform};
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the dock window: wParam 0 = a captured window drew a new frame, 1 = windows moved, opened or closed.
pub const WM_BACKDROP: u32 = WM_APP + 20;

const MAX_WINDOWS: usize = 8;

/// COM pointers moved into the frame handler. D3D11 is made thread-safe below (SetMultithreadProtected).
struct Sendable<T>(T);
unsafe impl<T> Send for Sendable<T> {}
unsafe impl<T> Sync for Sendable<T> {}
impl<T> Sendable<T> {
    // a method call makes closures capture the whole wrapper, not its !Send field
    fn get(&self) -> &T {
        &self.0
    }
}

#[derive(Default)]
struct Latest {
    tex: Option<ID3D11Texture2D>,
    w: u32,
    h: u32,
    fresh: bool,
}

struct Cap {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    latest: Arc<Sendable<Mutex<Latest>>>,
    rect: RECT,               // the window on screen when its crop was read
    crop: Option<(RECT, Pixmap)>, // screen rect and pixels of the part under the dock
}

impl Drop for Cap {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

pub struct Backdrop {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    rt: IDirect3DDevice,
    caps: HashMap<isize, Cap>,
    order: Vec<isize>, // top to bottom
    notify: isize,
    pending: Arc<AtomicBool>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    region: RECT,
}

thread_local! {
    static NOTIFY: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static LAYOUT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, _: u32, h: HWND, obj: i32, _: i32, _: u32, _: u32) {
    if obj != 0 || h.0.is_null() || LAYOUT_PENDING.get() || GetAncestor(h, GA_ROOT) != h {
        return;
    }
    LAYOUT_PENDING.set(true);
    let _ = PostMessageW(HWND(NOTIFY.get() as *mut _), WM_BACKDROP, WPARAM(1), LPARAM(0));
}

fn rect_of(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).ok()? };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

fn intersect(a: &RECT, b: &RECT) -> Option<RECT> {
    let r = RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// Visible, uncloaked app windows overlapping `region`, top to bottom, down to the first opaque one covering it all.
fn windows_under(region: &RECT) -> Vec<(isize, RECT)> {
    struct S {
        region: RECT,
        me: u32,
        out: Vec<(isize, RECT)>,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let s = &mut *(l.0 as *mut S);
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return true.into();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == s.me {
            return true.into();
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 {
            return true.into(); // click-through overlays
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return true.into();
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return true.into();
        }
        let Some(r) = rect_of(h) else { return true.into() };
        if intersect(&r, &s.region).is_none() {
            return true.into();
        }
        s.out.push((h.0 as isize, r));
        let covers = r.left <= s.region.left && r.top <= s.region.top && r.right >= s.region.right && r.bottom >= s.region.bottom;
        (!(covers && ex & WS_EX_LAYERED.0 == 0) && s.out.len() < MAX_WINDOWS).into()
    }
    let mut s = S { region: *region, me: unsafe { GetCurrentProcessId() }, out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut s as *mut S as isize));
    }
    s.out
}

impl Backdrop {
    pub fn new(notify: HWND) -> Option<Backdrop> {
        unsafe {
            if !GraphicsCaptureSession::IsSupported().ok()? {
                return None;
            }
            let (dev, ctx) = crate::gfx::d3d()?; // shared with the GPU genie, multithread-protected
            let rt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dev.cast::<IDXGIDevice>().ok()?).ok()?.cast().ok()?;
            NOTIFY.set(notify.0 as isize);
            // window opened, closed, moved, raised, minimized or restored
            for (a, b) in [(0x0003u32, 0x0003u32), (0x0016, 0x0017), (0x8001, 0x8003), (0x800B, 0x800B)] {
                SetWinEventHook(a, b, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
            }
            Some(Backdrop {
                dev, ctx, rt, caps: HashMap::new(), order: Vec::new(), notify: notify.0 as isize,
                pending: Arc::new(AtomicBool::new(false)), staging: None, region: RECT::default(),
            })
        }
    }

    /// The dock handled a WM_BACKDROP: let the next one through.
    pub fn handled(&self, layout: bool) {
        if layout {
            LAYOUT_PENDING.set(false);
        } else {
            self.pending.store(false, Ordering::Release);
        }
    }

    /// Stop every capture (dock hidden, or no glass wanted).
    pub fn clear(&mut self) {
        self.caps.clear();
        self.order.clear();
        self.region = RECT::default(); // start again on the next render
    }

    fn start(&self, h: isize) -> Option<Cap> {
        unsafe {
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ok()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(HWND(h as *mut _)).ok()?;
            let size = item.Size().ok()?;
            let fmt = DirectXPixelFormat::B8G8R8A8UIntNormalized;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&self.rt, fmt, 2, size).ok()?;
            let latest = Arc::new(Sendable(Mutex::new(Latest::default())));
            let (l2, dev, ctx, rt) = (latest.clone(), Sendable(self.dev.clone()), Sendable(self.ctx.clone()), Sendable(self.rt.clone()));
            let (pending, notify) = (self.pending.clone(), self.notify);
            let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let Some(pool) = pool.as_ref() else { return Ok(()) };
                let frame = pool.TryGetNextFrame()?;
                let size = frame.ContentSize()?;
                let src: ID3D11Texture2D = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                src.GetDesc(&mut desc);
                let (w, h) = ((size.Width.max(1) as u32).min(desc.Width), (size.Height.max(1) as u32).min(desc.Height));
                {
                    let mut l = l2.get().lock().unwrap();
                    if l.tex.is_none() || (l.w, l.h) != (w, h) {
                        let d = D3D11_TEXTURE2D_DESC {
                            Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, ..Default::default()
                        };
                        let mut t = None;
                        dev.get().CreateTexture2D(&d, None, Some(&mut t))?;
                        (l.tex, l.w, l.h) = (t, w, h);
                    }
                    if let Some(t) = &l.tex {
                        let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                        ctx.get().CopySubresourceRegion(t, 0, 0, 0, 0, &src, 0, Some(&bx));
                    }
                    l.fresh = true;
                }
                drop(frame);
                if size.Width as u32 != desc.Width || size.Height as u32 != desc.Height {
                    pool.Recreate(rt.get(), DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
                }
                if !pending.swap(true, Ordering::AcqRel) {
                    let _ = PostMessageW(HWND(notify as *mut _), WM_BACKDROP, WPARAM(0), LPARAM(0));
                }
                Ok(())
            });
            pool.FrameArrived(&handler).ok()?;
            let session = pool.CreateCaptureSession(&item).ok()?;
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture().ok()?;
            Some(Cap { session, pool, latest, rect: RECT::default(), crop: None })
        }
    }

    /// Copy `r` (texture pixels) of `tex` to the CPU.
    fn read(&mut self, tex: &ID3D11Texture2D, r: RECT) -> Option<Pixmap> {
        let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        let mut pm = Pixmap::new(w, h)?;
        unsafe {
            if self.staging.as_ref().map(|s| (s.1, s.2)) != Some((w, h)) {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..Default::default()
                };
                let mut t = None;
                self.dev.CreateTexture2D(&d, None, Some(&mut t)).ok()?;
                self.staging = Some((t?, w, h));
            }
            let st = &self.staging.as_ref()?.0;
            let bx = D3D11_BOX { left: r.left as u32, top: r.top as u32, front: 0, right: r.right as u32, bottom: r.bottom as u32, back: 1 };
            self.ctx.CopySubresourceRegion(st, 0, 0, 0, 0, tex, 0, Some(&bx));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            let src = std::slice::from_raw_parts(m.pData as *const u8, m.RowPitch as usize * h as usize);
            let row = w as usize * 4;
            let dst = pm.data_mut();
            let mut any_alpha = false;
            for y in 0..h as usize {
                let s = &src[y * m.RowPitch as usize..][..row];
                dst[y * row..][..row].copy_from_slice(s);
                any_alpha |= s.chunks_exact(4).any(|p| p[3] != 0);
            }
            self.ctx.Unmap(st, 0);
            if !any_alpha {
                // some windows hand over no alpha at all: they are opaque
                dst.chunks_exact_mut(4).for_each(|p| p[3] = 255);
            }
        }
        Some(pm)
    }

    /// Bring the captures in line with the windows behind `region` (screen pixels) and re-read the strips
    /// that changed. True when what the dock shows would look different.
    pub fn update(&mut self, region: RECT) -> bool {
        let list = windows_under(&region);
        let region_moved = region != self.region;
        self.region = region;
        let before = self.caps.len();
        self.caps.retain(|h, _| list.iter().any(|(w, _)| w == h));
        let mut changed = self.caps.len() != before;
        for (h, _) in &list {
            if !self.caps.contains_key(h) {
                if let Some(c) = self.start(*h) {
                    self.caps.insert(*h, c);
                }
            }
        }
        let order: Vec<isize> = list.iter().map(|(h, _)| *h).collect();
        changed |= order != self.order;
        self.order = order;
        for (h, rect) in &list {
            let Some(cap) = self.caps.get(h) else { continue };
            let latest = cap.latest.clone();
            let mut l = latest.get().lock().unwrap();
            let Some(tex) = l.tex.clone() else { continue };
            if !(l.fresh || region_moved || cap.rect != *rect || cap.crop.is_none()) {
                continue;
            }
            l.fresh = false;
            let (tw, th) = (l.w as i32, l.h as i32);
            drop(l);
            let cr = *rect;
            // the frame covers the window's visible frame bounds
            let crop = intersect(&cr, &region).and_then(|vis| {
                let t = RECT {
                    left: (vis.left - cr.left).clamp(0, tw), top: (vis.top - cr.top).clamp(0, th),
                    right: (vis.right - cr.left).clamp(0, tw), bottom: (vis.bottom - cr.top).clamp(0, th),
                };
                (t.right > t.left && t.bottom > t.top).then_some(())?;
                let pm = self.read(&tex, t)?;
                Some((RECT { left: cr.left + t.left, top: cr.top + t.top, right: cr.left + t.right, bottom: cr.top + t.bottom }, pm))
            });
            let cap = self.caps.get_mut(h).unwrap();
            let same = match (&cap.crop, &crop) {
                (Some((ra, a)), Some((rb, b))) => ra == rb && a.data() == b.data(),
                (None, None) => true,
                _ => false,
            };
            changed |= !same;
            cap.rect = cr;
            cap.crop = crop;
        }
        changed
    }

    pub fn region(&self) -> RECT {
        self.region
    }

    /// Draw the captured windows, bottom to top, over `out` (the wallpaper under `region` already in it).
    pub fn draw(&self, region: RECT, out: &mut Pixmap) {
        for h in self.order.iter().rev() {
            if let Some((r, pm)) = self.caps.get(h).and_then(|c| c.crop.as_ref()) {
                out.draw_pixmap(r.left - region.left, r.top - region.top, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
    }
}
