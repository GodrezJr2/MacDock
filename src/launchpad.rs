//! Launchpad: every installed app (the shell's AppsFolder: Start menu shortcuts and packaged apps) in a
//! searchable, paged grid. Type to filter, arrows and Enter to launch, right-click to keep one in the dock.
//!
//! The list and the icons are read on a worker thread and kept for the next open, so only the very first
//! open waits; each open refreshes the list in the background for apps installed since.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use tiny_skia::Pixmap;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY,
    SIGDN_PARENTRELATIVEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::flyout::{white, Cmd, Content, Ui, ACCENT, WM_FLY_DATA};
use crate::i18n::t;

#[derive(Clone)]
pub(crate) struct App {
    pub(crate) name: String,
    /// shell:AppsFolder\<id>, what launching and pinning use
    pub(crate) spec: String,
    pub(crate) icon: Option<Arc<Pixmap>>,
}

pub(crate) static CATALOG: Mutex<Vec<App>> = Mutex::new(Vec::new());
static LOADING: AtomicBool = AtomicBool::new(false);
/// the open panel's window, which the worker tells about new data (0 = none)
pub(crate) static NOTIFY: AtomicIsize = AtomicIsize::new(0);

pub(crate) const ICON_PX: u32 = 96;

/// How well an app name (lower case) matches a search (lower case): names starting with it first (0), then
/// names with a word starting with it (1), then anywhere in the name (2).
pub(crate) fn rank(n: &str, q: &str) -> Option<u8> {
    if n.starts_with(q) {
        Some(0)
    } else if n.split([' ', '-', '.', '_']).any(|w| w.starts_with(q)) {
        Some(1)
    } else if n.contains(q) {
        Some(2)
    } else {
        None
    }
}

/// The caret blinks at this half period, as on macOS.
pub(crate) const BLINK_MS: u128 = 530;
/// Results fade and settle in this long after the search changes.
pub(crate) const RESULTS_IN_MS: f32 = 200.0;

pub(crate) fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn notify() {
    let h = NOTIFY.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_FLY_DATA, WPARAM(0), LPARAM(0));
        }
    }
}

/// Not apps: the uninstallers, read-mes and web links that installers drop into the Start menu.
fn junk(name: &str, id: &str) -> bool {
    let n = name.to_lowercase();
    let id = id.to_lowercase();
    const WORDS: [&str; 9] = ["uninstall", "readme", "read me", "release notes", "documentation", "help file", "website", "license", "changelog"];
    WORDS.iter().any(|w| n.contains(w)) || id.starts_with("http:") || id.starts_with("https:") || id.ends_with(".url") || id.ends_with(".txt") || id.ends_with(".pdf") || id.ends_with(".chm") || id.ends_with(".html")
}

unsafe fn shell_name(it: &IShellItem, kind: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    let p = it.GetDisplayName(kind).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s.filter(|s| !s.trim().is_empty())
}

fn enumerate() -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.push(("MacDock Settings".to_string(), "macdock:settings".to_string()));
    unsafe {
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(w!("shell:AppsFolder"), None) else { return out };
        let Ok(en) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut one = [None];
            let mut n = 0u32;
            if en.Next(&mut one, Some(&mut n)).is_err() || n == 0 {
                break;
            }
            let Some(it) = one[0].take() else { break };
            let (Some(name), Some(id)) = (shell_name(&it, SIGDN_NORMALDISPLAY), shell_name(&it, SIGDN_PARENTRELATIVEPARSING)) else { continue };
            if !junk(&name, &id) {
                out.push((name, format!("shell:AppsFolder\\{id}")));
            }
        }
    }
    out.sort_by_key(|a| a.0.to_lowercase());
    out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    out
}

/// Re-read the app list (keeping icons already loaded), then fill in missing icons a few at a time.
/// Test aid: fill the catalog with the real app list and icons, on this thread.
#[cfg(test)]
pub(crate) fn load_now() {
    let apps = enumerate().into_iter().map(|(name, spec)| {
        let pm = if spec == "macdock:settings" {
            std::env::current_exe().ok().and_then(|e| crate::shell::icon(&e.to_string_lossy(), ICON_PX))
        } else {
            crate::shell::icon(&spec, ICON_PX)
        };
        App { icon: pm.map(Arc::new), name, spec }
    }).collect();
    *CATALOG.lock().unwrap() = apps;
}

pub(crate) fn refresh() {
    if LOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    let ok = std::thread::Builder::new().name("launchpad".into()).spawn(|| {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let list = enumerate();
        {
            let mut cat = CATALOG.lock().unwrap();
            let old: std::collections::HashMap<String, Option<Arc<Pixmap>>> = cat.drain(..).map(|a| (a.spec, a.icon)).collect();
            *cat = list.into_iter().map(|(name, spec)| App { icon: old.get(&spec).cloned().flatten(), name, spec }).collect();
        }
        notify();
        let todo: Vec<String> = CATALOG.lock().unwrap().iter().filter(|a| a.icon.is_none()).map(|a| a.spec.clone()).collect();
        for (i, spec) in todo.iter().enumerate() {
            let pm = if spec == "macdock:settings" {
                std::env::current_exe().ok().and_then(|e| crate::shell::icon(&e.to_string_lossy(), ICON_PX))
            } else {
                crate::shell::icon(spec, ICON_PX)
            };
            if let Some(pm) = pm {
                let pm = Arc::new(pm);
                if let Some(a) = CATALOG.lock().unwrap().iter_mut().find(|a| &a.spec == spec) {
                    a.icon = Some(pm);
                }
            }
            if i % 14 == 13 {
                notify();
            }
        }
        notify();
        LOADING.store(false, Ordering::Release);
    });
    if ok.is_err() {
        LOADING.store(false, Ordering::Release);
    }
}

const COLS: usize = 7;
const ROWS: usize = 5;
const PER_PAGE: usize = COLS * ROWS;
const SEARCH_W: f32 = 240.0;
const SEARCH_H: f32 = 30.0;
const ID_APP: u32 = 1; // ids 1..=PER_PAGE: the cells of the page shown
const ID_DOT: u32 = 100; // 100.. the page dots
const ID_SEARCH: u32 = 90; // the search field: a click there must not count as the background
const ID_SETTINGS: u32 = 95; // the settings button

pub struct Launchpad {
    apps: Vec<App>,
    query: String,
    /// indices into `apps` that match the query, in display order
    shown: Vec<usize>,
    page: usize,
    /// keyboard selection, an index into `shown`
    sel: usize,
    /// a scroll in progress: wheel notches add up until a page turn
    wheel: i32,
    /// where the pages are, in pages (2.4 = 40% of the way from page 2 to page 3); eases to `page`
    pos: f32,
    /// a touchpad swipe in progress: the pages follow the fingers until they stop
    swipe: Option<Swipe>,
    /// the screen in DIPs
    screen: (f32, f32),
    last_tick: Option<std::time::Instant>,
    motion: bool,
    /// the search field takes the typing (clicked or typed into); `focus_t` follows it, 0 -> 1
    focus: bool,
    focus_t: f32,
    /// the caret shows solid from here, then blinks
    blink: std::time::Instant,
    /// when the results last changed, for their fade-in
    changed: Option<std::time::Instant>,
}

/// Wheel units of touchpad scrolling that move one whole page.
const SWIPE_UNITS: f32 = 700.0;
/// No touchpad input for this long means the fingers have lifted.
const SWIPE_END_MS: u128 = 90;

struct Swipe {
    /// the page the swipe began on
    start: usize,
    /// where the fingers alone would put the pages, before the edges hold them back
    raw: f32,
    at: std::time::Instant,
    /// pages per second, for a flick
    vel: f32,
}

impl Launchpad {
    pub fn new(motion: bool) -> Launchpad {
        let mut l = Launchpad::empty(Vec::new(), motion);
        l.pick_up();
        refresh();
        l
    }

    fn empty(apps: Vec<App>, motion: bool) -> Launchpad {
        Launchpad {
            apps, query: String::new(), shown: Vec::new(), page: 0, sel: 0, wheel: 0, pos: 0.0, swipe: None,
            screen: (1536.0, 864.0), last_tick: None, motion, focus: false, focus_t: 0.0,
            blink: std::time::Instant::now(), changed: None,
        }
    }

    fn pick_up(&mut self) {
        self.apps = CATALOG.lock().map(|c| c.clone()).unwrap_or_default();
        self.filter();
    }

    fn filter(&mut self) {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            self.shown = (0..self.apps.len()).collect();
        } else {
            // names starting with the query first, then words starting with it, then anywhere in the name
            let mut hits: Vec<(u8, usize)> = self
                .apps
                .iter()
                .enumerate()
                .filter_map(|(i, a)| rank(&a.name.to_lowercase(), &q).map(|r| (r, i)))
                .collect();
            hits.sort();
            self.shown = hits.into_iter().map(|h| h.1).collect();
        }
        let pages = self.pages();
        self.page = self.page.min(pages - 1);
        self.sel = self.sel.min(self.shown.len().saturating_sub(1));
        self.pos = self.page as f32;
        self.swipe = None;
    }

    fn pages(&self) -> usize {
        self.shown.len().div_ceil(PER_PAGE).max(1)
    }

    fn app_at(&self, id: u32) -> Option<&App> {
        let cell = id.checked_sub(ID_APP)? as usize;
        if cell >= PER_PAGE {
            return None;
        }
        self.shown.get(self.page * PER_PAGE + cell).map(|&i| &self.apps[i])
    }

    fn set_query(&mut self, q: String) {
        self.query = q;
        self.focus = true;
        self.blink = std::time::Instant::now();
        if self.motion {
            self.changed = Some(std::time::Instant::now());
        }
        self.page = 0;
        self.sel = 0;
        self.filter();
    }

    /// Show page `p`, sliding over from the one shown now.
    fn go(&mut self, p: usize) {
        let p = p.min(self.pages() - 1);
        if p == self.page {
            return;
        }
        self.page = p;
        if self.motion {
            self.last_tick = None;
        } else {
            self.pos = p as f32;
        }
    }

    /// Touchpad scroll (`d` in wheel units, positive = towards the next page): the pages follow it.
    fn swipe_by(&mut self, d: f32) {
        let last = (self.pages() - 1) as f32;
        let now = std::time::Instant::now();
        let fresh = self.swipe.is_none();
        let sw = self.swipe.get_or_insert(Swipe { start: self.page, raw: self.pos, at: now, vel: 0.0 });
        let dt = (now - sw.at).as_secs_f32().max(0.004);
        sw.at = now;
        let step = d / SWIPE_UNITS;
        if !fresh {
            // pages per second, smoothed; the first step has no time before it to measure against
            sw.vel = sw.vel * 0.6 + step / dt * 0.4;
        }
        // one page per swipe, as on macOS: the fingers can pull up to the neighbour and no further
        let lo = (sw.start as f32 - 1.0).max(0.0);
        let hi = (sw.start as f32 + 1.0).min(last);
        sw.raw = (sw.raw + step).clamp(lo - 0.5, hi + 0.5);
        // past the first or last page it gives a little, like a rubber band
        let over = if sw.raw < lo { sw.raw - lo } else if sw.raw > hi { sw.raw - hi } else { 0.0 };
        self.pos = sw.raw.clamp(lo, hi) + over * 0.3;
        self.last_tick = None;
    }

    /// The fingers stopped: settle on the page they pulled to, or the next one if they flicked.
    fn release(&mut self) {
        let Some(sw) = self.swipe.take() else { return };
        let last = self.pages() as i32 - 1;
        let moved = self.pos - sw.start as f32;
        let dir = if moved > 0.12 || sw.vel > 1.2 {
            1
        } else if moved < -0.12 || sw.vel < -1.2 {
            -1
        } else {
            0
        };
        let p = (sw.start as i32 + dir).clamp(0, last) as usize;
        if p != self.page {
            self.sel = p * PER_PAGE;
        }
        self.page = p;
    }

    fn turn(&mut self, d: i32) {
        let p = (self.page as i32 + d).clamp(0, self.pages() as i32 - 1) as usize;
        if p != self.page {
            self.go(p);
            self.sel = p * PER_PAGE;
        }
    }

    fn move_sel(&mut self, d: i32) {
        if self.shown.is_empty() {
            return;
        }
        let s = (self.sel as i32 + d).clamp(0, self.shown.len() as i32 - 1) as usize;
        self.sel = s;
        self.go(s / PER_PAGE);
    }

    fn tick_pages(&mut self, now: std::time::Instant) {
        // a touchpad sends nothing when the fingers lift, so a short silence ends the swipe
        if self.swipe.as_ref().is_some_and(|s| s.at.elapsed().as_millis() > SWIPE_END_MS) {
            self.release();
        }
        if self.swipe.is_some() {
            return;
        }
        let target = self.page as f32;
        if (self.pos - target).abs() <= 0.0005 {
            self.pos = target;
            self.last_tick = Some(now);
            return;
        }
        let dt = self.last_tick.map_or(0.016, |t| (now - t).as_secs_f32()).min(0.05);
        self.last_tick = Some(now);
        // ease out: about 300 ms to settle
        self.pos = target + (self.pos - target) * (-dt * 14.0).exp();
    }

    /// The grid on screen: left edge, top, cell size, icon size.
    fn grid(&self) -> (f32, f32, f32, f32, f32) {
        let (w, h) = self.screen;
        let gw = (w * 0.74).min(1240.0);
        let cell_w = gw / COLS as f32;
        let top = 96.0;
        let grid_h = (h - top - 96.0).max(300.0);
        let cell_h = grid_h / ROWS as f32;
        let icon = (cell_w * 0.5).min(cell_h * 0.62).clamp(48.0, 104.0);
        ((w - gw) / 2.0, top, cell_w, cell_h, icon)
    }

    /// One page of icons, shifted `dx` DIPs sideways; only the page shown gets clickable spots.
    fn draw_page(&self, ui: &mut Ui, page: usize, dx: f32, live: bool) {
        let (x0, top, cell_w, cell_h, icon) = self.grid();
        let start = page * PER_PAGE;
        for cell in 0..PER_PAGE {
            let Some(&ai) = self.shown.get(start + cell) else { break };
            let (r, c) = (cell / COLS, cell % COLS);
            let x = x0 + c as f32 * cell_w + dx;
            let y = top + r as f32 * cell_h;
            if x + cell_w < 0.0 || x > self.screen.0 {
                continue;
            }
            let ix = x + (cell_w - icon) / 2.0;
            let iy = y + (cell_h - icon - 26.0) / 2.0;
            let id = ID_APP + cell as u32;
            if live {
                ui.hit(id, ix - 10.0, iy - 6.0, icon + 20.0, icon + 34.0);
            }
            let selected = live && !self.query.is_empty() && start + cell == self.sel;
            if selected {
                ui.fill(ix - 10.0, iy - 8.0, icon + 20.0, icon + 20.0, 22.0, white(0x33));
            }
            let a = &self.apps[ai];
            match &a.icon {
                Some(img) => ui.image(img, ix, iy, icon, icon),
                None => ui.fill(ix + icon * 0.08, iy + icon * 0.08, icon * 0.84, icon * 0.84, icon * 0.22, white(0x1F)),
            }
            if live && ui.pressed == Some(id) {
                ui.fill(ix + icon * 0.06, iy + icon * 0.06, icon * 0.88, icon * 0.88, icon * 0.22, (0, 0, 0, 0x55));
            }
            let max = cell_w - 12.0;
            let tw = ui.text_w(&a.name, 12.5, 500).min(max);
            let (tx, ty) = (x + (cell_w - tw) / 2.0, iy + icon + 8.0);
            ui.text(tx, ty + 1.0, &a.name, 12.5, 500, (0, 0, 0, 0x70), max); // shadow keeps it readable on bright wallpapers
            ui.text(tx, ty, &a.name, 12.5, 500, white(0xF2), max);
        }
    }
}

impl Content for Launchpad {
    fn animating(&self) -> bool {
        self.swipe.is_some()
            || (self.pos - self.page as f32).abs() > 0.0005
            || (self.focus_t - if self.focus { 1.0 } else { 0.0 }).abs() > 0.001
            || self.changed.is_some()
    }

    fn period_ms(&self) -> u32 {
        BLINK_MS as u32 / 2 // often enough that the caret's blink lands on time
    }

    fn escape(&mut self) -> Cmd {
        if !self.query.is_empty() {
            self.set_query(String::new());
            Cmd::None
        } else if self.focus {
            self.focus = false;
            Cmd::None
        } else {
            Cmd::Close
        }
    }

    fn motion(&self) -> bool {
        self.motion
    }

    fn tick(&mut self) {
        let now = std::time::Instant::now();
        let dt = self.last_tick.map_or(0.016, |t| (now - t).as_secs_f32()).min(0.05);
        let goal = if self.focus { 1.0 } else { 0.0 };
        if !self.motion {
            self.focus_t = goal;
        } else if (self.focus_t - goal).abs() > 0.001 {
            self.focus_t = goal + (self.focus_t - goal) * (-dt * 16.0).exp();
        } else {
            self.focus_t = goal;
        }
        if self.changed.is_some_and(|c| c.elapsed().as_secs_f32() * 1000.0 > RESULTS_IN_MS) {
            self.changed = None;
        }
        self.tick_pages(now);
    }

    fn width(&self) -> f32 {
        self.screen.0
    }

    fn stage(&mut self, w: f32, h: f32) -> bool {
        self.screen = (w, h);
        true
    }

    fn opened(&mut self, hwnd: isize) {
        NOTIFY.store(hwnd, Ordering::Relaxed);
    }

    fn closing(&mut self) {
        NOTIFY.store(0, Ordering::Relaxed);
    }

    fn data(&mut self) {
        self.pick_up();
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let (w, h) = self.screen;
        // search field, small and centred at the top
        let sx = (w - SEARCH_W) / 2.0;
        let sy = 34.0;
        let hov = ui.hit(ID_SEARCH, sx, sy, SEARCH_W, SEARCH_H);
        // empty and idle, the glass and the label sit in the middle; focused, the label slides to the left edge
        // where the typing starts and the field brightens
        let f = ease_out(self.focus_t);
        let lit = if hov && !self.focus { 0x0E } else { 0 };
        ui.fill(sx, sy, SEARCH_W, SEARCH_H, SEARCH_H / 2.0, white(0x2E + (0x14 as f32 * f) as u8 + lit));
        ui.outline(sx, sy, SEARCH_W, SEARCH_H, SEARCH_H / 2.0, 1.0, white(0x2E + (0x30 as f32 * f) as u8 + lit));
        let my = sy + SEARCH_H / 2.0;
        let caret_on = self.focus && (self.blink.elapsed().as_millis() / BLINK_MS) % 2 == 0;
        let caret = (ACCENT.0, ACCENT.1, ACCENT.2, 255);
        if self.query.is_empty() {
            let label = t("Search");
            let lw = ui.text_w(label, 13.0, 400);
            let (centre, left) = ((w - lw - 20.0) / 2.0, sx + 10.0);
            let gx = centre + (left - centre) * f;
            let a = (0xB3 as f32 - 0x33 as f32 * f) as u8;
            ui.glyph(gx + 6.0, my, '\u{E721}', 11.5, white(0xB3));
            ui.text_mid(gx + 20.0, my, label, 13.0, 400, white(a), 0.0);
            if caret_on {
                ui.fill(gx + 19.0, my - 7.5, 1.5, 15.0, 0.0, caret);
            }
        } else {
            ui.glyph(sx + 16.0, my, '\u{E721}', 11.5, white(0xB3));
            let tx = sx + 30.0;
            let end = ui.text_mid(tx, my, &self.query, 13.0, 400, white(0xF2), SEARCH_W - 44.0);
            if caret_on {
                ui.fill(tx + end.min(SEARCH_W - 44.0) + 1.0, my - 7.5, 1.5, 15.0, 0.0, caret);
            }
        }

        // Settings button in top right
        let btn_sz = 30.0;
        let btn_x = (w - 48.0).max(sx + SEARCH_W + 20.0);
        let btn_y = sy + (SEARCH_H - btn_sz) / 2.0;
        let hov_set = ui.hit(ID_SETTINGS, btn_x, btn_y, btn_sz, btn_sz);
        let bg_set = white(if ui.pressed == Some(ID_SETTINGS) { 0x44 } else if hov_set { 0x2D } else { 0x1A });
        ui.fill(btn_x, btn_y, btn_sz, btn_sz, btn_sz / 2.0, bg_set);
        ui.outline(btn_x, btn_y, btn_sz, btn_sz, btn_sz / 2.0, 1.0, white(if hov_set { 0x3F } else { 0x24 }));
        ui.glyph(btn_x + btn_sz / 2.0, btn_y + btn_sz / 2.0, '\u{E713}', 13.0, white(if hov_set { 0xFF } else { 0xD0 }));

        let (_, top, _, cell_h, _) = self.grid();
        let grid_h = cell_h * ROWS as f32;
        if self.shown.is_empty() {
            let msg = t(if self.apps.is_empty() { "Loading apps…" } else { "No Results" });
            let mw = ui.text_w(msg, 20.0, 500);
            ui.text_mid((w - mw) / 2.0, top + grid_h / 2.0, msg, 20.0, 500, white(0xB3), 0.0);
        }
        // every page within reach of `pos`, side by side; only the settled one takes clicks
        let pages_n = self.pages();
        let settled = self.swipe.is_none() && (self.pos - self.page as f32).abs() < 0.02;
        let first = self.pos.floor().max(0.0) as usize;
        let fresh = self.changed.map(|c| ease_out(c.elapsed().as_secs_f32() * 1000.0 / RESULTS_IN_MS));
        if let Some(e) = fresh {
            // new results: the grid fades in and settles from a touch smaller
            ui.layer(0.0, top - 12.0, w, grid_h + 24.0, 0.25 + 0.75 * e, 0.97 + 0.03 * e, [0.0; 4], |ui| {
                self.draw_page(ui, self.page, 0.0, true);
            });
        } else {
            for p in first..(first + 2).min(pages_n) {
                let dx = (p as f32 - self.pos) * w;
                if dx.abs() < w {
                    self.draw_page(ui, p, dx, settled && p == self.page);
                }
            }
        }

        // page dots
        let pages = self.pages();
        let dy = top + grid_h + 30.0;
        if pages > 1 {
            let gap = 18.0;
            let x0 = (w - gap * (pages - 1) as f32) / 2.0;
            for p in 0..pages {
                let cx = x0 + gap * p as f32;
                let hov = ui.hit(ID_DOT + p as u32, cx - 8.0, dy - 8.0, 16.0, 16.0);
                let a = if p == self.page { 0xF2 } else if hov { 0x99 } else { 0x59 };
                ui.circle(cx, dy, 4.0, white(a));
            }
        }
        h
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        if id == ID_SETTINGS {
            return Cmd::Settings;
        }
        if id >= ID_DOT {
            self.go((id - ID_DOT) as usize);
            return Cmd::None;
        }
        if id == ID_SEARCH {
            self.focus = true;
            self.blink = std::time::Instant::now();
            return Cmd::None;
        }
        match self.app_at(id) {
            Some(a) => {
                if a.spec == "macdock:settings" {
                    Cmd::Settings
                } else {
                    Cmd::Launch(a.spec.clone())
                }
            }
            None => Cmd::None,
        }
    }

    fn right_click(&mut self, id: u32) -> Cmd {
        if id == ID_SETTINGS {
            return Cmd::Settings;
        }
        match self.app_at(id) {
            Some(a) => {
                if a.spec == "macdock:settings" {
                    Cmd::Settings
                } else {
                    Cmd::Pin(a.spec.clone())
                }
            }
            None => Cmd::None,
        }
    }

    fn wheel(&mut self, delta: i32) {
        // a touchpad scrolls in small steps; a mouse wheel in whole notches of 120
        if self.motion && (delta % 120 != 0 || self.swipe.is_some()) {
            self.swipe_by(-delta as f32);
            return;
        }
        self.wheel += delta;
        if self.wheel.abs() >= 120 {
            self.turn(if self.wheel < 0 { 1 } else { -1 });
            self.wheel = 0;
        }
    }

    fn hwheel(&mut self, delta: i32) {
        // fingers moving left scroll right, which brings in the next page
        if self.motion {
            self.swipe_by(delta as f32);
        } else {
            self.wheel(-delta);
        }
    }

    fn char(&mut self, c: char) -> Cmd {
        match c {
            '\u{8}' => {
                let mut q = self.query.clone();
                q.pop();
                self.set_query(q);
            }
            '\r' => {
                if let Some(&i) = self.shown.get(self.sel) {
                    if self.apps[i].spec == "macdock:settings" {
                        return Cmd::Settings;
                    }
                    return Cmd::Launch(self.apps[i].spec.clone());
                }
            }
            c if !c.is_control() && self.query.chars().count() < 40 => {
                let q = format!("{}{c}", self.query);
                self.set_query(q);
            }
            _ => {}
        }
        Cmd::None
    }

    fn key(&mut self, vk: u32) -> Cmd {
        match vk {
            0x25 => self.move_sel(-1),          // left
            0x27 => self.move_sel(1),           // right
            0x26 => self.move_sel(-(COLS as i32)), // up
            0x28 => self.move_sel(COLS as i32), // down
            0x21 => self.turn(-1),              // page up
            0x22 => self.turn(1),               // page down
            _ => {}
        }
        Cmd::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(names: &[&str]) -> Launchpad {
        let apps = names.iter().map(|n| App { name: n.to_string(), spec: format!("shell:AppsFolder\\{n}"), icon: None }).collect();
        let mut l = Launchpad::empty(apps, false);
        l.filter();
        l
    }

    #[test]
    fn search_ranks_prefix_first() {
        let mut l = pad(&["Microsoft Edge", "Edge Canary", "Paint", "Notepad"]);
        l.set_query("edge".into());
        let names: Vec<&str> = l.shown.iter().map(|&i| l.apps[i].name.as_str()).collect();
        assert_eq!(names, ["Edge Canary", "Microsoft Edge"]);
        l.set_query("pad".into());
        assert_eq!(l.shown.len(), 1);
    }

    #[test]
    fn settings_button_and_search_open_settings() {
        let mut l = Launchpad::empty(vec![
            App { name: "MacDock Settings".into(), spec: "macdock:settings".into(), icon: None },
            App { name: "Calculator".into(), spec: "shell:AppsFolder\\calc".into(), icon: None },
        ], false);
        l.filter();

        // 1. Click settings button directly
        assert!(matches!(l.click(ID_SETTINGS, 0.0), Cmd::Settings));
        assert!(matches!(l.right_click(ID_SETTINGS), Cmd::Settings));

        // 2. Search "settings" and launch via Enter
        l.set_query("settings".into());
        assert_eq!(l.shown.len(), 1);
        assert!(matches!(l.char('\r'), Cmd::Settings));

        // 3. Click app cell
        assert!(matches!(l.click(ID_APP, 0.0), Cmd::Settings));
    }

    #[test]
    fn junk_is_skipped() {
        assert!(junk("Uninstall Foo", "x"));
        assert!(junk("Foo website", "x"));
        assert!(junk("Foo", "https://foo"));
        assert!(!junk("Visual Studio Code", "Microsoft.VisualStudioCode"));
    }

    #[test]
    fn paging_and_selection() {
        let names: Vec<String> = (0..80).map(|i| format!("App {i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut l = pad(&refs);
        assert_eq!(l.pages(), 3);
        l.turn(1);
        assert_eq!(l.page, 1);
        l.move_sel(PER_PAGE as i32);
        assert_eq!(l.page, 2);
        l.turn(5);
        assert_eq!(l.page, 2);
    }

    fn swipe_pad() -> Launchpad {
        let names: Vec<String> = (0..80).map(|i| format!("App {i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut l = pad(&refs);
        l.motion = true;
        l
    }

    /// Lift the fingers and let the pages settle.
    fn settle(l: &mut Launchpad) {
        if let Some(s) = l.swipe.as_mut() {
            s.at -= std::time::Duration::from_millis(200);
        }
        for _ in 0..200 {
            l.tick();
            l.last_tick = l.last_tick.map(|t| t - std::time::Duration::from_millis(16));
        }
    }

    #[test]
    fn touchpad_swipe_follows_and_snaps() {
        let mut l = swipe_pad();
        // a slow drag halfway: the pages sit where the fingers are
        for _ in 0..5 {
            l.wheel(-70);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!((l.pos - 0.5).abs() < 0.01, "pos {}", l.pos);
        assert!(l.animating());
        settle(&mut l);
        assert_eq!(l.page, 1);
        assert_eq!(l.pos, 1.0);

        // a small slow pull springs back
        for _ in 0..2 {
            l.hwheel(-20);
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        settle(&mut l);
        assert_eq!(l.page, 1);

        // a long drag still turns only one page
        for _ in 0..30 {
            l.hwheel(120 + 1);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(l.pos < 2.2, "pos {}", l.pos); // the last page: only the rubber band past it
        settle(&mut l);
        assert_eq!(l.page, 2);

        // past the last page it gives a little, then comes back
        for _ in 0..4 {
            l.hwheel(100);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(l.pos > 2.0 && l.pos < 2.2, "pos {}", l.pos);
        settle(&mut l);
        assert_eq!((l.page, l.pos), (2, 2.0));

        // a mouse wheel notch still turns a page at once
        l.wheel(120);
        assert_eq!(l.page, 1);
    }

    /// Real app list rendered to target/launchpad.png (cargo test launchpad_snapshot -- --ignored).
    #[test]
    #[ignore]
    fn launchpad_snapshot() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let apps: Vec<App> = enumerate()
            .into_iter()
            .map(|(name, spec)| App { icon: crate::shell::icon(&spec, ICON_PX).map(Arc::new), name, spec })
            .collect();
        assert!(apps.len() > 10, "only {} apps", apps.len());
        let mut l = Launchpad::empty(apps, false);
        l.filter();
        let (w, h, px) = crate::flyout::snapshot_stage(&mut l, 1536.0, 864.0, 1.25);
        let path = format!(r"{}\target\launchpad.png", env!("CARGO_MANIFEST_DIR"));
        assert!(crate::gfx::save_png(&path, w, h, &px));
    }
}
