//! Spotlight, as on macOS 26: a Liquid Glass capsule in the upper part of the screen that grows downward
//! with the results as you type. Apps first (the same list the Apps window shows), then a web search.
//! Opens from the menu bar's magnifier or Alt+Space.

use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::flyout::{white, Cmd, Content, Ui, ACCENT, MARGIN};
use crate::i18n::t;
use crate::launchpad::{ease_out, refresh, App, BLINK_MS, CATALOG, NOTIFY, RESULTS_IN_MS};

const W: f32 = 660.0;
const PAD: (f32, f32) = (16.0, 0.0);
/// the capsule: its height is twice the glass corner radius, so the ends are round
const BAR: f32 = 52.0;
const ROW: f32 = 40.0;
const MAX_APPS: usize = 7;
const LIST_TOP: f32 = BAR + 7.0;
const LIST_PAD: f32 = 9.0;
/// the tallest it gets: every app row plus the web row
const MAX_H: f32 = LIST_TOP + (MAX_APPS + 1) as f32 * ROW + LIST_PAD;
/// where the capsule's top sits, as a fraction of the screen height (macOS keeps it above the middle)
const TOP: f32 = 0.22;

const ID_FIELD: u32 = 1;
const ID_ROW: u32 = 10;

pub struct Spotlight {
    apps: Vec<App>,
    query: String,
    /// the apps matching, best first (indexes into `apps`)
    hits: Vec<usize>,
    sel: usize,
    /// the content height on screen, easing to `want_h()`
    h: f32,
    blink: Instant,
    changed: Option<Instant>,
    last_tick: Option<Instant>,
    motion: bool,
}

/// %-encode a search for a URL.
fn encode(q: &str) -> String {
    let mut out = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Spotlight {
    pub fn new(motion: bool) -> Spotlight {
        let mut s = Spotlight::empty(Vec::new(), motion);
        s.pick_up();
        refresh();
        s
    }

    fn empty(apps: Vec<App>, motion: bool) -> Spotlight {
        Spotlight { apps, query: String::new(), hits: Vec::new(), sel: 0, h: BAR, blink: Instant::now(), changed: None, last_tick: None, motion }
    }

    fn pick_up(&mut self) {
        self.apps = CATALOG.lock().map(|c| c.clone()).unwrap_or_default();
        self.filter();
    }

    fn filter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.hits.clear();
        if !q.is_empty() {
            let mut hits: Vec<(u8, usize, usize)> =
                self.apps.iter().enumerate().filter_map(|(i, a)| a.search_rank(&q).map(|r| (r, a.name.len(), i))).collect();
            hits.sort();
            self.hits = hits.into_iter().map(|h| h.2).take(MAX_APPS).collect();
        }
        self.sel = self.sel.min(self.rows().saturating_sub(1));
    }

    /// Rows under the field: the apps, then the web search (none while the field is empty).
    fn rows(&self) -> usize {
        if self.query.trim().is_empty() { 0 } else { self.hits.len() + 1 }
    }

    fn want_h(&self) -> f32 {
        match self.rows() {
            0 => BAR,
            n => LIST_TOP + n as f32 * ROW + LIST_PAD,
        }
    }

    fn set_query(&mut self, q: String) {
        self.query = q;
        self.sel = 0;
        self.blink = Instant::now();
        self.filter();
        if self.motion {
            self.changed = Some(Instant::now());
        } else {
            self.h = self.want_h();
        }
    }

    fn open(&self, row: usize) -> Cmd {
        match self.hits.get(row) {
            Some(&ai) => {
                let spec = self.apps[ai].spec.clone();
                if spec == "macdock:settings" {
                    return Cmd::Settings;
                }
                crate::apps::remember(&spec);
                Cmd::Launch(spec)
            }
            None if row == self.hits.len() && !self.query.trim().is_empty() => {
                Cmd::Launch(format!("https://www.google.com/search?q={}", encode(self.query.trim())))
            }
            None => Cmd::None,
        }
    }

    fn draw_field(&mut self, ui: &mut Ui) {
        let cw = ui.w;
        let my = BAR / 2.0;
        // the magnifier
        let c = white(0xC8);
        ui.outline(2.0, my - 9.0, 14.0, 14.0, 7.0, 1.9, c);
        ui.line(13.6, my + 2.6, 18.5, my + 7.5, 2.1, c);
        ui.hit(ID_FIELD, 0.0, 0.0, cw, BAR);
        let tx = 30.0;
        let max = cw - tx - 8.0;
        let caret_on = (self.blink.elapsed().as_millis() / BLINK_MS) % 2 == 0;
        let caret = (ACCENT.0, ACCENT.1, ACCENT.2, 255);
        let end = if self.query.is_empty() {
            ui.text_mid(tx, my, t("Spotlight Search"), 21.0, 400, white(0x8C), max);
            0.0
        } else {
            ui.text_mid(tx, my, &self.query, 21.0, 400, white(0xF5), max)
        };
        if caret_on {
            ui.fill(tx + end.min(max) + if end > 0.0 { 1.0 } else { -1.5 }, my - 11.0, 1.6, 22.0, 0.0, caret);
        }
    }

    fn draw_rows(&mut self, ui: &mut Ui) {
        let cw = ui.w;
        let n = self.rows();
        if n == 0 || self.h <= BAR + 1.0 {
            return;
        }
        let a = match self.changed {
            Some(c) => ease_out(c.elapsed().as_secs_f32() * 1000.0 / RESULTS_IN_MS),
            None => 1.0,
        };
        let view = self.h - BAR;
        let (hits, sel, q) = (self.hits.clone(), self.sel, self.query.trim().to_string());
        let apps = &self.apps;
        ui.layer(-PAD.0, BAR, cw + 2.0 * PAD.0, view, 1.0, 1.0, [0.0, 0.0, 0.0, 0.0], |ui| {
            ui.line(0.0, BAR + 0.5, cw, BAR + 0.5, 1.0, white(0x1F));
            let rise = (1.0 - a) * 6.0;
            for r in 0..n {
                let y = LIST_TOP + r as f32 * ROW + rise;
                let id = ID_ROW + r as u32;
                let hov = ui.hit(id, -8.0, y, cw + 16.0, ROW);
                let on = r == sel;
                if on {
                    ui.fill(-8.0, y + 1.0, cw + 16.0, ROW - 2.0, 11.0, (ACCENT.0, ACCENT.1, ACCENT.2, (0xC8 as f32 * a) as u8));
                } else if hov {
                    ui.fill(-8.0, y + 1.0, cw + 16.0, ROW - 2.0, 11.0, white((0x1A as f32 * a) as u8));
                }
                let ty = y + ROW / 2.0;
                let fade = |c: u8| (c as f32 * a) as u8;
                let (name, kind) = match hits.get(r) {
                    Some(&ai) => {
                        let app = &apps[ai];
                        match &app.icon {
                            Some(img) => ui.image(img, 0.0, ty - 14.0, 28.0, 28.0),
                            None => ui.fill(2.0, ty - 12.0, 24.0, 24.0, 6.0, white(0x22)),
                        }
                        (app.display_name().to_owned(), t("Application"))
                    }
                    None => {
                        ui.glyph(14.0, ty, '\u{E774}', 17.0, white(fade(0xE0)));
                        (format!("{} \u{201C}{q}\u{201D}", t("Search the Web for")), "")
                    }
                };
                let kw = if kind.is_empty() { 0.0 } else { ui.text_w(kind, 12.0, 400) + 12.0 };
                ui.text_mid(40.0, ty, &name, 14.5, if on { 500 } else { 400 }, white(fade(0xF5)), cw - 48.0 - kw);
                if !kind.is_empty() {
                    ui.text_mid(cw - kw + 4.0, ty, kind, 12.0, 400, white(fade(if on { 0xE0 } else { 0x8C })), 0.0);
                }
            }
        });
    }
}

impl Content for Spotlight {
    fn width(&self) -> f32 {
        W + 2.0 * MARGIN
    }

    fn padding(&self) -> (f32, f32) {
        PAD
    }

    fn glass(&self) -> Option<f32> {
        Some(self.h)
    }

    fn glass_top(&self) -> Option<(f32, f32)> {
        Some((TOP, MAX_H))
    }

    fn motion(&self) -> bool {
        self.motion
    }

    fn animating(&self) -> bool {
        (self.h - self.want_h()).abs() > 0.25 || self.changed.is_some()
    }

    fn period_ms(&self) -> u32 {
        BLINK_MS as u32 / 2
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = self.last_tick.map_or(0.016, |t| (now - t).as_secs_f32()).min(0.05);
        self.last_tick = Some(now);
        let goal = self.want_h();
        // a quick spring-like ease, the way the capsule opens into the list on macOS
        self.h = if !self.motion || (self.h - goal).abs() <= 0.25 { goal } else { goal + (self.h - goal) * (-dt * 18.0).exp() };
        if self.changed.is_some_and(|c| c.elapsed().as_secs_f32() * 1000.0 > RESULTS_IN_MS) {
            self.changed = None;
        }
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
        self.draw_field(ui);
        self.draw_rows(ui);
        self.h
    }

    fn hover(&mut self, id: Option<u32>) {
        if let Some(r) = id.and_then(|i| i.checked_sub(ID_ROW)) {
            self.sel = r as usize;
        }
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id.checked_sub(ID_ROW) {
            Some(r) => self.open(r as usize),
            None => {
                self.blink = Instant::now();
                Cmd::None
            }
        }
    }

    fn right_click(&mut self, id: u32) -> Cmd {
        match id.checked_sub(ID_ROW).and_then(|r| self.hits.get(r as usize)) {
            Some(&ai) => {
                if self.apps[ai].spec == "macdock:settings" {
                    Cmd::Settings
                } else {
                    Cmd::Pin(self.apps[ai].spec.clone())
                }
            }
            None => Cmd::None,
        }
    }

    fn char(&mut self, c: char) -> Cmd {
        match c {
            '\u{8}' => {
                let mut q = self.query.clone();
                q.pop();
                self.set_query(q);
            }
            '\r' => return self.open(self.sel),
            '\t' => {}
            c if !c.is_control() && self.query.chars().count() < 60 => {
                let q = format!("{}{c}", self.query);
                self.set_query(q);
            }
            _ => {}
        }
        Cmd::None
    }

    fn key(&mut self, vk: u32) -> Cmd {
        let n = self.rows();
        match vk {
            0x26 if n > 0 => self.sel = (self.sel + n - 1) % n,
            0x28 if n > 0 => self.sel = (self.sel + 1) % n,
            _ => {}
        }
        Cmd::None
    }

    fn escape(&mut self) -> Cmd {
        if self.query.is_empty() {
            Cmd::Close
        } else {
            self.set_query(String::new());
            Cmd::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spot(names: &[&str]) -> Spotlight {
        let list = names.iter().map(|n| App { name: n.to_string(), spec: format!("shell:AppsFolder\\{n}"), icon: None }).collect();
        let mut s = Spotlight::empty(list, false);
        s.filter();
        s
    }

    #[test]
    fn results_rank_and_end_with_the_web() {
        let mut s = spot(&["Visual Studio Code", "Code Writer", "Calculator", "Chrome"]);
        assert_eq!(s.rows(), 0);
        assert_eq!(s.want_h(), BAR);
        for c in "code".chars() {
            s.char(c);
        }
        let names: Vec<&str> = s.hits.iter().map(|&i| s.apps[i].name.as_str()).collect();
        assert_eq!(names, ["Code Writer", "Visual Studio Code"]);
        assert_eq!(s.rows(), 3);
        assert_eq!(s.h, s.want_h()); // no motion: it jumps
        s.key(0x26); // up from the first wraps to the web row
        match s.char('\r') {
            Cmd::Launch(u) => assert_eq!(u, "https://www.google.com/search?q=code"),
            _ => panic!("enter on the web row should search"),
        }
        assert!(matches!(s.escape(), Cmd::None));
        assert!(s.query.is_empty());
        assert!(matches!(s.escape(), Cmd::Close));
        assert_eq!(encode("a b&c"), "a+b%26c");
    }

    /// Spotlight with a search typed, on the real app list: target/spotlight.png
    #[test]
    #[ignore]
    fn spotlight_snapshot() {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        crate::launchpad::load_now();
        let mut s = Spotlight::empty(Vec::new(), false);
        s.pick_up();
        for c in "co".chars() {
            s.char(c);
        }
        s.blink = Instant::now();
        let (w, h, px) = crate::flyout::snapshot_glass(&mut s, 1.25);
        let path = format!(r"{}\target\spotlight.png", env!("CARGO_MANIFEST_DIR"));
        assert!(crate::gfx::save_png(&path, w, h, &px));
    }
}
