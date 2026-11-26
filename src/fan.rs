//! Window fan: hovering a dock icon whose app has two or more windows fans pictures of them out above the
//! icon, like a hand of cards on an arc. Hovering a card lifts it out of the hand, in front of the others; a
//! click brings that window up (through the dock, so a minimized one comes back with the genie). The fan
//! goes away once the pointer has been off the cards, the icon and the way between them for a moment.
//!
//! Its own thread and window (layered, topmost, never activates), like the switcher: taking the pictures
//! (PrintWindow, ~15-25 ms a window) and drawing the cards stay off the dock's frame loop.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Mask, Paint, Pixmap, PixmapPaint, Point, Rect, SpreadMode, Stroke,
    Transform,
};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};

/// What the dock asks the fan to show.
pub struct Request {
    /// the dock item it is for
    pub key: String,
    /// (hwnd, title), front first
    pub wins: Vec<(isize, String)>,
    pub icon: Option<Pixmap>,
    /// the icon on screen, px
    pub anchor: RECT,
    /// its monitor, px
    pub monitor: RECT,
    pub scale: f32,
    pub motion: bool,
    /// the dock window to tell which window was picked
    pub dock: isize,
}

/// Posted to the dock when a card is clicked; wparam is the window.
pub const WM_FAN_PICK: u32 = WM_APP + 40;
const WM_FAN_SHOW: u32 = WM_APP + 1;
const WM_FAN_HIDE: u32 = WM_APP + 2;
const TIMER_TICK: usize = 1;

// DIPs
const CARD_W: f32 = 232.0;
const RADIUS: f32 = 640.0; // of the arc the cards' bottoms sit on
const MAX_STEP: f32 = 12.0; // degrees between two cards
const MAX_SPREAD: f32 = 64.0; // degrees from the first card to the last
const GAP: f32 = 18.0; // lowest card corner to the top of the icon
const LIFT: f32 = 30.0; // a hovered card moves out of the hand this far
const MARGIN: f32 = 44.0; // room for shadows and the lift around the cards
const CORNER: f32 = 12.0;
const MAX_CARDS: usize = 9;
const OPEN_SECS: f32 = 0.30;
const LINGER_SECS: f32 = 0.28; // off the fan this long and it closes

static HWND_FAN: AtomicIsize = AtomicIsize::new(0);
static OPEN: AtomicBool = AtomicBool::new(false);
static KEY: Mutex<String> = Mutex::new(String::new());
static PENDING: Mutex<Option<Request>> = Mutex::new(None);

fn post(msg: u32) {
    let h = HWND_FAN.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Fan out this app's windows (replaces a fan already up).
pub fn show(r: Request) {
    if HWND_FAN.load(Ordering::Relaxed) == 0 {
        return;
    }
    *KEY.lock().unwrap() = r.key.clone();
    *PENDING.lock().unwrap() = Some(r);
    OPEN.store(true, Ordering::Relaxed);
    post(WM_FAN_SHOW);
}

/// Fold the fan away, if one is up.
pub fn hide() {
    if OPEN.load(Ordering::Relaxed) {
        post(WM_FAN_HIDE);
    }
}

pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// A fan is up (or on its way) for this dock item.
pub fn open_for(key: &str) -> bool {
    is_open() && *KEY.lock().unwrap() == key
}

struct Card {
    hwnd: isize,
    img: Pixmap,
    shadow: Option<(Pixmap, f32, f32)>,
    angle: f32, // at rest, degrees
    lift: f32,  // 0..1, follows the hover
}

#[derive(Clone, Copy)]
struct Pose {
    x: f32, // bottom centre, window px
    y: f32,
    angle: f32,
    scale: f32,
    alpha: f32,
}

struct Fan {
    hwnd: HWND,
    cards: Vec<Card>,
    s: f32,
    pos: (i32, i32),
    size: (u32, u32),
    pivot: (f32, f32), // window px
    anchor: RECT,      // screen px
    low: f32,          // lowest card corner at rest, screen px
    open: f32,         // 0..1
    closing: bool,
    motion: bool,
    hovered: Option<usize>,
    top: Option<usize>, // drawn last: the hovered card, or the one settling back
    off_since: Option<Instant>,
    last: Instant,
    dock: isize,
    surface: Option<Surface>,
    drawn: bool,
}

thread_local! {
    static STATE: std::cell::RefCell<Option<Fan>> = const { std::cell::RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Fan) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn ease_out(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// The angles of n cards, left to right, centred on 0.
fn angles(n: usize) -> Vec<f32> {
    if n < 2 {
        return vec![0.0; n];
    }
    let step = MAX_STEP.min(MAX_SPREAD / (n - 1) as f32);
    (0..n).map(|i| (i as f32 - (n - 1) as f32 / 2.0) * step).collect()
}

/// Where a card at this angle on the arc around the pivot (radius r) sits, lifted (0..1) towards the
/// outside and turned a little upright.
fn pose_on_arc(pivot: (f32, f32), r: f32, angle: f32, lift: f32, lift_px: f32) -> Pose {
    let a = angle.to_radians();
    let d = r + lift * lift_px;
    Pose { x: pivot.0 + d * a.sin(), y: pivot.1 - d * a.cos(), angle: angle * (1.0 - 0.3 * lift), scale: 1.0 + 0.07 * lift, alpha: 1.0 }
}

fn transform(p: &Pose, w: f32, h: f32) -> Transform {
    Transform::from_translate(p.x, p.y).pre_rotate(p.angle).pre_scale(p.scale, p.scale).pre_translate(-w / 2.0, -h)
}

fn corners(p: &Pose, w: f32, h: f32) -> [Point; 4] {
    let mut pts = [Point::from_xy(0.0, 0.0), Point::from_xy(w, 0.0), Point::from_xy(0.0, h), Point::from_xy(w, h)];
    transform(p, w, h).map_points(&mut pts);
    pts
}

/// Cut a title to fit room px, with an ellipsis.
fn fit(title: &str, font: Font, room: f32) -> String {
    if gfx::measure(title, font).0 as f32 <= room {
        return title.to_string();
    }
    let mut chars: Vec<char> = title.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let s: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
        if gfx::measure(&s, font).0 as f32 <= room {
            return s;
        }
    }
    String::new()
}

fn paint_rgba(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(b, g, r, a)); // BGRA surfaces
    p.anti_alias = true;
    p
}

/// One card: the window's picture (or the app icon on a dark card), its title on a dark fade along the
/// bottom, rounded, with a thin light rim.
fn make_card(pic: Option<Pixmap>, icon: Option<&Pixmap>, title: &str, s: f32) -> Option<Pixmap> {
    let cw = (CARD_W * s).round();
    let aspect = pic.as_ref().map_or(0.62, |p| p.height() as f32 / p.width().max(1) as f32).clamp(0.5, 0.78);
    let ch = (cw * aspect).round();
    let mut pm = Pixmap::new(cw as u32, ch as u32)?;
    let q = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
    match pic {
        Some(p) => {
            let p = gfx::shrink(p, cw as u32);
            // cover the card, top aligned so the window's title bar shows
            let k = (cw / p.width() as f32).max(ch / p.height() as f32);
            let x = (cw - p.width() as f32 * k) / 2.0;
            pm.draw_pixmap(0, 0, p.as_ref(), &q, Transform::from_row(k, 0.0, 0.0, k, x, 0.0), None);
        }
        None => {
            pm.fill(Color::from_rgba8(0x2E, 0x2A, 0x2A, 0xFF));
            if let Some(ic) = icon {
                let side = ch * 0.46;
                let k = side / ic.width().max(ic.height()) as f32;
                let t = Transform::from_row(k, 0.0, 0.0, k, (cw - ic.width() as f32 * k) / 2.0, ch * 0.38 - ic.height() as f32 * k / 2.0);
                pm.draw_pixmap(0, 0, ic.as_ref(), &q, t, None);
            }
        }
    }
    // title on a dark fade
    let fade = 44.0 * s;
    if let (Some(rect), Some(shader)) = (
        Rect::from_xywh(0.0, ch - fade, cw, fade),
        LinearGradient::new(
            Point::from_xy(0.0, ch - fade),
            Point::from_xy(0.0, ch),
            vec![GradientStop::new(0.0, Color::from_rgba8(0, 0, 0, 0)), GradientStop::new(1.0, Color::from_rgba8(0, 0, 0, 0xC0))],
            SpreadMode::Pad,
            Transform::identity(),
        ),
    ) {
        let p = Paint { shader, anti_alias: false, ..Default::default() };
        pm.fill_rect(rect, &p, Transform::identity(), None);
    }
    let font = Font { face: gfx::UI, px: (12.5 * s).round() as i32, weight: 600 };
    let title = fit(title.trim(), font, cw - 24.0 * s);
    if let Some(t) = gfx::text_ex(&title, font, (255, 255, 255)) {
        let x = ((cw - t.width() as f32) / 2.0).round() as i32;
        let y = (ch - 9.0 * s - t.height() as f32).round() as i32;
        pm.draw_pixmap(x, y, t.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }
    // round the corners, then the rim
    let r = CORNER * s;
    let path = gfx::rrect(0.0, 0.0, cw, ch, r)?;
    let mut mask = Mask::new(cw as u32, ch as u32)?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    pm.apply_mask(&mask);
    if let Some(rim) = gfx::rrect(0.5, 0.5, cw - 1.0, ch - 1.0, r - 0.5) {
        let st = Stroke { width: 1.0, ..Default::default() };
        pm.stroke_path(&rim, &paint_rgba(255, 255, 255, 0x40), &st, Transform::identity(), None);
    }
    Some(pm)
}

impl Fan {
    fn card_size(&self, i: usize) -> (f32, f32) {
        let c = &self.cards[i];
        (c.img.width() as f32, c.img.height() as f32)
    }

    /// Card i's pose now: coming out of the icon while opening, lifted when hovered.
    fn pose(&self, i: usize) -> Pose {
        let n = self.cards.len();
        let c = &self.cards[i];
        let rest = pose_on_arc(self.pivot, RADIUS * self.s, c.angle, c.lift, LIFT * self.s);
        // the outer cards trail a little behind the middle ones
        let mid = (n as f32 - 1.0) / 2.0;
        let lag = if n > 1 { (i as f32 - mid).abs() / mid.max(1.0) * 0.3 } else { 0.0 };
        let t = ease_out((self.open - lag) / (1.0 - 0.3));
        if t >= 1.0 {
            return rest;
        }
        let ax = (self.anchor.left + self.anchor.right) as f32 / 2.0 - self.pos.0 as f32;
        let ay = self.anchor.top as f32 + (self.anchor.bottom - self.anchor.top) as f32 * 0.5 - self.pos.1 as f32;
        Pose {
            x: lerp(ax, rest.x, t),
            y: lerp(ay, rest.y, t),
            angle: rest.angle * t,
            scale: lerp(0.22, rest.scale, t),
            alpha: (t * 1.8).min(1.0),
        }
    }

    fn order(&self) -> Vec<usize> {
        let mut o: Vec<usize> = (0..self.cards.len()).filter(|&i| Some(i) != self.top).collect();
        if let Some(t) = self.top {
            o.push(t);
        }
        o
    }

    /// Card i where it rests in the hand, not lifted.
    fn rest_pose(&self, i: usize) -> Pose {
        pose_on_arc(self.pivot, RADIUS * self.s, self.cards[i].angle, 0.0, LIFT * self.s)
    }

    fn contains(&self, i: usize, p: &Pose, x: f32, y: f32, slack: f32) -> bool {
        let (w, h) = self.card_size(i);
        let Some(inv) = transform(p, w, h).invert() else { return false };
        let mut pt = Point::from_xy(x, y);
        inv.map_point(&mut pt);
        pt.x >= -slack && pt.x <= w + slack && pt.y >= -slack && pt.y <= h + slack
    }

    /// The card under a window point, as drawn now, topmost first; slack px around each card counts too.
    fn card_at(&self, x: f32, y: f32, slack: f32) -> Option<usize> {
        self.order().into_iter().rev().find(|&i| self.contains(i, &self.pose(i), x, y, slack))
    }

    /// The card to lift for a window point. Judged against the cards at rest, which do not move, so lifting
    /// one never decides the hover by itself: with the moving cards, a card lifted out from under the pointer
    /// left it on its neighbour, which lifted in turn, and the two swapped back and forth. The hovered card
    /// stays hovered while the pointer is on it at rest or where it is lifted to.
    fn hover_at(&self, x: f32, y: f32) -> Option<usize> {
        if let Some(h) = self.hovered.filter(|&h| h < self.cards.len()) {
            if self.contains(h, &self.rest_pose(h), x, y, 0.0) || self.contains(h, &self.pose(h), x, y, 0.0) {
                return Some(h);
            }
        }
        (0..self.cards.len()).rev().find(|&i| self.contains(i, &self.rest_pose(i), x, y, 0.0))
    }

    fn start(&mut self, r: Request) {
        let s = r.scale.max(0.5);
        self.s = s;
        self.motion = r.motion;
        self.dock = r.dock;
        self.anchor = r.anchor;
        // back window on the left, the front one on the right and on top
        let wins: Vec<(isize, String)> = r.wins.into_iter().take(MAX_CARDS).rev().collect();
        let pics: Vec<Option<Pixmap>> = std::thread::scope(|sc| {
            let jobs: Vec<_> = wins.iter().map(|(h, _)| {
                let h = *h;
                sc.spawn(move || crate::genie::window_picture(HWND(h as *mut _)))
            }).collect();
            jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
        });
        let ang = angles(wins.len());
        self.cards = wins.iter().zip(pics).zip(ang).filter_map(|(((h, title), pic), angle)| {
            let img = make_card(pic, r.icon.as_ref(), title, s)?;
            let (w, hh) = (img.width() as f32, img.height() as f32);
            let shadow = gfx::rrect_shadow(0.0, 6.0 * s, w, hh, CORNER * s, 16.0 * s, (0, 0, 0, 0x80));
            Some(Card { hwnd: *h, img, shadow, angle, lift: 0.0 })
        }).collect();
        if self.cards.is_empty() {
            self.finish();
            return;
        }
        self.layout(r.monitor);
        self.render();
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            SetTimer(self.hwnd, TIMER_TICK, 15, None);
        }
    }

    /// Lay the hand out in screen px with its pivot under the icon, lifted clear of it, kept on the monitor;
    /// sizes the window and resets the animation.
    fn layout(&mut self, monitor: RECT) {
        let (s, anchor) = (self.s, self.anchor);
        // lay the hand out in screen px with its pivot under the icon, then lift it clear of the icon
        let rr = RADIUS * s;
        let ax = (anchor.left + anchor.right) as f32 / 2.0;
        let mut pivot = (ax, rr);
        let all = |pivot: (f32, f32), lift: f32| -> Vec<Point> {
            self.cards.iter().flat_map(|c| {
                corners(&pose_on_arc(pivot, rr, c.angle, lift, LIFT * s), c.img.width() as f32, c.img.height() as f32)
            }).collect()
        };
        let low = all(pivot, 0.0).iter().map(|p| p.y).fold(f32::MIN, f32::max);
        pivot.1 += anchor.top as f32 - GAP * s - low;
        let mut pts = all(pivot, 0.0);
        pts.extend(all(pivot, 1.0));
        let m = MARGIN * s;
        let (mut l, mut t, mut rt, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &pts {
            (l, t, rt, b) = (l.min(p.x), t.min(p.y), rt.max(p.x), b.max(p.y));
        }
        (l, t, rt, b) = (l - m, t - m, rt + m, b + m);
        // keep it on the monitor
        let shift = if l < monitor.left as f32 { monitor.left as f32 - l } else if rt > monitor.right as f32 { monitor.right as f32 - rt } else { 0.0 };
        (l, rt) = (l + shift, rt + shift);
        pivot.0 += shift;
        t = t.max(monitor.top as f32);
        self.low = anchor.top as f32 - GAP * s;
        self.pos = (l.floor() as i32, t.floor() as i32);
        self.size = ((rt - l).ceil().max(1.0) as u32, (b - t).ceil().max(1.0) as u32);
        self.pivot = (pivot.0 - self.pos.0 as f32, pivot.1 - self.pos.1 as f32);
        self.open = if self.motion { 0.0 } else { 1.0 };
        self.closing = false;
        self.hovered = None;
        self.top = None;
        self.off_since = None;
        self.last = Instant::now();
        self.surface = Surface::new(self.size.0, self.size.1);
        self.drawn = false;
    }

    fn close(&mut self) {
        if self.cards.is_empty() {
            self.finish();
            return;
        }
        self.closing = true;
        self.hovered = None;
        if !self.motion {
            self.finish();
        }
    }

    fn finish(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TICK);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.cards.clear();
        self.surface = None;
        self.closing = false;
        // a newer fan may already be waiting to be shown
        if PENDING.lock().unwrap().is_none() {
            OPEN.store(false, Ordering::Relaxed);
        }
    }

    /// The pointer is on the fan, the icon, or the way from one to the other.
    fn pointer_near(&self, p: POINT) -> bool {
        let s = self.s;
        let a = &self.anchor;
        let (x, y) = (p.x as f32, p.y as f32);
        let pad = 6.0 * s;
        if x >= a.left as f32 - pad && x <= a.right as f32 + pad && y >= a.top as f32 - pad && y <= a.bottom as f32 + pad {
            return true;
        }
        let lane = 24.0 * s;
        if x >= a.left as f32 - lane && x <= a.right as f32 + lane && y >= self.low - 40.0 * s && y <= a.top as f32 {
            return true;
        }
        self.card_at(x - self.pos.0 as f32, y - self.pos.1 as f32, 18.0 * s).is_some()
    }

    fn tick(&mut self) {
        if self.cards.is_empty() {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.05);
        self.last = now;
        let mut moving = false;
        if self.closing {
            self.open = (self.open - dt / (OPEN_SECS * 0.6)).max(0.0);
            if self.open <= 0.0 {
                self.finish();
                return;
            }
            moving = true;
        } else if self.open < 1.0 {
            self.open = (self.open + dt / OPEN_SECS).min(1.0);
            moving = true;
        }
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        if !self.closing {
            if self.pointer_near(p) {
                self.off_since = None;
            } else if self.off_since.get_or_insert(now).elapsed().as_secs_f32() > LINGER_SECS {
                self.close();
            }
        }
        let hov = if self.closing || self.open < 0.7 { None } else { self.hover_at((p.x - self.pos.0) as f32, (p.y - self.pos.1) as f32) };
        if hov != self.hovered {
            self.hovered = hov;
            moving = true;
        }
        if hov.is_some() {
            self.top = hov;
        }
        let k = if self.motion { 1.0 - (-dt * 16.0).exp() } else { 1.0 };
        for (i, c) in self.cards.iter_mut().enumerate() {
            let want = if Some(i) == hov { 1.0 } else { 0.0 };
            let next = if (c.lift - want).abs() < 0.002 { want } else { c.lift + (want - c.lift) * k };
            if next != c.lift {
                c.lift = next;
                moving = true;
            }
        }
        if hov.is_none() && self.top.is_some_and(|t| self.cards[t].lift == 0.0) {
            self.top = None;
        }
        if moving || !self.drawn {
            self.render();
        }
    }

    fn render(&mut self) {
        let Some(mut sf) = self.surface.take() else { return };
        self.paint(&mut sf.pixmap());
        sf.present(self.hwnd, self.pos.0, self.pos.1);
        self.surface = Some(sf);
        self.drawn = true;
    }

    fn paint(&self, pm: &mut tiny_skia::PixmapMut) {
        let order = self.order();
        let poses: Vec<Pose> = (0..self.cards.len()).map(|i| self.pose(i)).collect();
        let fade = if self.closing { ease_out(self.open) } else { 1.0 };
        pm.fill(Color::TRANSPARENT);
        for &i in &order {
            let c = &self.cards[i];
            let p = poses[i];
            let (w, h) = (c.img.width() as f32, c.img.height() as f32);
            let t = transform(&p, w, h);
            let a = p.alpha * fade;
            if let Some((sh, ox, oy)) = &c.shadow {
                let pp = PixmapPaint { opacity: a * (0.75 + 0.25 * c.lift), quality: FilterQuality::Bilinear, ..Default::default() };
                pm.draw_pixmap(0, 0, sh.as_ref(), &pp, t.pre_translate(*ox, *oy), None);
            }
            let pp = PixmapPaint { opacity: a, quality: FilterQuality::Bilinear, ..Default::default() };
            pm.draw_pixmap(0, 0, c.img.as_ref(), &pp, t, None);
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        if self.closing || self.cards.is_empty() {
            return;
        }
        // the lifted card is the one the user is looking at; else whatever is drawn under the pointer
        if let Some(i) = self.hovered.or_else(|| self.card_at(x, y, 0.0)) {
            let h = self.cards[i].hwnd;
            unsafe {
                let _ = PostMessageW(HWND(self.dock as *mut _), WM_FAN_PICK, WPARAM(h as usize), LPARAM(0));
            }
            self.close();
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_FAN_SHOW => {
            let r = PENDING.lock().unwrap().take();
            if let Some(r) = r {
                with(|f| f.start(r));
            }
            return LRESULT(0);
        }
        WM_FAN_HIDE => {
            with(|f| f.close());
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_TICK => {
            with(|f| f.tick());
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONUP => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            with(|f| f.click(x, y));
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Start the fan's thread: its window and a message loop.
pub fn start() {
    let _ = std::thread::Builder::new().name("fan".into()).spawn(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockFan"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            w!("MacDockFan"), w!("MacDock Windows"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return;
        };
        STATE.with(|st| {
            *st.borrow_mut() = Some(Fan {
                hwnd, cards: Vec::new(), s: 1.0, pos: (0, 0), size: (1, 1), pivot: (0.0, 0.0), anchor: RECT::default(), low: 0.0,
                open: 0.0, closing: false, motion: true, hovered: None, top: None, off_since: None, last: Instant::now(), dock: 0,
                surface: None, drawn: false,
            })
        });
        HWND_FAN.store(hwnd.0 as isize, Ordering::Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angles_are_centred_and_capped() {
        assert_eq!(angles(1), vec![0.0]);
        assert_eq!(angles(3), vec![-MAX_STEP, 0.0, MAX_STEP]);
        let a = angles(9);
        assert!((a[8] - a[0] - MAX_SPREAD).abs() < 1e-3);
        assert!((a[0] + a[8]).abs() < 1e-3);
    }

    #[test]
    fn a_lifted_card_moves_outwards() {
        let rest = pose_on_arc((0.0, 1000.0), 600.0, 10.0, 0.0, 30.0);
        let up = pose_on_arc((0.0, 1000.0), 600.0, 10.0, 1.0, 30.0);
        assert!(up.x > rest.x && up.y < rest.y);
        assert!(up.angle.abs() < rest.angle.abs());
    }

    /// Render a fan of five made-up windows over a plain backdrop to target/fan.png, the third card hovered
    /// (cargo test fan_snapshot -- --ignored).
    #[test]
    #[ignore]
    fn fan_snapshot() {
        let s = 1.25;
        let colours = [(0x1E, 0x1E, 0x1E), (0x0C, 0x30, 0x60), (0xF2, 0xF2, 0xF2), (0x30, 0x10, 0x40), (0x12, 0x40, 0x22)];
        let cards: Vec<Card> = angles(5).into_iter().zip(colours).enumerate().map(|(i, (angle, (r, g, b)))| {
            let mut pic = Pixmap::new(1600, 1000).unwrap();
            pic.fill(Color::from_rgba8(b, g, r, 255));
            let bar = Rect::from_xywh(0.0, 0.0, 1600.0, 70.0).unwrap();
            pic.fill_rect(bar, &paint_rgba(0x55, 0x55, 0x60, 255), Transform::identity(), None);
            let img = make_card(Some(pic), None, &format!("Window {} — some long title to cut short here", i + 1), s).unwrap();
            let (w, h) = (img.width() as f32, img.height() as f32);
            let shadow = gfx::rrect_shadow(0.0, 6.0 * s, w, h, CORNER * s, 16.0 * s, (0, 0, 0, 0x80));
            Card { hwnd: 0, img, shadow, angle, lift: if i == 2 { 1.0 } else { 0.0 } }
        }).collect();
        let mut f = Fan {
            hwnd: HWND::default(), cards, s, pos: (0, 0), size: (1, 1), pivot: (0.0, 0.0),
            anchor: RECT { left: 1200, top: 1840, right: 1290, bottom: 1930 }, low: 0.0, open: 1.0, closing: false, motion: true,
            hovered: Some(2), top: Some(2), off_since: None, last: Instant::now(), dock: 0, surface: None, drawn: false,
        };
        f.layout(RECT { left: 0, top: 0, right: 2560, bottom: 2000 });
        f.open = 1.0;
        let mut out = Pixmap::new(f.size.0, f.size.1).unwrap();
        out.fill(Color::from_rgba8(0x90, 0x50, 0x40, 255));
        let mut layer = Pixmap::new(f.size.0, f.size.1).unwrap();
        f.paint(&mut layer.as_mut());
        out.draw_pixmap(0, 0, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        eprintln!("fan size {:?} pos {:?} pivot {:?}", f.size, f.pos, f.pivot);
        let path = format!(r"{}/target/fan.png", env!("CARGO_MANIFEST_DIR"));
        assert!(gfx::save_png(&path, out.width(), out.height(), out.data()));
    }

    /// Two overlapping cards, the pointer on the lower edge of the right one, where its lift leaves it and
    /// only the left one is drawn: the hover must stay put frame after frame instead of swapping.
    #[test]
    fn hover_does_not_swap_between_overlapping_cards() {
        let s = 1.0;
        let cards: Vec<Card> = angles(2).into_iter().map(|angle| {
            let img = Pixmap::new(232, 150).unwrap();
            Card { hwnd: 0, img, shadow: None, angle, lift: 0.0 }
        }).collect();
        let mut f = Fan {
            hwnd: HWND::default(), cards, s, pos: (0, 0), size: (1, 1), pivot: (0.0, 0.0),
            anchor: RECT { left: 1000, top: 1400, right: 1060, bottom: 1460 }, low: 0.0, open: 1.0, closing: false, motion: true,
            hovered: None, top: None, off_since: None, last: Instant::now(), dock: 0, surface: None, drawn: false,
        };
        f.layout(RECT { left: 0, top: 0, right: 2560, bottom: 1600 });
        f.open = 1.0;
        // a point inside the right card at rest, near its bottom, also inside the left card
        let r = f.rest_pose(1);
        let (w, h) = f.card_size(1);
        let mut pt = Point::from_xy(12.0, h - 6.0);
        transform(&r, w, h).map_point(&mut pt);
        let mut seen = Vec::new();
        for _ in 0..20 {
            let hov = f.hover_at(pt.x, pt.y);
            f.hovered = hov;
            if hov.is_some() { f.top = hov; }
            for (i, c) in f.cards.iter_mut().enumerate() {
                c.lift = if Some(i) == hov { 1.0 } else { 0.0 };
            }
            seen.push(hov);
        }
        assert!(seen.iter().all(|&s| s == seen[0]), "hover changed: {seen:?}");
        assert_eq!(seen[0], Some(1));
    }

    #[test]
    fn shrink_halves_down_to_size() {
        let mut p = Pixmap::new(1000, 600).unwrap();
        p.fill(Color::WHITE);
        let s = gfx::shrink(p, 200);
        assert_eq!((s.width(), s.height()), (250, 150));
        assert_eq!(s.data()[0..4], [255, 255, 255, 255]);
    }
}
