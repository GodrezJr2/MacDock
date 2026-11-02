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
        Font { face: if icons { gfx::ICONS } else { gfx::UI }, px: (size * self.s).round() as i32, weight }
    }

    pub fn text_w(&self, s: &str, size: f32, weight: i32) -> f32 {
        gfx::measure(s, self.font(size, weight, false)).0 as f32 / self.s
    }

    /// Text with its line box's top at y; returns its width. `max_w` > 0 cuts it with an ellipsis.
    pub fn text(&mut self, x: f32, y: f32, s: &str, size: f32, weight: i32, c: (u8, u8, u8, u8), max_w: f32) -> f32 {
        let f = self.font(size, weight, false);
        let mut s = s.to_string();
        if max_w > 0.0 && gfx::measure(&s, f).0 as f32 / self.s > max_w {
            while !s.is_empty() && gfx::measure(&format!("{s}…"), f).0 as f32 / self.s > max_w {
                s.pop();
            }
            s.push('…');
        }
        self.draw_text(x, y, &s, f, c)
    }

    /// Text vertically centred on cy.
    pub fn text_mid(&mut self, x: f32, cy: f32, s: &str, size: f32, weight: i32, c: (u8, u8, u8, u8), max_w: f32) -> f32 {
        let h = gfx::measure("Ag", self.font(size, weight, false)).1 as f32 / self.s;
        self.text(x, cy - h / 2.0, s, size, weight, c, max_w)
    }

    /// A Segoe Fluent Icons glyph centred on (cx, cy).
    pub fn glyph(&mut self, cx: f32, cy: f32, g: char, size: f32, c: (u8, u8, u8, u8)) {
        let f = self.font(size, 400, true);
        let s = g.to_string();
        let (w, h) = gfx::measure(&s, f);
        self.draw_text(cx - w as f32 / self.s / 2.0, cy - h as f32 / self.s / 2.0, &s, f, c);
    }

    fn draw_text(&mut self, x: f32, y: f32, s: &str, f: Font, c: (u8, u8, u8, u8)) -> f32 {
        let Some(t) = gfx::text_ex(s, f, (c.0, c.1, c.2)) else { return 0.0 };
        let w = (t.width() as f32 - 2.0) / self.s;
        let paint = PixmapPaint { opacity: c.3 as f32 / 255.0, ..Default::default() };
        let px = ((self.ox + x) * self.s).round() as i32;
        let py = ((self.oy + y) * self.s).round() as i32;
        self.pm.draw_pixmap(px, py, t.as_ref(), &paint, Transform::identity(), None);
        w
    }

    /// An image (premultiplied BGRA) scaled into the box.
    pub fn image(&mut self, img: &Pixmap, x: f32, y: f32, w: f32, h: f32) {
        let sx = w * self.s / img.width() as f32;
        let sy = h * self.s / img.height() as f32;
        let tf = Transform::from_row(sx, 0.0, 0.0, sy, (self.ox + x) * self.s, (self.oy + y) * self.s);
        let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
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
        gfx::measure("Ag", self.font(size, weight, false)).1 as f32 / self.s
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
    /// Panel width in DIPs (outside the shadow margin).
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
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    match msg {
        WM_MOUSEMOVE => push(Ev::Fly(FlyEv::Move(x, y))),
        WM_MOUSELEAVE => push(Ev::Fly(FlyEv::Leave)),
        WM_LBUTTONDOWN => {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(h);
            push(Ev::Fly(FlyEv::Down(x, y)));
        }
        WM_LBUTTONUP => {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            push(Ev::Fly(FlyEv::Up(x, y)));
        }
        WM_RBUTTONUP => push(Ev::Fly(FlyEv::RightUp(x, y))),
        WM_MOUSEWHEEL => push(Ev::Fly(FlyEv::Wheel(((wp.0 >> 16) & 0xFFFF) as i16 as i32))),
        WM_CHAR => {
            if let Some(c) = char::from_u32(wp.0 as u32) {
                push(Ev::Fly(FlyEv::Char(c)));
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => push(Ev::Fly(FlyEv::Key(wp.0 as u32))),
        WM_ACTIVATE if (wp.0 & 0xFFFF) == 0 => push(Ev::Fly(FlyEv::Deactivated)),
        WM_TIMER => push(Ev::Fly(FlyEv::Tick)),
        WM_FLY_DATA => push(Ev::Fly(FlyEv::Data)),
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn create_window() -> HWND {
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
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("MacDockFlyout"), w!("MacDock Flyout"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        )
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
    tracking: bool,
    pub stay_open: bool,
}

impl Flyout {
    /// Open under the menu bar item whose right edge is `right_px`; `top_px` is the bar's bottom.
    pub fn open(kind: crate::menubar::Extra, content: Box<dyn Content>, right_px: i32, top_px: i32, dpi: f64) -> Flyout {
        let hwnd = create_window();
        let screen_w_px = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let mut f = Flyout {
            hwnd, kind, content, dpi: dpi as f32, right_px, top_px, screen_w_px, pos: (0, 0), mouse: None, hot: None,
            press: None, spots: Vec::new(), surface: None, tracking: false, stay_open: false,
        };
        f.content.opened(hwnd.0 as isize);
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
            let _ = DestroyWindow(self.hwnd);
        }
    }

    fn content_origin(&self) -> (f32, f32) {
        let (px, py) = self.content.padding();
        (MARGIN + px, MARGIN + py)
    }

    fn to_content(&self, x: i32, y: i32) -> (f32, f32) {
        let (ox, oy) = self.content_origin();
        (x as f32 / self.dpi - ox, y as f32 / self.dpi - oy)
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
                }
            }
            FlyEv::Up(x, y) => {
                let p = self.to_content(x, y);
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
            FlyEv::Char(c) => cmd = self.content.char(c),
            FlyEv::Key(0x1B) => cmd = Cmd::Close,
            FlyEv::Key(vk) => cmd = self.content.key(vk),
            FlyEv::Deactivated => {
                if !self.stay_open {
                    cmd = Cmd::Close;
                }
            }
            FlyEv::Tick => self.content.tick(),
            FlyEv::Data => self.content.data(),
        }
        if !matches!(cmd, Cmd::None) {
            return Some(cmd);
        }
        self.render();
        None
    }

    pub fn render(&mut self) {
        let w = self.content.width();
        let (padx, pady) = self.content.padding();
        let s = self.dpi;
        // measure pass: draw into a scratch pixmap sized for the tallest panel, keep the height
        let cw = w - 2.0 * padx;
        let max_h = 900.0;
        let (pw, ph_max) = (((w + 2.0 * MARGIN) * s).ceil() as u32, ((max_h + 2.0 * MARGIN) * s).ceil() as u32);
        let mut pm = Pixmap::new(pw, ph_max).unwrap();
        let (ox, oy) = (MARGIN + padx, MARGIN + pady);
        // panel first at full height guess, then content; redrawn below once the height is known
        let mut spots = Vec::new();
        let ch = {
            let mut scratch = Pixmap::new(pw, ph_max).unwrap();
            let mut ui = Ui { pm: &mut scratch, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: cw };
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
        {
            let mut ui = Ui { pm: &mut pm, s, ox, oy, mouse: self.mouse, pressed: self.press, spots: &mut spots, w: cw };
            self.content.draw(&mut ui);
        }
        self.spots = spots;

        // right-align under the anchor, inside the screen
        let win_w = pw as i32;
        let x = (self.right_px + (MARGIN * s) as i32 - win_w).clamp((8.0 * s) as i32 - (MARGIN * s) as i32, self.screen_w_px - win_w + (MARGIN * s) as i32 - (4.0 * s) as i32);
        let y = self.top_px - (7.0 * s) as i32;
        self.pos = (x, y);
        self.content.placed(&self.spots, (x + (ox * s) as i32, y + (oy * s) as i32), s);

        if self.surface.as_ref().is_none_or(|sf| sf.w != pw || sf.h != total_h) {
            self.surface = Surface::new(pw, total_h);
        }
        if let Some(sf) = self.surface.as_mut() {
            let src = &pm.data()[..(pw * total_h * 4) as usize];
            sf.bytes().copy_from_slice(src);
            sf.present(self.hwnd, x, y);
        }
    }
}
