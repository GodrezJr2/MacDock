//! macOS-style popup menu (port of MenuStyle.xaml): dark rounded panel, blue highlight, check marks and
//! shortcut hints. `popup` is modal like TrackPopupMenu: it runs its own message loop and returns the
//! chosen entry. The window holds the mouse capture, so a click anywhere else closes it.

use std::cell::RefCell;

use tiny_skia::{Color, FillRule, Paint, Pixmap, PixmapPaint, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture, SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};

pub enum Entry {
    Item { text: String, gesture: String, enabled: bool, checked: bool },
    Sep,
}

impl Entry {
    pub fn item(text: impl Into<String>) -> Entry {
        Entry::Item { text: text.into(), gesture: String::new(), enabled: true, checked: false }
    }
    pub fn gesture(mut self, g: &str) -> Entry {
        if let Entry::Item { gesture, .. } = &mut self {
            *gesture = g.into();
        }
        self
    }
    pub fn checked(mut self, on: bool) -> Entry {
        if let Entry::Item { checked, .. } = &mut self {
            *checked = on;
        }
        self
    }
    pub fn enabled(mut self, on: bool) -> Entry {
        if let Entry::Item { enabled, .. } = &mut self {
            *enabled = on;
        }
        self
    }
}

/// Where the menu goes, in screen pixels.
#[derive(Clone, Copy)]
pub enum Anchor {
    /// top-left at the point (the pointer)
    Point(i32, i32),
    /// centred on x, its bottom edge at y (above a dock icon)
    Above(i32, i32),
    /// left edge at x, top at y (under a menu bar item)
    Below(i32, i32),
}

const MARGIN: f64 = 14.0; // room for the shadow around the panel
const PAD: f64 = 5.0;
const ITEM_H: f64 = 25.0;
const SEP_H: f64 = 11.0;
const MIN_W: f64 = 200.0;

struct State {
    hover: Option<usize>,
    result: Option<Option<usize>>, // Some(_) once the menu is done
    rows: Vec<(f64, f64, bool)>,    // (top, height, selectable) per entry, DIPs inside the panel's content
    dpi: f64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn row_at(st: &State, x: i32, y: i32, w: f64) -> Option<usize> {
    let (x, y) = (x as f64 / st.dpi - MARGIN - PAD, y as f64 / st.dpi - MARGIN - PAD);
    if x < 0.0 || x > w {
        return None;
    }
    st.rows.iter().position(|&(t, h, sel)| sel && y >= t && y < t + h)
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    let mut rc = windows::Win32::Foundation::RECT::default();
    let _ = GetClientRect(h, &mut rc);
    let inside = x >= 0 && y >= 0 && x < rc.right && y < rc.bottom;
    let content_w = |st: &State| rc.right as f64 / st.dpi - 2.0 * (MARGIN + PAD);
    match msg {
        WM_MOUSEMOVE => {
            with(|st| st.hover = row_at(st, x, y, content_w(st)));
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN if !inside => {
            with(|st| st.result = Some(None));
        }
        WM_LBUTTONUP | WM_RBUTTONUP if inside => {
            with(|st| {
                if let Some(i) = row_at(st, x, y, content_w(st)) {
                    st.result = Some(Some(i));
                }
            });
        }
        WM_KEYDOWN => {
            let k = wp.0 as u16;
            with(|st| {
                let sel: Vec<usize> = st.rows.iter().enumerate().filter(|r| r.1 .2).map(|r| r.0).collect();
                if sel.is_empty() {
                    if k == VK_ESCAPE.0 {
                        st.result = Some(None);
                    }
                    return;
                }
                let cur = st.hover.and_then(|h| sel.iter().position(|&i| i == h));
                if k == VK_DOWN.0 {
                    st.hover = Some(sel[cur.map(|c| (c + 1) % sel.len()).unwrap_or(0)]);
                } else if k == VK_UP.0 {
                    st.hover = Some(sel[cur.map(|c| (c + sel.len() - 1) % sel.len()).unwrap_or(sel.len() - 1)]);
                } else if k == VK_RETURN.0 {
                    if let Some(h) = st.hover {
                        st.result = Some(Some(h));
                    }
                } else if k == VK_ESCAPE.0 {
                    st.result = Some(None);
                }
            });
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            with(|st| {
                if st.result.is_none() {
                    st.result = Some(None);
                }
            });
        }
        WM_ACTIVATE if (wp.0 & 0xFFFF) == 0 => {
            with(|st| {
                if st.result.is_none() {
                    st.result = Some(None);
                }
            });
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn register() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap().into(),
            lpszClassName: w!("MacDockMenu"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
}

/// Show the menu and wait for a choice. `owner` gets focus back afterwards. Returns the entry index.
pub fn popup(owner: HWND, entries: &[Entry], at: Anchor, dpi: f64) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    register();
    let font = Font { face: gfx::UI, px: (13.0 * dpi).round() as i32, weight: 400 };
    // texts first: their widths size the panel
    let mut texts: Vec<Option<(Pixmap, Option<Pixmap>)>> = Vec::new();
    let mut rows = Vec::new();
    let (mut y, mut text_w, mut gest_w) = (0.0, 0.0f64, 0.0f64);
    for e in entries {
        match e {
            Entry::Item { text, gesture, enabled, .. } => {
                let t = gfx::text_ex(text, font, (255, 255, 255));
                let g = if gesture.is_empty() { None } else { gfx::text_ex(gesture, font, (255, 255, 255)) };
                text_w = text_w.max(t.as_ref().map(|p| p.width() as f64 / dpi).unwrap_or(0.0));
                gest_w = gest_w.max(g.as_ref().map(|p| p.width() as f64 / dpi + 28.0).unwrap_or(0.0));
                texts.push(t.map(|t| (t, g)));
                rows.push((y, ITEM_H, *enabled));
                y += ITEM_H;
            }
            Entry::Sep => {
                texts.push(None);
                rows.push((y, SEP_H, false));
                y += SEP_H;
            }
        }
    }
    let content_w = (6.0 + 18.0 + text_w + gest_w + 12.0).max(MIN_W).ceil();
    let (pw, ph) = (content_w + 2.0 * (PAD + MARGIN), y + 2.0 * (PAD + MARGIN));
    let (wpx, hpx) = ((pw * dpi).ceil() as i32, (ph * dpi).ceil() as i32);

    // place it, kept on the monitor's work area
    let m = (MARGIN * dpi).round() as i32;
    let (mut x, mut yy) = match at {
        Anchor::Point(x, y) => (x - m, y - m),
        Anchor::Above(cx, bottom) => (cx - wpx / 2, bottom - hpx + m),
        Anchor::Below(left, top) => (left - m, top - m),
    };
    unsafe {
        let mon = MonitorFromPoint(POINT { x: x + m, y: yy + m }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let r = mi.rcMonitor;
            x = x.clamp(r.left - m, (r.right - wpx + m).max(r.left - m));
            if let Anchor::Point(..) = at {
                if yy + hpx - m > r.bottom {
                    yy -= hpx - 2 * m; // open upwards from the pointer
                }
            }
            yy = yy.clamp(r.top - m, (r.bottom - hpx + m).max(r.top - m));
        }
    }

    STATE.with(|s| *s.borrow_mut() = Some(State { hover: None, result: None, rows, dpi }));
    let mut surface = Surface::new(wpx as u32, hpx as u32);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW, w!("MacDockMenu"), w!("MacDock Menu"), WS_POPUP,
            x, yy, wpx, hpx, HWND::default(), HMENU::default(), GetModuleHandleW(None).unwrap(), None,
        )
    };
    let Ok(hwnd) = hwnd else {
        STATE.with(|s| *s.borrow_mut() = None);
        return None;
    };
    let mut drawn_hover = Some(usize::MAX);
    let mut result = None;
    unsafe {
        let draw = |surface: &mut Option<Surface>, hover: Option<usize>| {
            if let Some(sf) = surface.as_mut() {
                paint(sf, entries, &texts, hover, content_w, dpi);
                sf.present(hwnd, x, yy);
            }
        };
        draw(&mut surface, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        SetCapture(hwnd);
        drawn_hover = None;
        let mut msg = MSG::default();
        loop {
            if let Some(r) = with(|st| st.result).flatten() {
                result = r;
                break;
            }
            if GetCapture() != hwnd {
                // something took the capture without telling us (a shell popup): treat as dismiss
                break;
            }
            if !GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            // keys go to the menu even if the owner still has focus
            if msg.message == WM_KEYDOWN {
                msg.hwnd = hwnd;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            let hover = with(|st| st.hover).flatten();
            if hover != drawn_hover {
                drawn_hover = hover;
                draw(&mut surface, hover);
            }
        }
        STATE.with(|s| *s.borrow_mut() = None);
        let _ = ReleaseCapture();
        let _ = DestroyWindow(hwnd);
        if !owner.0.is_null() {
            crate::ensure_no_activate(owner);
        }
    }
    let _ = drawn_hover;
    result
}

fn paint(sf: &mut Surface, entries: &[Entry], texts: &[Option<(Pixmap, Option<Pixmap>)>], hover: Option<usize>, cw: f64, dpi: f64) {
    let (pw, ph) = (sf.w as f64 / dpi - 2.0 * MARGIN, sf.h as f64 / dpi - 2.0 * MARGIN);
    let mut pm = sf.pixmap();
    pm.fill(Color::TRANSPARENT);
    let base = Transform::from_scale(dpi as f32, dpi as f32);
    let Some(panel) = gfx::rrect(MARGIN as f32, MARGIN as f32, pw as f32, ph as f32, 10.0) else { return };
    // shadow: blur 20, 5 down, 45% black
    if let Some(sp) = panel.clone().transform(base.pre_translate(0.0, 5.0)) {
        if let Some((sh, ox, oy)) = gfx::shadow(&sp, (20.0 * dpi) as f32 / 2.0, (0, 0, 0, 0x73)) {
            pm.draw_pixmap(ox.round() as i32, oy.round() as i32, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
    }
    let mut paint = Paint::default();
    paint.anti_alias = true;
    // knock the shadow out under the panel so the translucent fill shows the desktop, not the shadow
    paint.blend_mode = tiny_skia::BlendMode::Clear;
    pm.fill_path(&panel, &paint, FillRule::Winding, base, None);
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
    paint.set_color(Color::from_rgba8(0x2E, 0x2A, 0x2A, 0xEE));
    pm.fill_path(&panel, &paint, FillRule::Winding, base, None);
    paint.set_color(Color::from_rgba8(255, 255, 255, 0x4D));
    pm.stroke_path(&panel, &paint, &Stroke { width: 0.8, ..Default::default() }, base, None);

    let (left, top) = (MARGIN + PAD, MARGIN + PAD);
    let mut y = top;
    let check = gfx::text_ex("✓", Font { face: gfx::UI, px: (12.0 * dpi).round() as i32, weight: 400 }, (255, 255, 255));
    for (i, e) in entries.iter().enumerate() {
        match e {
            Entry::Sep => {
                if let Some(r) = tiny_skia::Rect::from_xywh((left + 10.0) as f32, (y + 5.0) as f32, (cw - 20.0) as f32, 1.0) {
                    paint.set_color(Color::from_rgba8(255, 255, 255, 0x2E));
                    pm.fill_rect(r, &paint, base, None);
                }
                y += SEP_H;
            }
            Entry::Item { enabled, checked, .. } => {
                let op = if *enabled { 1.0 } else { 0.4 };
                if hover == Some(i) && *enabled {
                    if let Some(hl) = gfx::rrect(left as f32, y as f32, cw as f32, ITEM_H as f32, 5.0) {
                        paint.set_color(Color::from_rgba8(0xFF, 0x84, 0x0A, 0xFF));
                        pm.fill_path(&hl, &paint, FillRule::Winding, base, None);
                    }
                }
                if let Some((t, g)) = &texts[i] {
                    let ty = ((y + (ITEM_H - t.height() as f64 / dpi) / 2.0) * dpi).round() as f32;
                    let pp = PixmapPaint { opacity: op, ..Default::default() };
                    pm.draw_pixmap(0, 0, t.as_ref(), &pp, Transform::from_translate(((left + 24.0) * dpi).round() as f32, ty), None);
                    if let Some(g) = g {
                        let gx = ((left + cw - 12.0) * dpi).round() as f32 - g.width() as f32;
                        let gp = PixmapPaint { opacity: op * 0.5, ..Default::default() };
                        pm.draw_pixmap(0, 0, g.as_ref(), &gp, Transform::from_translate(gx, ty), None);
                    }
                    if *checked {
                        if let Some(c) = &check {
                            let cx = ((left + 6.0 + 9.0) * dpi).round() as f32 - c.width() as f32 / 2.0;
                            let cy = ((y + ITEM_H / 2.0) * dpi).round() as f32 - c.height() as f32 / 2.0;
                            pm.draw_pixmap(0, 0, c.as_ref(), &pp, Transform::from_translate(cx, cy), None);
                        }
                    }
                }
                y += ITEM_H;
            }
        }
    }
}
