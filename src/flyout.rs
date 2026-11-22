//! Menu bar extras (port of Flyout.cs): a dark rounded panel that drops from a menu bar item, takes focus while
//! open (so the Wi-Fi password field gets keys) and closes on Esc or when anything else is clicked.
//!
//! The panels draw themselves immediate-mode: every redraw a `Content` paints into a `Ui` and names the
//! clickable spots it drew, so hit-testing always matches what is on screen.

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};
use crate::{push, Ev};

pub const MARGIN: f32 = 13.0; // shadow room around the panel
const PANEL_FILL: (u8, u8, u8, u8) = (0x1C, 0x1C, 0x1F, 0xF2);
pub const ACCENT: (u8, u8, u8) = (0x0A, 0x84, 0xFF);
pub const ACCENT4: (u8, u8, u8, u8) = (0x0A, 0x84, 0xFF, 0xFF);
const WM_MOUSELEAVE: u32 = 0x02A3;
pub const WM_FLY_DATA: u32 = WM_APP + 30;
const TIMER_TICK: usize = 1;
const TIMER_ANIM: usize = 2;
/// how long a full-screen panel takes to zoom and fade in
const STAGE_IN_MS: f32 = 240.0;
/// a panel under the menu bar: fade and grow in, fade out
const PANEL_IN_MS: f32 = 170.0;
const PANEL_OUT_MS: u64 = 140;
/// a glass window (the Apps window): springs up from 90% as it fades in
const GLASS_IN_MS: f32 = 420.0;
/// the glass window's corners
const GLASS_R: f32 = 26.0;

/// false while Reduce motion is on: panels appear and go at once.
static MOTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_motion(on: bool) {
    MOTION.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn motion() -> bool {
    MOTION.load(std::sync::atomic::Ordering::Relaxed)
}

/// The blurred, dimmed wallpaper behind full-screen panels, kept for the next open (screen px).
static BACKDROP: std::sync::Mutex<Option<std::sync::Arc<Pixmap>>> = std::sync::Mutex::new(None);

/// The wallpaper changed: build the full-screen backdrop again next time it is needed.
pub fn forget_backdrop() {
    *BACKDROP.lock().unwrap() = None;
}

/// Build the backdrop on a worker (decoding and blurring the wallpaper takes ~150 ms) so the Launchpad's first frame doesn't wait.
pub fn warm_backdrop() {
    std::thread::spawn(|| unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        if sw > 0 && sh > 0 {
            let _ = stage_backdrop(sw as u32, sh as u32);
        }
    });
}

/// The wallpaper at `w` x `h`, heavily blurred and dimmed like the macOS Launchpad background.
fn stage_backdrop(w: u32, h: u32) -> std::sync::Arc<Pixmap> {
    let mut cache = BACKDROP.lock().unwrap();
    if let Some(b) = cache.as_ref().filter(|b| b.width() == w && b.height() == h) {
        return b.clone();
    }
    let mut pm = Pixmap::new(w, h).unwrap();
    pm.fill(Color::from_rgba8(0x1E, 0x22, 0x30, 0xFF));
    // blur a small copy and stretch it: smooth, and cheap enough to do on open
    let (bw, bh) = ((w / 8).max(8), (h / 8).max(8));
    // blurred with a margin that is cut off after, so the edges don't fade into the dark outside
    const M: u32 = 8;
    if let Some(mut small) = gfx::wallpaper(bw + 2 * M, bh + 2 * M) {
        gfx::blur(&mut small, 9.0);
        let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
        let (kx, ky) = (w as f32 / bw as f32, h as f32 / bh as f32);
        let tf = Transform::from_row(kx, 0.0, 0.0, ky, -(M as f32) * kx, -(M as f32) * ky);
        pm.draw_pixmap(0, 0, small.as_ref(), &paint, tf, None);
    }
    let mut veil = Paint::default();
    veil.set_color(Color::from_rgba8(0, 0, 0, 0x5A));
    pm.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap(), &veil, Transform::identity(), None);
    let b = std::sync::Arc::new(pm);
    *cache = Some(b.clone());
    b
}

/// Straight (r, g, b, a) to the BGRA layout our surfaces use.
pub fn rgba(c: (u8, u8, u8, u8)) -> Color {
    Color::from_rgba8(c.2, c.1, c.0, c.3)
}

pub fn white(a: u8) -> (u8, u8, u8, u8) {
    (255, 255, 255, a)
}

pub enum FlyEv {
    Move(i32, i32),
    Leave,
    Down(i32, i32),
    Up(i32, i32),
    RightUp(i32, i32),
    Wheel(i32),
    /// horizontal wheel: a sideways two-finger swipe on a touchpad (positive = towards the right)
    HWheel(i32),
    Char(char),
    Key(u32),
    Deactivated,
    Tick,
    Data,
}

/// What a panel asks for after an input.
pub enum Cmd {
    None,
    Close,
    /// close and open this target (ms-settings: page, app)
    Launch(String),
    /// Control Center's Wi-Fi row: swap to the Wi-Fi flyout
    OpenWifi,
    /// close and keep this app in the dock
    Pin(String),
    /// window panels only (traffic lights): minimize, and fill the screen / back
    Minimize,
    Zoom,
    /// close and open MacDock's Settings
    Settings,
}

thread_local! {
    /// Images already scaled to a size, keyed by the source's address, a sample of its pixels and the size:
    /// a full-screen Launchpad draws 35 icons a frame and scaling them each time was most of a frame.
    static SCALED: std::cell::RefCell<std::collections::HashMap<(usize, u64, u32, u32), std::rc::Rc<Pixmap>>> = Default::default();
}

fn scaled(img: &Pixmap, dw: u32, dh: u32) -> Option<std::rc::Rc<Pixmap>> {
    let d = img.data();
    // the address alone could be reused by another image after this one is dropped
    let mut sample = (img.width() as u64) << 32 | img.height() as u64;
    let step = (d.len() / 4 / 32).max(1) * 4;
    for i in (0..d.len().saturating_sub(3)).step_by(step) {
        sample = sample.rotate_left(7) ^ u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as u64;
    }
    let key = (d.as_ptr() as usize, sample, dw, dh);
    if let Some(p) = SCALED.with(|m| m.borrow().get(&key).cloned()) {
        return Some(p);
    }
    let mut small = Pixmap::new(dw, dh)?;
    let tf = Transform::from_scale(dw as f32 / img.width() as f32, dh as f32 / img.height() as f32);
    let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    small.draw_pixmap(0, 0, img.as_ref(), &paint, tf, None);
    let small = std::rc::Rc::new(small);
    SCALED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 300 {
            m.clear();
        }
        m.insert(key, small.clone());
    });
    Some(small)
}

/// Drawing context, in DIPs relative to the panel's content box.
pub struct Ui<'a> {
    pm: &'a mut Pixmap,
    s: f32,
    ox: f32,
    oy: f32,
    mouse: Option<(f32, f32)>,
    pub pressed: Option<u32>,
    spots: &'a mut Vec<Spot>,
    pub w: f32, // content width
    /// scale for text: the same as `s`, except while the Launchpad zooms in (text keeps its size, only moves)
    fs: f32,
    /// nothing is moving: scaled images may be remembered and reused
    still: bool,
}

#[derive(Clone, Copy)]
pub struct Spot {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Ui<'_> {
    fn tf(&self) -> Transform {
        Transform::from_row(self.s, 0.0, 0.0, self.s, self.ox * self.s, self.oy * self.s)
    }

    /// Draw `f` into a layer clipped to (`x`, `y`, `w`, `h`), then put it down at `opacity`, scaled by `zoom`
    /// about its middle, its edges fading out over `fade` DIPs (top, bottom, left, right: a scroll view's soft
    /// edges). Spots outside the rect are dropped, and the mouse only counts inside it.
    #[allow(clippy::too_many_arguments)]
    pub fn layer(&mut self, x: f32, y: f32, w: f32, h: f32, opacity: f32, zoom: f32, fade: [f32; 4], f: impl FnOnce(&mut Ui)) {
        let s = self.s;
        let (dx0, dy0) = (((self.ox + x) * s).floor(), ((self.oy + y) * s).floor());
        let (dw, dh) = ((w * s).ceil() as u32 + 1, (h * s).ceil() as u32 + 1);
        let Some(mut pm) = Pixmap::new(dw, dh) else { return };
        let n0 = self.spots.len();
        let inside = |p: &(f32, f32)| p.0 >= x && p.0 < x + w && p.1 >= y && p.1 < y + h;
        {
            let mut ui = Ui {
                pm: &mut pm, s, ox: self.ox - dx0 / s, oy: self.oy - dy0 / s, mouse: self.mouse.filter(inside),
                pressed: self.pressed, spots: &mut *self.spots, w: self.w, fs: self.fs, still: self.still,
            };
            f(&mut ui);
        }
        let added: Vec<Spot> = self.spots.drain(n0..).collect();
        for sp in added {
            let (l, t, r, b) = (sp.x.max(x), sp.y.max(y), (sp.x + sp.w).min(x + w), (sp.y + sp.h).min(y + h));
            if r > l && b > t {
                self.spots.push(Spot { id: sp.id, x: l, y: t, w: r - l, h: b - t });
            }
        }
        if opacity <= 0.0 {
            return;
        }
        // soft edges: scale whole rows and columns down (the data is premultiplied, so all four bytes)
        let smooth = |v: f32| v * v * (3.0 - 2.0 * v);
        let (fw, fh) = (dw as usize, dh as usize);
        let data = pm.data_mut();
        for (k, &f) in fade.iter().enumerate() {
            let n = (f * s).ceil() as usize;
            if n == 0 {
                continue;
            }
            for i in 0..n.min(if k < 2 { fh } else { fw }) {
                let a = smooth((i as f32 + 0.5) / n as f32);
                let m = (a * 256.0) as u32;
                if k < 2 {
                    let row = if k == 0 { i } else { fh - 1 - i };
                    for b in &mut data[row * fw * 4..(row + 1) * fw * 4] {
                        *b = ((*b as u32 * m) >> 8) as u8;
                    }
                } else {
                    let col = if k == 2 { i } else { fw - 1 - i };
                    for row in 0..fh {
                        let o = (row * fw + col) * 4;
                        for b in &mut data[o..o + 4] {
                            *b = ((*b as u32 * m) >> 8) as u8;
                        }
                    }
                }
            }
        }
        let quality = if (zoom - 1.0).abs() > 0.0005 { tiny_skia::FilterQuality::Bilinear } else { tiny_skia::FilterQuality::Nearest };
        let paint = PixmapPaint { opacity: opacity.min(1.0), quality, ..Default::default() };
        let (cx, cy) = (dw as f32 / 2.0, dh as f32 / 2.0);
        let tf = Transform::from_row(zoom, 0.0, 0.0, zoom, dx0 + cx - cx * zoom, dy0 + cy - cy * zoom);
        self.pm.draw_pixmap(0, 0, pm.as_ref(), &paint, tf, None);
    }

    /// Name a clickable spot; returns whether the mouse is over it.
    pub fn hit(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32) -> bool {
        self.spots.push(Spot { id, x, y, w, h });
        self.mouse.is_some_and(|(mx, my)| mx >= x && mx < x + w && my >= y && my < y + h)
    }

    pub fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: (u8, u8, u8, u8)) {
        if c.3 == 0 || w <= 0.0 || h <= 0.0 {
            return;
        }
        if let Some(p) = gfx::rrect(x, y, w, h, r) {
            let mut paint = Paint::default();
            paint.set_color(rgba(c));
            paint.anti_alias = true;
            let tf = self.tf();
            self.pm.fill_path(&p, &paint, FillRule::Winding, tf, None);
        }
    }

    pub fn outline(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: (u8, u8, u8, u8)) {
        if let Some(p) = gfx::rrect(x + width / 2.0, y + width / 2.0, w - width, h - width, r) {
            let mut paint = Paint::default();
            paint.set_color(rgba(c));
            paint.anti_alias = true;
            let tf = self.tf();
            self.pm.stroke_path(&p, &paint, &Stroke { width, ..Default::default() }, tf, None);
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: (u8, u8, u8, u8)) {
        if let Some(p) = PathBuilder::from_circle(cx, cy, r) {
            let mut paint = Paint::default();
            paint.set_color(rgba(c));
            paint.anti_alias = true;
            let tf = self.tf();
            self.pm.fill_path(&p, &paint, FillRule::Winding, tf, None);
        }
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: (u8, u8, u8, u8)) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(p) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(rgba(c));
            paint.anti_alias = true;
            let tf = self.tf();
            self.pm.stroke_path(&p, &paint, &Stroke { width, line_cap: LineCap::Round, ..Default::default() }, tf, None);
        }
    }

    fn font(&self, size: f32, weight: i32, icons: bool) -> Font {
        Font { face: if icons { gfx::ICONS } else { gfx::UI }, px: (size * self.fs).round() as i32, weight }
    }

    pub fn text_w(&self, s: &str, size: f32, weight: i32) -> f32 {
        gfx::measure(s, self.font(size, weight, false)).0 as f32 / self.fs
    }

    /// Text with its line box's top at y; returns its width. `max_w` > 0 cuts it with an ellipsis.
    pub fn text(&mut self, x: f32, y: f32, s: &str, size: f32, weight: i32, c: (u8, u8, u8, u8), max_w: f32) -> f32 {
        let f = self.font(size, weight, false);
        let mut s = s.to_string();
        if max_w > 0.0 && gfx::measure(&s, f).0 as f32 / self.fs > max_w {
            while !s.is_empty() && gfx::measure(&format!("{s}…"), f).0 as f32 / self.fs > max_w {
                s.pop();
            }
            s.push('…');
        }
        self.draw_text(x, y, &s, f, c)
    }

    /// Text vertically centred on cy.
    pub fn text_mid(&mut self, x: f32, cy: f32, s: &str, size: f32, weight: i32, c: (u8, u8, u8, u8), max_w: f32) -> f32 {
        let h = gfx::measure("Ag", self.font(size, weight, false)).1 as f32 / self.fs;
        self.text(x, cy - h / 2.0, s, size, weight, c, max_w)
    }

    /// A Segoe Fluent Icons glyph centred on (cx, cy).
    pub fn glyph(&mut self, cx: f32, cy: f32, g: char, size: f32, c: (u8, u8, u8, u8)) {
        let f = self.font(size, 400, true);
        let s = g.to_string();
        let (w, h) = gfx::measure(&s, f);
        self.draw_text(cx - w as f32 / self.fs / 2.0, cy - h as f32 / self.fs / 2.0, &s, f, c);
    }

    fn draw_text(&mut self, x: f32, y: f32, s: &str, f: Font, c: (u8, u8, u8, u8)) -> f32 {
        let Some(t) = gfx::text_ex(s, f, (c.0, c.1, c.2)) else { return 0.0 };
        let w = (t.width() as f32 - 2.0) / self.fs;
        let paint = PixmapPaint { opacity: c.3 as f32 / 255.0, ..Default::default() };
        let px = ((self.ox + x) * self.s).round() as i32;
        let py = ((self.oy + y) * self.s).round() as i32;
        self.pm.draw_pixmap(px, py, t.as_ref(), &paint, Transform::identity(), None);
        w
    }

    /// An image (premultiplied BGRA) scaled into the box.
    pub fn image(&mut self, img: &Pixmap, x: f32, y: f32, w: f32, h: f32) {
        let (dw, dh) = ((w * self.s).round() as u32, (h * self.s).round() as u32);
        if self.still && dw > 0 && dh > 0 && dw <= 512 && dh <= 512 {
            if let Some(small) = scaled(img, dw, dh) {
                let px = ((self.ox + x) * self.s).round() as i32;
                let py = ((self.oy + y) * self.s).round() as i32;
                self.pm.draw_pixmap(px, py, (*small).as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                return;
            }
        }
        let sx = w * self.s / img.width() as f32;
        let sy = h * self.s / img.height() as f32;
        let tf = Transform::from_row(sx, 0.0, 0.0, sy, (self.ox + x) * self.s, (self.oy + y) * self.s);
        let quality = if self.still { tiny_skia::FilterQuality::Bicubic } else { tiny_skia::FilterQuality::Bilinear };
        let paint = PixmapPaint { quality, ..Default::default() };
        self.pm.draw_pixmap(0, 0, img.as_ref(), &paint, tf, None);
    }

    /// A row that lights up under the mouse (macOS menu bar extra item); returns whether it is hovered.
    pub fn row(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32) -> bool {
        let hov = self.hit(id, x, y, w, h);
        if hov {
            self.fill(x, y, w, h, 8.0, white(0x1C));
        }
        hov
    }

    /// An image drawn into (x, y, w, h), clipped to the rounded box `clip` (x, y, w, h) with radius r.
    pub fn image_rounded(&mut self, img: &Pixmap, x: f32, y: f32, w: f32, h: f32, clip: (f32, f32, f32, f32), r: f32) {
        let s = self.s;
        let Some(mut mask) = tiny_skia::Mask::new(self.pm.width(), self.pm.height()) else { return };
        if let Some(p) = gfx::rrect((self.ox + clip.0) * s, (self.oy + clip.1) * s, clip.2 * s, clip.3 * s, r * s) {
            mask.fill_path(&p, FillRule::Winding, true, Transform::identity());
        }
        let sx = w * s / img.width() as f32;
        let sy = h * s / img.height() as f32;
        let tf = Transform::from_row(sx, 0.0, 0.0, sy, (self.ox + x) * s, (self.oy + y) * s);
        let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
        self.pm.draw_pixmap(0, 0, img.as_ref(), &paint, tf, Some(&mask));
    }

    pub fn divider(&mut self, y: f32) {
        let w = self.w;
        self.fill(0.0, y, w, 1.0, 0.0, white(0x1A));
    }

    /// macOS switch, 36x20 at (x, y).
    pub fn switch(&mut self, id: u32, x: f32, y: f32, on: bool) {
        self.hit(id, x, y, 36.0, 20.0);
        let track = if on { (ACCENT.0, ACCENT.1, ACCENT.2, 255) } else { white(0x40) };
        self.fill(x, y, 36.0, 20.0, 10.0, track);
        let kx = if on { x + 36.0 - 18.0 } else { x + 2.0 };
        self.circle(kx + 8.0, y + 11.0, 8.0, (0, 0, 0, 0x30));
        self.circle(kx + 8.0, y + 10.0, 8.0, white(255));
    }

    /// Wi-Fi bars as a dot and two arcs, dimmed past `bars`.
    pub fn battery(&mut self, x: f32, y: f32, w: f32, h: f32, pct: u8, plugged: bool, charging: bool) {
        let s = self.s;
        crate::menubar::draw_battery(self.pm, (self.ox + x) * s, (self.oy + y) * s, w * s, h * s, pct, plugged, charging);
    }

    /// Height of one text line at this size.
    pub fn line_h(&self, size: f32, weight: i32) -> f32 {
        gfx::measure("Ag", self.font(size, weight, false)).1 as f32 / self.fs
    }

    /// Word-wrapped text; returns the height used.
    pub fn text_wrap(&mut self, x: f32, y: f32, s: &str, size: f32, weight: i32, c: (u8, u8, u8, u8), max_w: f32) -> f32 {
        let lh = self.line_h(size, weight);
        let mut lines: Vec<String> = Vec::new();
        let mut cur = String::new();
        for word in s.split_whitespace() {
            let next = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if !cur.is_empty() && self.text_w(&next, size, weight) > max_w {
                lines.push(std::mem::replace(&mut cur, word.to_string()));
            } else {
                cur = next;
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        for (i, l) in lines.iter().enumerate() {
            self.text(x, y + i as f32 * lh, l, size, weight, c, 0.0);
        }
        lines.len() as f32 * lh
    }

    pub fn wifi(&mut self, x: f32, y: f32, w: f32, h: f32, bars: u8, off: bool) {
        let s = self.s;
        crate::menubar::draw_wifi(self.pm, (self.ox + x) * s, (self.oy + y) * s, w * s, h * s, bars, off);
    }
}

/// One extra's body.
pub trait Content {
    /// Window width in DIPs, the 13 px shadow margin on both sides included (as Flyout.cs).
    fn width(&self) -> f32;
    /// Panel padding (left/right, top/bottom).
    fn padding(&self) -> (f32, f32) {
        (14.0, 14.0)
    }
    /// Paint everything; return the content height.
    fn draw(&mut self, ui: &mut Ui) -> f32;
    fn click(&mut self, _id: u32, _fx: f32) -> Cmd {
        Cmd::None
    }
    fn right_click(&mut self, _id: u32) -> Cmd {
        Cmd::None
    }
    /// A press started on this spot (sliders follow the mouse from here).
    fn press(&mut self, _id: u32, _fx: f32) {}
    fn drag(&mut self, _id: u32, _fx: f32) {}
    fn release(&mut self, _id: u32, _fx: f32) {}
    /// Mouse over a spot changed (the tray forwards it to the app).
    fn hover(&mut self, _id: Option<u32>) {}
    fn wheel(&mut self, _delta: i32) {}
    fn hwheel(&mut self, _delta: i32) {}
    fn char(&mut self, _c: char) -> Cmd {
        Cmd::None
    }
    fn key(&mut self, _vk: u32) -> Cmd {
        Cmd::None
    }
    /// Timer period (0 = none) and what to do on it.
    fn period_ms(&self) -> u32 {
        0
    }
    fn tick(&mut self) {}
    /// A worker posted WM_FLY_DATA: pick up its results.
    fn data(&mut self) {}
    /// Screen rects of spots, for apps that anchor their popups to a tray icon.
    fn placed(&mut self, _spots: &[Spot], _origin: (i32, i32), _dpi: f32) {}
    fn closing(&mut self) {}
    /// The window exists (workers post WM_FLY_DATA to it).
    fn opened(&mut self, _hwnd: isize) {}
    /// Window panels: a press here (not on a spot) drags the window, like a title bar.
    fn drag_area(&self, _x: f32, _y: f32) -> bool {
        false
    }
    /// Full-screen panels (Launchpad) say yes and get the screen size in DIPs; they draw over the blurred
    /// wallpaper with no panel, and a click on nothing closes them.
    fn stage(&mut self, _w: f32, _h: f32) -> bool {
        false
    }
    /// Something is moving (a page sliding): keep ticking at the frame rate.
    fn animating(&self) -> bool {
        false
    }
    /// false when Reduce motion is on: no zoom on open.
    fn motion(&self) -> bool {
        true
    }
    /// A floating Liquid Glass window (the Apps window) of this fixed content height in DIPs: centred on the
    /// screen over a lens of what is behind it, springing in when it opens.
    fn glass(&self) -> Option<f32> {
        None
    }
    /// A glass window that grows downward as it fills (Spotlight): its top at this fraction of the screen
    /// height, and the tallest it gets. The window is that tall from the start; the glass follows the content.
    fn glass_top(&self) -> Option<(f32, f32)> {
        None
    }
    /// Esc: close, unless the panel has something to undo first (a search).
    fn escape(&mut self) -> Cmd {
        Cmd::Close
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    match msg {
        WM_MOUSEMOVE => push(Ev::Fly(h.0 as isize, FlyEv::Move(x, y))),
        WM_MOUSELEAVE => push(Ev::Fly(h.0 as isize, FlyEv::Leave)),
        WM_LBUTTONDOWN => {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(h);
            push(Ev::Fly(h.0 as isize, FlyEv::Down(x, y)));
        }
        WM_LBUTTONUP => {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            push(Ev::Fly(h.0 as isize, FlyEv::Up(x, y)));
        }
        WM_RBUTTONUP => push(Ev::Fly(h.0 as isize, FlyEv::RightUp(x, y))),
        WM_MOUSEWHEEL => push(Ev::Fly(h.0 as isize, FlyEv::Wheel(((wp.0 >> 16) & 0xFFFF) as i16 as i32))),
        WM_MOUSEHWHEEL => {
            push(Ev::Fly(h.0 as isize, FlyEv::HWheel(((wp.0 >> 16) & 0xFFFF) as i16 as i32)));
            return LRESULT(1); // handled, so the driver doesn't fall back to other messages
        }
        WM_CHAR => {
            if let Some(c) = char::from_u32(wp.0 as u32) {
                push(Ev::Fly(h.0 as isize, FlyEv::Char(c)));
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => push(Ev::Fly(h.0 as isize, FlyEv::Key(wp.0 as u32))),
        WM_ACTIVATE if (wp.0 & 0xFFFF) == 0 => push(Ev::Fly(h.0 as isize, FlyEv::Deactivated)),
        WM_TIMER => push(Ev::Fly(h.0 as isize, FlyEv::Tick)),
        // back from minimized (Alt+Tab, the taskbar): paint the layered surface again
        WM_SIZE if wp.0 as u32 == SIZE_RESTORED => push(Ev::Fly(h.0 as isize, FlyEv::Tick)),
        WM_FLY_DATA => push(Ev::Fly(h.0 as isize, FlyEv::Data)),
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn create_window(window: bool) -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockFlyout"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        // a window panel (Settings) is a normal app window: in Alt+Tab, minimizable, not always on top
        let (ex, title) = if window {
            (WS_EX_LAYERED | WS_EX_APPWINDOW, w!("MacDock Settings"))
        } else {
            (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("MacDock Flyout"))
        };
        CreateWindowExW(ex, w!("MacDockFlyout"), title, WS_POPUP | WS_MINIMIZEBOX, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None)
        .unwrap()
    }
}

pub struct Flyout {
    pub hwnd: HWND,
    pub kind: crate::menubar::Extra,
    content: Box<dyn Content>,
    dpi: f32,
    right_px: i32, // the anchor's right edge
    top_px: i32,
    screen_w_px: i32,
    pos: (i32, i32),
    mouse: Option<(f32, f32)>, // content coordinates
    hot: Option<u32>,
    press: Option<u32>,
    spots: Vec<Spot>,
    surface: Option<Surface>,
    /// the Launchpad's frame: the backdrop with the content drawn straight over it
    stage_pm: Option<Pixmap>,
    /// the glass window's body (shadow, lens of the screen behind it, tint), built once when it opens
    glass: Option<Pixmap>,
    /// the blurred screen behind a growing glass window, and the panel height its glass was cut for
    under: Option<Pixmap>,
    glass_h: f32,
    tracking: bool,
    pub stay_open: bool,
    /// a window panel: stays open, can be dragged, minimized and zoomed to fill the screen
    window: bool,
    /// where the user dragged the window to (screen px of its top-left), if anywhere
    moved: Option<(i32, i32)>,
    /// filling the screen (the green light)
    full: bool,
    /// scale and content origin (DIPs) of the last render, for mapping the mouse
    view: (f32, f32, f32),
    /// full screen over the blurred wallpaper (Launchpad), and when it opened (for the zoom in)
    stage: bool,
    born: std::time::Instant,
    anim_timer: bool,
}

impl Flyout {
    /// Open under the menu bar item whose right edge is `right_px`; `top_px` is the bar's bottom.
    pub fn open(kind: crate::menubar::Extra, content: Box<dyn Content>, right_px: i32, top_px: i32, dpi: f64) -> Flyout {
        let window = kind.is_window();
        let hwnd = create_window(window);
        let screen_w_px = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let mut f = Flyout {
            hwnd, kind, content, dpi: dpi as f32, right_px, top_px, screen_w_px, pos: (0, 0), mouse: None, hot: None,
            press: None, spots: Vec::new(), surface: None, stage_pm: None, glass: None, under: None, glass_h: 0.0, tracking: false, stay_open: window, window, moved: None,
            full: false, view: (dpi as f32, 0.0, 0.0), stage: false, born: std::time::Instant::now(), anim_timer: false,
        };
        f.content.opened(hwnd.0 as isize);
        // the animation starts once it can be seen: setting up the content can take a while (Control Center)
        f.born = std::time::Instant::now();
        f.render();
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(hwnd);
            let p = f.content.period_ms();
            if p > 0 {
                SetTimer(hwnd, TIMER_TICK, p, None);
            }
        }
        f
    }

    pub fn close(&mut self) {
        self.content.closing();
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_ANIM);
            if !motion() || IsIconic(self.hwnd).as_bool() {
                let _ = DestroyWindow(self.hwnd);
                return;
            }
            // fade out like a macOS menu: clicks go through it meanwhile, and its own thread destroys it after
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32);
        }
        let h = self.hwnd.0 as isize;
        std::thread::spawn(move || {
            let hwnd = HWND(h as *mut _);
            let steps = 8u64;
            for i in 1..=steps {
                std::thread::sleep(std::time::Duration::from_millis(PANEL_OUT_MS / steps));
                let a = 1.0 - i as f32 / steps as f32;
                gfx::set_alpha(hwnd, (a * a * 255.0) as u8);
            }
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn to_content(&self, x: i32, y: i32) -> (f32, f32) {
        let (s, ox, oy) = self.view;
        (x as f32 / s - ox, y as f32 / s - oy)
    }

    /// Bring a minimized window panel back and to the front.
    pub fn raise(&mut self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.render();
    }

    fn set_full(&mut self, on: bool) {
        self.full = on;
        unsafe {
            // full screen covers the dock and menu bar like a macOS full-screen app; a window sits among apps
            let z = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(self.hwnd, z, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
    }

    /// Start a title-bar drag: Windows moves the window until the button comes up.
    fn drag_window(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            SendMessageW(self.hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            let mut r = windows::Win32::Foundation::RECT::default();
            if GetWindowRect(self.hwnd, &mut r).is_ok() {
                self.moved = Some((r.left, r.top));
            }
        }
    }

    fn spot_at(&self, p: (f32, f32)) -> Option<Spot> {
        // last drawn wins: rows first, buttons inside them after
        self.spots.iter().rev().find(|s| p.0 >= s.x && p.0 < s.x + s.w && p.1 >= s.y && p.1 < s.y + s.h).copied()
    }

    fn frac(s: &Spot, p: (f32, f32)) -> f32 {
        ((p.0 - s.x) / s.w.max(1.0)).clamp(0.0, 1.0)
    }

    /// Returns true when the flyout should close (and the Cmd to run after).
    pub fn handle(&mut self, e: FlyEv) -> Option<Cmd> {
        let mut cmd = Cmd::None;
        match e {
            FlyEv::Move(x, y) => {
                if !self.tracking {
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut t);
                    }
                    self.tracking = true;
                }
                let p = self.to_content(x, y);
                self.mouse = Some(p);
                if let Some(id) = self.press {
                    if let Some(s) = self.spots.iter().find(|s| s.id == id).copied() {
                        self.content.drag(id, Self::frac(&s, p));
                    }
                }
                let hot = self.spot_at(p).map(|s| s.id);
                if hot != self.hot {
                    self.hot = hot;
                    self.content.hover(hot);
                }
            }
            FlyEv::Leave => {
                self.tracking = false;
                self.mouse = None;
                if self.hot.take().is_some() {
                    self.content.hover(None);
                }
            }
            FlyEv::Down(x, y) => {
                let p = self.to_content(x, y);
                if let Some(s) = self.spot_at(p) {
                    self.press = Some(s.id);
                    self.content.press(s.id, Self::frac(&s, p));
                } else if self.window && !self.full && self.content.drag_area(p.0, p.1) {
                    self.drag_window();
                }
            }
            FlyEv::Up(x, y) => {
                let p = self.to_content(x, y);
                if self.stage && self.press.is_none() && self.spot_at(p).is_none() {
                    cmd = Cmd::Close; // a click on the background, like macOS Launchpad
                }
                if let Some(id) = self.press.take() {
                    if let Some(s) = self.spots.iter().find(|s| s.id == id).copied() {
                        self.content.release(id, Self::frac(&s, p));
                    }
                    if self.spot_at(p).is_some_and(|s| s.id == id) {
                        let s = self.spot_at(p).unwrap();
                        cmd = self.content.click(id, Self::frac(&s, p));
                    }
                }
            }
            FlyEv::RightUp(x, y) => {
                if let Some(s) = self.spot_at(self.to_content(x, y)) {
                    cmd = self.content.right_click(s.id);
                }
            }
            FlyEv::Wheel(d) => self.content.wheel(d),
            FlyEv::HWheel(d) => self.content.hwheel(d),
            FlyEv::Char(c) => cmd = self.content.char(c),
            FlyEv::Key(0x1B) if self.full => self.set_full(false),
            FlyEv::Key(0x1B) => cmd = self.content.escape(),
            FlyEv::Key(vk) => cmd = self.content.key(vk),
            FlyEv::Deactivated => {
                if !self.stay_open {
                    cmd = Cmd::Close;
                }
            }
            FlyEv::Tick => self.content.tick(),
            FlyEv::Data => self.content.data(),
        }
        match cmd {
            Cmd::None => {}
            Cmd::Minimize => unsafe {
                self.set_full(false);
                let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
            },
            Cmd::Zoom => {
                let on = !self.full;
                self.set_full(on);
            }
            cmd => return Some(cmd),
        }
        self.render();
        None
    }

    pub fn render(&mut self) {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        self.stage = self.content.stage(sw as f32 / self.dpi, sh as f32 / self.dpi);
        if self.stage {
            self.render_stage(sw, sh);
        } else {
            self.render_panel();
        }
        self.keep_ticking();
    }

    /// Run a frame-rate timer while the open zoom or the content is moving.
    fn keep_ticking(&mut self) {
        let dur = if self.stage { STAGE_IN_MS } else if self.content.glass().is_some() { GLASS_IN_MS } else { PANEL_IN_MS };
        let opening = motion() && self.content.motion() && !self.full && self.born.elapsed().as_secs_f32() * 1000.0 < dur + 40.0; // one frame past the end, so the last one is whole
        let want = opening || self.content.animating();
        if want != self.anim_timer {
            self.anim_timer = want;
            unsafe {
                if want {
                    SetTimer(self.hwnd, TIMER_ANIM, 15, None);
                } else {
                    let _ = KillTimer(self.hwnd, TIMER_ANIM);
                }
            }
        }
    }

    /// Full screen over the blurred wallpaper, no panel; zooms down and fades in when it opens.
    /// The content is drawn straight over a copy of the backdrop at the zoom's scale (no second full-screen
    /// layer to transform), and the fade is the window's own opacity, so a frame costs about a copy and a draw.
    fn render_stage(&mut self, sw: i32, sh: i32) {
        let (pw, ph) = (sw as u32, sh as u32);
        let s = self.dpi;
        let t = if motion() && self.content.motion() { (self.born.elapsed().as_secs_f32() * 1000.0 / STAGE_IN_MS).min(1.0) } else { 1.0 };
        let e = 1.0 - (1.0 - t).powi(3); // ease out
        let back = stage_backdrop(pw, ph);
        if self.stage_pm.as_ref().is_none_or(|p| p.width() != pw || p.height() != ph) {
            self.stage_pm = Pixmap::new(pw, ph);
        }
        let Some(frame) = self.stage_pm.as_mut() else { return };
        frame.data_mut().copy_from_slice(back.data());
        // the icons settle from slightly larger, as on macOS: scale about the screen's centre
        let z = 1.0 + 0.08 * (1.0 - e);
        let (ox, oy) = (sw as f32 / 2.0 * (1.0 - z) / (s * z), sh as f32 / 2.0 * (1.0 - z) / (s * z));
        let mut spots = Vec::new();
        {
            let mut ui = Ui { pm: frame, s: s * z, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: sw as f32 / s, fs: s, still: e >= 1.0 };
            self.content.draw(&mut ui);
        }
        self.spots = spots;
        self.view = (s, 0.0, 0.0);
        self.pos = (0, 0);
        if self.surface.as_ref().is_none_or(|sf| sf.w != pw || sf.h != ph) {
            self.surface = Surface::new(pw, ph);
        }
        let Some(sf) = self.surface.as_mut() else { return };
        sf.bytes().copy_from_slice(frame.data());
        sf.present_alpha(self.hwnd, 0, 0, (e * 255.0).round().clamp(1.0, 255.0) as u8);
    }

    /// The glass window: what is on screen behind it, blurred and bent at the rim like a lens, with a dark tint
    /// so the labels read on any wallpaper. Captured once, before the window first shows.
    #[allow(clippy::too_many_arguments)]
    fn build_glass(x: i32, y: i32, pw: u32, ph: u32, w: f32, panel_h: f32, s: f32) -> Option<Pixmap> {
        let under = Self::glass_under(x, y, pw, ph, s)?;
        Self::glass_body(&under, pw, ph, w, panel_h, s)
    }

    /// What is on screen where the window will be, blurred.
    fn glass_under(x: i32, y: i32, pw: u32, ph: u32, s: f32) -> Option<Pixmap> {
        let mut under = gfx::capture_screen(x, y, pw, ph).or_else(|| {
            let mut p = Pixmap::new(pw, ph)?;
            p.fill(Color::from_rgba8(0x30, 0x30, 0x34, 0xFF));
            Some(p)
        })?;
        gfx::blur(&mut under, 22.0 * s);
        Some(under)
    }

    fn glass_body(under: &Pixmap, pw: u32, ph: u32, w: f32, panel_h: f32, s: f32) -> Option<Pixmap> {
        let mut pm = Pixmap::new(pw, ph)?;
        let (gx, gy, gw, gh, r) = (MARGIN * s, MARGIN * s, w * s, panel_h * s, GLASS_R * s);
        if let Some((sh, sx, sy)) = gfx::rrect_shadow(gx, gy + 5.0 * s, gw, gh, r, 12.0 * s, (0, 0, 0, 0x70)) {
            // straight row blends (tiny-skia's general pipeline took several ms for these two big copies)
            gfx::blit_over(&mut pm.as_mut(), &sh, sx.round() as i32, sy.round() as i32);
        }
        let (mut g, lx, ly) = gfx::liquid_glass(under, gx, gy, gw, gh, r, 16.0 * s, (0.0, 0.0), (0.0, 0.0))?;
        // like the dock: a light grey wash, a touch brighter at the top; just enough shade that white labels
        // still read over a bright wallpaper. The glass is opaque inside, so the shadow under it never shows.
        gfx::shade_wash(&mut g, gy - ly as f32, gh, (0x20, 0x20, 0x24, 0x40), 0x20, 0x0E);
        gfx::blit_over(&mut pm.as_mut(), &g, lx, ly);
        // a hairline rim on top of the lens's own highlight
        if let Some(p) = gfx::rrect(gx + 0.5 * s, gy + 0.5 * s, gw - s, gh - s, r - 0.5 * s) {
            let mut paint = Paint::default();
            paint.set_color(rgba(white(0x26)));
            paint.anti_alias = true;
            pm.stroke_path(&p, &paint, &Stroke { width: s, ..Default::default() }, Transform::identity(), None);
        }
        Some(pm)
    }

    fn render_glass(&mut self, ch: f32) {
        let w = self.content.width() - 2.0 * MARGIN;
        let (padx, pady) = self.content.padding();
        let s = self.dpi;
        let panel_h = ch + 2.0 * pady;
        let top = self.content.glass_top();
        let full_h = top.map_or(panel_h, |t| t.1 + 2.0 * pady);
        let (pw, ph) = (((w + 2.0 * MARGIN) * s).ceil() as u32, ((full_h + 2.0 * MARGIN) * s).ceil() as u32);
        let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let x = (self.screen_w_px - pw as i32) / 2;
        let y = match top {
            // a fixed top, growing down
            Some((f, _)) => ((sh as f32 * f) as i32 - (MARGIN * s) as i32).max(self.top_px - (40.0 * s) as i32).max(0),
            // centred, a little above the middle as macOS places it, and never under the menu bar
            None => ((sh - ph as i32) / 2 - (24.0 * s) as i32).max(self.top_px - (40.0 * s) as i32).max(0),
        };
        self.pos = (x, y);
        let stale = self.glass.as_ref().is_none_or(|g| g.width() != pw || g.height() != ph);
        if top.is_none() {
            if stale {
                self.glass = Self::build_glass(x, y, pw, ph, w, panel_h, s);
            }
        } else if stale || (self.glass_h - panel_h).abs() > 0.25 {
            // the screen is captured once (before the window shows); only the glass is cut again as it grows
            if self.under.as_ref().is_none_or(|u| u.width() != pw || u.height() != ph) {
                self.under = Self::glass_under(x, y, pw, ph, s);
            }
            if let Some(u) = self.under.as_ref() {
                self.glass = Self::glass_body(u, pw, ph, w, panel_h, s);
                self.glass_h = panel_h;
            }
        }
        let Some(base) = self.glass.as_ref() else { return };
        let mut pm = base.clone();
        let (ox, oy) = (MARGIN + padx, MARGIN + pady);
        let mut spots = Vec::new();
        let t = if motion() && self.content.motion() { self.born.elapsed().as_secs_f32() } else { 1.0 };
        let opening = t * 1000.0 < GLASS_IN_MS;
        {
            let mut ui = Ui { pm: &mut pm, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: w - 2.0 * padx, fs: s, still: true };
            self.content.draw(&mut ui);
        }
        self.spots = spots;
        self.view = (s, ox, oy);
        self.content.placed(&self.spots, (x + (ox * s) as i32, y + (oy * s) as i32), s);
        if opening {
            // a damped spring: up from 90%, a hair past full size, settled by ~400 ms; the fade leads it
            let k = 1.0 - (-t * 14.0).exp() * (t * 16.0).cos();
            let z = 0.9 + 0.1 * k;
            let a = (t / 0.16).min(1.0);
            let a = 1.0 - (1.0 - a) * (1.0 - a);
            if let Some(mut out) = Pixmap::new(pw, ph) {
                let (ax, ay) = (pw as f32 / 2.0, (MARGIN + panel_h / 2.0) * s);
                let paint = PixmapPaint { opacity: a, quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
                out.draw_pixmap(0, 0, pm.as_ref(), &paint, Transform::from_row(z, 0.0, 0.0, z, ax - ax * z, ay - ay * z), None);
                pm = out;
            }
        }
        if self.surface.as_ref().is_none_or(|sf| sf.w != pw || sf.h != ph) {
            self.surface = Surface::new(pw, ph);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.bytes().copy_from_slice(pm.data());
            sf.present(self.hwnd, x, y);
        }
    }

    fn render_panel(&mut self) {
        if let Some(ch) = self.content.glass() {
            return self.render_glass(ch);
        }
        let w = self.content.width() - 2.0 * MARGIN; // the width includes the shadow margin, as in Flyout.cs
        let (padx, pady) = self.content.padding();
        let s = self.dpi;
        // measure pass: draw into a scratch pixmap sized for the tallest panel, keep the height
        let cw = w - 2.0 * padx;
        let max_h = 1000.0; // fits a 1080p screen at 100% under the bar
        let (pw, ph_max) = (((w + 2.0 * MARGIN) * s).ceil() as u32, ((max_h + 2.0 * MARGIN) * s).ceil() as u32);
        let mut pm = Pixmap::new(pw, ph_max).unwrap();
        let (ox, oy) = (MARGIN + padx, MARGIN + pady);
        // panel first at full height guess, then content; redrawn below once the height is known
        let mut spots = Vec::new();
        let ch = {
            let mut scratch = Pixmap::new(pw, ph_max).unwrap();
            let mut ui = Ui { pm: &mut scratch, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: cw, fs: s, still: true };
            self.content.draw(&mut ui)
        };
        let panel_h = (ch + 2.0 * pady).min(max_h);
        let total_h = ((panel_h + 2.0 * MARGIN) * s).ceil() as u32;
        // shadow + panel
        if let Some(path) = gfx::rrect(MARGIN * s, MARGIN * s, w * s, panel_h * s, 16.0 * s) {
            if let Some((sh, sx, sy)) = gfx::shadow(&path, 13.0 * s, (0, 0, 0, 0x80)) {
                pm.draw_pixmap(sx.round() as i32, (sy + 6.0 * s).round() as i32, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
            // clear the panel area so the translucent fill isn't darkened by the shadow under it
            let mut clear = Paint::default();
            clear.blend_mode = tiny_skia::BlendMode::Clear;
            pm.fill_path(&path, &clear, FillRule::Winding, Transform::identity(), None);
            let mut paint = Paint::default();
            paint.set_color(rgba(PANEL_FILL));
            paint.anti_alias = true;
            pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
        if let Some(p) = gfx::rrect(MARGIN * s + 0.5 * s, MARGIN * s + 0.5 * s, w * s - s, panel_h * s - s, 15.5 * s) {
            let mut paint = Paint::default();
            paint.set_color(rgba(white(0x2A)));
            paint.anti_alias = true;
            pm.stroke_path(&p, &paint, &Stroke { width: s, ..Default::default() }, Transform::identity(), None);
        }
        spots.clear();
        if self.full {
            return self.render_full(w, panel_h, padx, pady);
        }
        let _ = panel_h;
        {
            let mut ui = Ui { pm: &mut pm, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: cw, fs: s, still: true };
            self.content.draw(&mut ui);
        }
        self.spots = spots;
        self.view = (s, ox, oy);

        // right-align under the anchor, inside the screen
        let win_w = pw as i32;
        let x = if self.kind.centred() {
            (self.screen_w_px - win_w) / 2 // a window of its own, not under a menu bar item
        } else {
            self.right_px + (MARGIN * s) as i32 - win_w
        };
        let x = x.min(self.screen_w_px - win_w - (4.0 * s) as i32).max((8.0 * s) as i32);
        let y = self.top_px - (7.0 * s) as i32;
        let (x, y) = self.moved.unwrap_or((x, y));
        self.pos = (x, y);
        self.content.placed(&self.spots, (x + (ox * s) as i32, y + (oy * s) as i32), s);

        if self.surface.as_ref().is_none_or(|sf| sf.w != pw || sf.h != total_h) {
            self.surface = Surface::new(pw, total_h);
        }
        // opening: fade in and grow from 96% towards the menu bar item (or the middle, for centred panels)
        let t = if motion() && self.moved.is_none() { (self.born.elapsed().as_secs_f32() * 1000.0 / PANEL_IN_MS).min(1.0) } else { 1.0 };
        if t < 1.0 {
            let e = 1.0 - (1.0 - t).powi(3);
            let z = 0.96 + 0.04 * e;
            let ax = if self.kind.centred() { pw as f32 / 2.0 } else { pw as f32 - MARGIN * s };
            let ay = if self.kind.centred() { total_h as f32 / 2.0 } else { MARGIN * s };
            if let Some(mut out) = Pixmap::new(pw, ph_max) {
                let paint = PixmapPaint { opacity: e, quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
                out.draw_pixmap(0, 0, pm.as_ref(), &paint, Transform::from_row(z, 0.0, 0.0, z, ax - ax * z, ay - ay * z), None);
                pm = out;
            }
        }
        if let Some(sf) = self.surface.as_mut() {
            let src = &pm.data()[..(pw * total_h * 4) as usize];
            sf.bytes().copy_from_slice(src);
            sf.present(self.hwnd, x, y);
        }
    }

    /// Full screen: an opaque backdrop over the whole monitor, the panel's content scaled up in its middle.
    fn render_full(&mut self, w: f32, panel_h: f32, padx: f32, pady: f32) {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let Some(mut pm) = Pixmap::new(sw as u32, sh as u32) else { return };
        pm.fill(Color::from_rgba8(0x1C, 0x1C, 0x1E, 0xFF));
        let s0 = self.dpi;
        let z = ((sh as f32 * 0.9) / (panel_h * s0)).min((sw as f32 * 0.9) / (w * s0)).clamp(1.0, 1.6);
        let s = s0 * z;
        let ox = (sw as f32 / s - w) / 2.0 + padx;
        let oy = (sh as f32 / s - panel_h) / 2.0 + pady;
        let mut spots = Vec::new();
        {
            let cw = w - 2.0 * padx;
            let mut ui = Ui { pm: &mut pm, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: cw, fs: s, still: true };
            self.content.draw(&mut ui);
        }
        self.spots = spots;
        self.view = (s, ox, oy);
        self.pos = (0, 0);
        let (pw, ph) = (sw as u32, sh as u32);
        if self.surface.as_ref().is_none_or(|sf| sf.w != pw || sf.h != ph) {
            self.surface = Surface::new(pw, ph);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.bytes().copy_from_slice(pm.data());
            sf.present(self.hwnd, 0, 0);
        }
    }
}

/// Test aid: a full-screen panel over the blurred wallpaper at `w` x `h` DIPs and scale `s`.
#[cfg(test)]
pub fn snapshot_stage(content: &mut dyn Content, w: f32, h: f32, s: f32) -> (u32, u32, Vec<u8>) {
    let (pw, ph) = ((w * s) as u32, (h * s) as u32);
    assert!(content.stage(w, h));
    let back = stage_backdrop(pw, ph);
    let mut pm = (*back).clone();
    let mut spots = Vec::new();
    let mut ui = Ui { pm: &mut pm, s, ox: 0.0, oy: 0.0, mouse: None, pressed: None, spots: &mut spots, w, fs: s, still: true };
    content.draw(&mut ui);
    (pw, ph, pm.data().to_vec())
}

/// Test aid: a glass window the way `render_glass` paints it, over what is on screen in the middle right now.
#[cfg(test)]
pub fn snapshot_glass(content: &mut dyn Content, s: f32) -> (u32, u32, Vec<u8>) {
    let w = content.width() - 2.0 * MARGIN;
    let (padx, pady) = content.padding();
    let panel_h = content.glass().unwrap() + 2.0 * pady;
    let (pw, ph) = (((w + 2.0 * MARGIN) * s).ceil() as u32, ((panel_h + 2.0 * MARGIN) * s).ceil() as u32);
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let (x, y) = ((sw - pw as i32) / 2, (sh - ph as i32) / 2);
    let mut pm = Flyout::build_glass(x, y, pw, ph, w, panel_h, s).unwrap();
    let mut spots = Vec::new();
    let mut ui = Ui { pm: &mut pm, s, ox: MARGIN + padx, oy: MARGIN + pady, mouse: None, pressed: None, spots: &mut spots, w: w - 2.0 * padx, fs: s, still: true };
    content.draw(&mut ui);
    // on a mid-grey so the shadow and the rim show
    let mut out = Pixmap::new(pw, ph).unwrap();
    out.fill(Color::from_rgba8(0x80, 0x80, 0x80, 255));
    out.draw_pixmap(0, 0, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    (pw, ph, out.data().to_vec())
}

/// Test aid: paint a panel the way `Flyout::render` does, on a dark backdrop, and return (w, h, BGRA).
#[cfg(test)]
pub fn snapshot(content: &mut dyn Content, s: f32) -> (u32, u32, Vec<u8>) {
    let w = content.width() - 2.0 * MARGIN;
    let (padx, pady) = content.padding();
    let cw = w - 2.0 * padx;
    let pw = ((w + 2.0 * MARGIN) * s).ceil() as u32;
    let mut spots = Vec::new();
    let mut scratch = Pixmap::new(pw, (1200.0 * s) as u32).unwrap();
    let ch = {
        let mut ui = Ui { pm: &mut scratch, s, ox: MARGIN + padx, oy: MARGIN + pady, mouse: None, pressed: None, spots: &mut spots, w: cw, fs: s, still: true };
        content.draw(&mut ui)
    };
    let panel_h = ch + 2.0 * pady;
    let ph = ((panel_h + 2.0 * MARGIN) * s).ceil() as u32;
    let mut pm = Pixmap::new(pw, ph).unwrap();
    pm.fill(Color::from_rgba8(0x40, 0x30, 0x28, 255));
    if let Some(path) = gfx::rrect(MARGIN * s, MARGIN * s, w * s, panel_h * s, 16.0 * s) {
        let mut paint = Paint::default();
        paint.set_color(rgba(PANEL_FILL));
        paint.anti_alias = true;
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    let mut ui = Ui { pm: &mut pm, s, ox: MARGIN + padx, oy: MARGIN + pady, mouse: None, pressed: None, spots: &mut spots, w: cw, fs: s, still: true };
    content.draw(&mut ui);
    (pw, ph, pm.data().to_vec())
}


#[cfg(test)]
mod glass_bench {
    /// How long cutting Spotlight's glass takes at its tallest (it is re-cut every frame while it grows).
    #[test]
    #[ignore]
    fn glass_cut_time() {
        let s = 1.25;
        let (w, h) = (660.0, 400.0);
        let (pw, ph) = (((w + 2.0 * super::MARGIN) * s) as u32, ((h + 2.0 * super::MARGIN) * s) as u32);
        let t = std::time::Instant::now();
        let under = super::Flyout::glass_under(400, 300, pw, ph, s).unwrap();
        let cap = t.elapsed();
        let _ = super::Flyout::glass_body(&under, pw, ph, w, 60.0, s); // the shadow's base, once
        let t = std::time::Instant::now();
        for k in 0..20 {
            let _ = super::Flyout::glass_body(&under, pw, ph, w, 60.0 + k as f32 * 17.0, s);
        }
        println!("capture+blur {:?}, cut {:?} per frame", cap, t.elapsed() / 20);
        let (gx, gy, gw, gh, r) = (super::MARGIN * s, super::MARGIN * s, w * s, 300.0 * s, super::GLASS_R * s);
        let t = std::time::Instant::now();
        let _ = crate::gfx::rrect_shadow(gx, gy, gw, gh, r, 12.0 * s, (0, 0, 0, 0x70));
        println!("shadow {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let (mut g, _, _) = crate::gfx::liquid_glass(&under, gx, gy, gw, gh, r, 16.0 * s, (0.0, 0.0), (0.0, 0.0)).unwrap();
        println!("lens {:?}", t.elapsed());
        let t = std::time::Instant::now();
        crate::gfx::white_wash(&mut g, gy, gh, 0x20, 0x0E);
        println!("wash {:?}", t.elapsed());
    }
}
