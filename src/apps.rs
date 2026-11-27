//! The Apps window, as macOS 26 replaced Launchpad with it: a floating Liquid Glass window with every installed
//! app, the ones opened lately in the first row, category chips to narrow it down and a search in the title.
//! Scrolls vertically (wheel or two fingers); type to search, arrows and Enter to launch, right-click to keep
//! one in the dock. The app list and icons are the Launchpad's (read once on a worker, kept between opens).

use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::flyout::{white, Cmd, Content, Ui, ACCENT, MARGIN};
use crate::i18n::t;
use crate::launchpad::{ease_out, refresh, App, BLINK_MS, CATALOG, NOTIFY, RESULTS_IN_MS};

/// Categories in the order macOS shows them; an app goes in the first whose words its name has.
const CATS: [(&str, &[&str]); 9] = [
    ("Utilities", &[
        "settings", "control panel", "task manager", "calculator", "notepad", "explorer", "7-zip", "winrar",
        "everything", "powertoys", "security", "defender", "recovery", "disk", "registry", "device manager",
        "remote desktop", "magnifier", "narrator", "keyboard", "character map", "system", "windows tools",
        "nvidia", "amd", "intel", "msi", "asus", "armoury", "predator", "acer", "logitech", "razer", "corsair",
        "driver", "update", "backup", "vpn", "tailscale", "wireguard", "cpu-z", "hwinfo", "afterburner", "rufus",
        "snipping", "clock", "alarm", "print", "scan", "cleanup", "defrag", "odbc", "services", "event viewer",
        "performance", "resource monitor", "sense", "macdock", "bluetooth", "quick assist", "winget", "installer",
    ]),
    ("Productivity & Finance", &[
        "word", "excel", "powerpoint", "outlook", "onenote", "office", "notion", "obsidian", "calendar", "mail",
        "to do", "todo", "acrobat", "pdf", "chrome", "edge", "firefox", "brave", "opera", "vivaldi", "arc", "zen",
        "browser", "claude", "chatgpt", "copilot", "gemini", "onedrive", "dropbox", "drive", "bank", "finance",
        "money", "wallet", "sticky", "teamviewer", "anydesk", "access", "publisher", "project", "visio",
        "libreoffice", "wps", "evernote", "trello", "todoist", "1password", "bitwarden", "keepass",
    ]),
    ("Entertainment", &[
        "spotify", "netflix", "vlc", "media player", "music", "movies", "tv", "youtube", "prime video", "disney",
        "twitch", "potplayer", "mpv", "itunes", "apple music", "deezer", "tidal", "plex", "kodi", "jellyfin",
        "stremio", "podcast", "radio",
    ]),
    ("Information & Reading", &[
        "news", "weather", "maps", "books", "kindle", "reader", "wikipedia", "stocks", "tips", "get help",
        "feedback", "dictionary", "translate", "learn", "duolingo", "manual",
    ]),
    ("Social", &[
        "discord", "whatsapp", "telegram", "teams", "zoom", "slack", "skype", "messenger", "line", "signal",
        "instagram", "facebook", "threads", "phone link", "wechat", "viber", "people", "meet", "webex",
    ]),
    ("Creativity", &[
        "photoshop", "illustrator", "premiere", "after effects", "lightroom", "blender", "figma", "canva",
        "davinci", "resolve", "gimp", "krita", "inkscape", "paint", "photos", "clipchamp", "obs", "audacity",
        "capcut", "affinity", "camera", "fl studio", "ableton", "audition", "media encoder", "handbrake",
        "sound recorder", "voice recorder", "studio", "3d", "draw", "sketch", "cinema", "reaper",
    ]),
    ("Developer Tools", &[
        "code", "visual studio", "git", "terminal", "powershell", "python", "node", "docker", "intellij",
        "pycharm", "webstorm", "rider", "clion", "cursor", "postman", "wsl", "ubuntu", "command prompt", "sublime",
        "notepad++", "jetbrains", "unity", "unreal", "godot", "rust", "ollama", "lm studio", "android studio",
        "xampp", "mysql", "postgres", "sql", "dbeaver", "putty", "winscp", "filezilla", "wireshark", "ghidra",
        "ida", "x64dbg", "debug", "sdk", "developer", "java", "neovim", "vim", "emacs", "insomnia", "anaconda",
        "jupyter", "kiro", "windsurf", "zed",
    ]),
    ("Games", &[
        "steam", "epic games", "game", "xbox", "minecraft", "riot", "valorant", "league of legends", "battle.net",
        "ubisoft", "ea app", "origin", "gog", "playnite", "roblox", "solitaire", "launcher", "genshin", "honkai",
        "rockstar", "nexus mods", "retroarch", "emulator", "yuzu", "ryujinx", "dolphin", "pcsx",
    ]),
    ("Other", &[]),
];
const OTHER: usize = CATS.len() - 1;

/// Which category an app belongs in, from its name.
fn category(name: &str) -> usize {
    let n = name.to_lowercase();
    // whole words only for short keys ("tv", "arc", "obs", "ida"), so "Activity" is not TV
    let words: Vec<&str> = n.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '-' && c != '.').filter(|w| !w.is_empty()).collect();
    let has = |k: &str| if k.len() <= 4 && !k.contains(' ') { words.contains(&k) } else { n.contains(k) };
    // the more specific groups first: "Visual Studio Code" is a developer tool before it is "studio"
    for &c in &[6usize, 7, 4, 2, 5, 3, 1, 0] {
        if CATS[c].1.iter().any(|k| has(k)) {
            return c;
        }
    }
    OTHER
}

/// Where the apps opened from here are remembered, newest first.
fn recents_path() -> String {
    format!("{}\\apps_recent.txt", crate::config::dir())
}

fn load_recents() -> Vec<String> {
    std::fs::read_to_string(recents_path()).map(|s| s.lines().filter(|l| !l.is_empty()).map(String::from).collect()).unwrap_or_default()
}

pub(crate) fn remember(spec: &str) {
    let mut r = load_recents();
    r.retain(|s| s != spec);
    r.insert(0, spec.to_string());
    r.truncate(COLS * 2);
    let _ = std::fs::create_dir_all(crate::config::dir());
    let _ = std::fs::write(recents_path(), r.join("\n"));
}

const DEFAULT_W: f32 = 560.0;
const PAD: (f32, f32) = (16.0, 12.0);
const DEFAULT_H: f32 = 372.0;
const COLS: usize = 7;
const HEAD_LINE: f32 = 31.0;
const CHIP_Y: f32 = 39.0;
const CHIP_H: f32 = 21.0;
const GRID_Y: f32 = 68.0;
const CELL_H: f32 = 72.0;
const ICON: f32 = 42.0;
/// the line between the recent row and the rest
const DIVIDER_H: f32 = 13.0;
/// a mouse wheel notch scrolls this far; touchpad units move the list this many DIPs each
const NOTCH: f32 = 72.0;
const TOUCH: f32 = 0.55;
const TOUCH_END_MS: u128 = 90;

const ID_SEARCH: u32 = 1;
const ID_MORE: u32 = 2;
const ID_CHIP: u32 = 10; // 10.. the category chips
const ID_APP: u32 = 100; // 100.. the cells drawn, an index into `cells`

pub struct Apps {
    work_area: Option<(windows::Win32::Foundation::RECT, f64)>,
    width: f32,
    height: f32, // content height, excluding padding
    cols: usize,
    dark: bool,
    theme_checked: Instant,
    apps: Vec<App>,
    cat_of: Vec<usize>,
    /// the categories that have apps, in order
    cats: Vec<usize>,
    recent_specs: Vec<String>,
    query: String,
    /// the category chip picked (an index into CATS)
    cat: Option<usize>,
    /// what the grid shows: the recent row (empty while searching or in a category), then the rest
    recent: Vec<usize>,
    shown: Vec<usize>,
    /// keyboard selection, an index into recent ++ shown; shown once the arrows are used or a search typed
    sel: usize,
    kb: bool,
    /// the list's scroll in DIPs, where it eases to, and the furthest it goes
    scroll: f32,
    target: f32,
    max_scroll: f32,
    /// the chip row scrolls sideways when it is wider than the window
    chip_scroll: f32,
    chip_max: f32,
    over_chips: bool,
    /// the last touchpad scroll: the list follows the fingers until they stop
    touch: Option<Instant>,
    focus: bool,
    focus_t: f32,
    blink: Instant,
    changed: Option<Instant>,
    last_tick: Option<Instant>,
    motion: bool,
}

impl Apps {
    pub fn new(motion: bool) -> Apps {
        let mut a = Apps::empty(Vec::new(), load_recents(), motion);
        let (area, dpi) = crate::display::cursor_work_area();
        a.fit_work_area(area, dpi);
        a.pick_up();
        refresh();
        a
    }

    fn empty(apps: Vec<App>, recent_specs: Vec<String>, motion: bool) -> Apps {
        Apps {
            work_area: None, width: DEFAULT_W, height: DEFAULT_H, cols: COLS, dark: crate::sysctl::dark(), theme_checked: Instant::now(),
            apps, cat_of: Vec::new(), cats: Vec::new(), recent_specs, query: String::new(), cat: None, recent: Vec::new(),
            shown: Vec::new(), sel: 0, kb: false, scroll: 0.0, target: 0.0, max_scroll: 0.0, chip_scroll: 0.0,
            chip_max: 0.0, over_chips: false, touch: None, focus: true, focus_t: 1.0, blink: Instant::now(),
            changed: None, last_tick: None, motion,
        }
    }

    fn fit_work_area(&mut self, area: windows::Win32::Foundation::RECT, dpi: f64) {
        self.work_area = Some((area, dpi));
        self.width = ((area.right - area.left) as f64 / (2.0 * dpi)) as f32;
        self.height = (((area.bottom - area.top) as f64 / (2.0 * dpi)) as f32 - 2.0 * PAD.1).max(1.0);
        let cell_width = (DEFAULT_W - 2.0 * PAD.0) / COLS as f32;
        self.cols = ((self.width - 2.0 * PAD.0) / cell_width).round().max(1.0) as usize;
        self.filter();
    }

    fn ink(&self, alpha: u8) -> (u8, u8, u8, u8) {
        if self.dark { white(alpha) } else { (24, 28, 36, alpha) }
    }

    fn pick_up(&mut self) {
        self.apps = CATALOG.lock().map(|c| c.clone()).unwrap_or_default();
        self.sort_out();
    }

    fn sort_out(&mut self) {
        self.cat_of = self.apps.iter().map(|a| category(&a.name)).collect();
        self.cats = (0..CATS.len()).filter(|c| self.cat_of.contains(c)).collect();
        if self.cat.is_some_and(|c| !self.cats.contains(&c)) {
            self.cat = None;
        }
        self.filter();
    }

    fn filter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.recent.clear();
        if !q.is_empty() {
            let mut hits: Vec<(u8, usize)> = self.apps.iter().enumerate().filter_map(|(i, a)| a.search_rank(&q).map(|r| (r, i))).collect();
            hits.sort();
            self.shown = hits.into_iter().map(|h| h.1).collect();
        } else if let Some(c) = self.cat {
            self.shown = (0..self.apps.len()).filter(|&i| self.cat_of[i] == c).collect();
        } else {
            self.recent = self.recent_specs.iter().filter_map(|s| self.apps.iter().position(|a| &a.spec == s)).take(self.cols).collect();
            self.shown = (0..self.apps.len()).collect();
        }
        let n = self.recent.len() + self.shown.len();
        self.sel = self.sel.min(n.saturating_sub(1));
        self.max_scroll = (self.list_h() - (self.height - GRID_Y)).max(0.0);
        self.target = self.target.clamp(0.0, self.max_scroll);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll);
    }

    fn top_of_rest(&self) -> f32 {
        if self.recent.is_empty() { 4.0 } else { 4.0 + CELL_H + DIVIDER_H }
    }

    fn list_h(&self) -> f32 {
        self.top_of_rest() + self.shown.len().div_ceil(self.cols) as f32 * CELL_H + 8.0
    }

    /// Where cell `i` of recent ++ shown sits in the list (DIPs from the list's top).
    fn cell_xy(&self, i: usize, cw: f32) -> (f32, f32) {
        let cell_w = cw / self.cols as f32;
        if i < self.recent.len() {
            (i as f32 * cell_w, 4.0)
        } else {
            let j = i - self.recent.len();
            ((j % self.cols) as f32 * cell_w, self.top_of_rest() + (j / self.cols) as f32 * CELL_H)
        }
    }

    fn app_of(&self, i: usize) -> Option<usize> {
        if i < self.recent.len() { self.recent.get(i).copied() } else { self.shown.get(i - self.recent.len()).copied() }
    }

    fn changed_now(&mut self) {
        self.target = 0.0;
        self.scroll = 0.0;
        self.sel = 0;
        if self.motion {
            self.changed = Some(Instant::now());
        }
    }

    fn set_query(&mut self, q: String) {
        self.query = q;
        self.focus = true;
        self.kb = !self.query.is_empty();
        self.blink = Instant::now();
        self.filter();
        self.changed_now();
    }

    fn set_cat(&mut self, c: Option<usize>) {
        self.cat = c;
        self.query.clear();
        self.kb = false;
        self.filter();
        self.changed_now();
    }

    fn launch(&self, ai: usize) -> Cmd {
        let spec = self.apps[ai].spec.clone();
        if spec == "macdock:settings" {
            return Cmd::Settings;
        }
        remember(&spec);
        Cmd::Launch(spec)
    }

    fn move_sel(&mut self, d: i32) {
        let n = self.recent.len() + self.shown.len();
        if n == 0 {
            return;
        }
        if !self.kb {
            self.kb = true;
            return;
        }
        // up and down from the recent row go to the same column in the next row, and back
        let s = self.sel as i32;
        let r = self.recent.len() as i32;
        let next = if d.abs() == self.cols as i32 && r > 0 && (s < r) != (s + d < r) {
            if d > 0 { r + s % self.cols as i32 } else { (s - r) % self.cols as i32 }
        } else {
            s + d
        };
        self.sel = next.clamp(0, n as i32 - 1) as usize;
        // keep it in view
        let (_, y) = self.cell_xy(self.sel, self.width - 2.0 * PAD.0);
        let view = self.height - GRID_Y;
        if y < self.target {
            self.target = (y - 4.0).max(0.0);
        } else if y + CELL_H > self.target + view {
            self.target = (y + CELL_H - view + 8.0).min(self.max_scroll);
        }
        if !self.motion {
            self.scroll = self.target;
        }
    }

    fn title(&self) -> &'static str {
        match self.cat {
            Some(c) => t(CATS[c].0),
            None => t("Applications"),
        }
    }

    fn draw_header(&mut self, ui: &mut Ui) {
        let cw = ui.w;
        let my = 14.0;
        // the Apps "A": three thin strokes
        let c = self.ink(0xD9);
        ui.line(2.5, my + 7.5, 9.0, my - 7.0, 1.6, c);
        ui.line(9.0, my - 7.0, 15.5, my + 7.5, 1.6, c);
        ui.line(1.0, my + 3.0, 17.0, my + 3.0, 1.6, c);
        // the title is the search field; it has the typing from the start, so the caret blinks before it
        let (fx, fw) = (24.0, cw - 24.0 - 34.0);
        let hov = ui.hit(ID_SEARCH, fx, 0.0, fw, 28.0);
        if hov {
            ui.fill(fx, 0.0, fw, 28.0, 8.0, self.ink(0x0C));
        }
        let caret_on = self.focus && (self.blink.elapsed().as_millis() / BLINK_MS) % 2 == 0;
        let caret = (ACCENT.0, ACCENT.1, ACCENT.2, 255);
        let tx = fx + 6.0;
        let max = fw - 14.0;
        if self.query.is_empty() {
            // a category picked shows its name; the placeholder dims a little once it has the typing
            let a = if self.cat.is_some() { 0xF2 } else { (0xF2 as f32 - 0x40 as f32 * ease_out(self.focus_t)) as u8 };
            ui.text_mid(tx, my, self.title(), 17.0, 400, self.ink(a), max);
            if caret_on {
                ui.fill(tx - 1.5, my - 9.0, 1.5, 18.0, 0.0, caret);
            }
        } else {
            let end = ui.text_mid(tx, my, &self.query, 17.0, 400, self.ink(0xF5), max);
            if caret_on {
                ui.fill(tx + end.min(max) + 1.0, my - 9.0, 1.5, 18.0, 0.0, caret);
            }
        }
        // "…": MacDock's settings (where the classic full-screen Launchpad can be picked)
        let (mx, mw) = (cw - 26.0, 26.0);
        if ui.hit(ID_MORE, mx, 1.0, mw, 26.0) {
            ui.fill(mx, 1.0, mw, 26.0, 13.0, self.ink(if ui.pressed == Some(ID_MORE) { 0x30 } else { 0x1A }));
        }
        for k in 0..3 {
            ui.circle(mx + 8.0 + 5.0 * k as f32, my, 1.3, self.ink(0xD9));
        }
        ui.line(0.0, HEAD_LINE, cw, HEAD_LINE, 1.0, self.ink(0x1F));
    }

    fn draw_chips(&mut self, ui: &mut Ui) {
        let cw = ui.w;
        let widths: Vec<f32> = self.cats.iter().map(|&c| ui.text_w(t(CATS[c].0), 11.0, 400) + 16.0).collect();
        let gap = 6.0;
        let total: f32 = widths.iter().sum::<f32>() + gap * (widths.len().saturating_sub(1)) as f32;
        self.chip_max = (total - cw).max(0.0);
        self.chip_scroll = self.chip_scroll.clamp(0.0, self.chip_max);
        let fade = [0.0, 0.0, if self.chip_scroll > 1.0 { 16.0 } else { 0.0 }, if self.chip_scroll < self.chip_max - 1.0 { 16.0 } else { 0.0 }];
        let (cats, sel, scroll) = (self.cats.clone(), self.cat, self.chip_scroll);
        // when they fit, spread them across the width like macOS does
        let extra = if self.chip_max <= 0.0 && widths.len() > 1 { (cw - total) / (widths.len() - 1) as f32 } else { 0.0 };
        let mut over = false;
        ui.layer(0.0, CHIP_Y - 2.0, cw, CHIP_H + 4.0, 1.0, 1.0, fade, |ui| {
            let mut x = -scroll;
            for (k, &c) in cats.iter().enumerate() {
                let w = widths[k];
                let id = ID_CHIP + c as u32;
                let hov = ui.hit(id, x, CHIP_Y, w, CHIP_H);
                over |= hov;
                let on = sel == Some(c);
                let pressed = ui.pressed == Some(id);
                let bg = self.ink(if on { 0x52 } else if pressed { 0x33 } else if hov { 0x28 } else { 0x17 });
                ui.fill(x, CHIP_Y, w, CHIP_H, 6.0, bg);
                ui.text_mid(x + 8.0, CHIP_Y + CHIP_H / 2.0, t(CATS[c].0), 11.0, 400, self.ink(if on { 0xFF } else { 0xD9 }), 0.0);
                x += w + gap + extra;
            }
        });
        self.over_chips = over;
    }

    fn draw_cell(&self, ui: &mut Ui, i: usize, x: f32, y: f32, cell_w: f32) {
        let Some(ai) = self.app_of(i) else { return };
        let a = &self.apps[ai];
        let id = ID_APP + i as u32;
        let ix = x + (cell_w - ICON) / 2.0;
        let iy = y + 4.0;
        ui.hit(id, x + 4.0, y, cell_w - 8.0, CELL_H - 4.0);
        if self.kb && i == self.sel {
            ui.fill(x + 4.0, y - 1.0, cell_w - 8.0, CELL_H - 2.0, 10.0, self.ink(0x2E));
        }
        match &a.icon {
            Some(img) => ui.image(img, ix, iy, ICON, ICON),
            None => ui.fill(ix + ICON * 0.08, iy + ICON * 0.08, ICON * 0.84, ICON * 0.84, ICON * 0.22, self.ink(0x1F)),
        }
        if ui.pressed == Some(id) {
            ui.fill(ix + ICON * 0.06, iy + ICON * 0.06, ICON * 0.88, ICON * 0.88, ICON * 0.22, (0, 0, 0, 0x55));
        }
        let max = cell_w - 8.0;
        let tw = ui.text_w(a.display_name(), 10.5, 400).min(max);
        let (tx, ty) = (x + (cell_w - tw) / 2.0, iy + ICON + 5.0);
        ui.text(tx, ty, a.display_name(), 10.5, 400, self.ink(0xF0), max);
    }

    fn draw_grid(&mut self, ui: &mut Ui) {
        let cw = ui.w;
        let cell_w = cw / self.cols as f32;
        let view = self.height - GRID_Y;
        if self.recent.is_empty() && self.shown.is_empty() {
            let msg = t(if self.apps.is_empty() { "Loading apps…" } else { "No Results" });
            let mw = ui.text_w(msg, 15.0, 500);
            ui.text_mid((cw - mw) / 2.0, GRID_Y + view / 2.0 - 10.0, msg, 15.0, 500, self.ink(0x99), 0.0);
            return;
        }
        let e = self.changed.map_or(1.0, |c| ease_out(c.elapsed().as_secs_f32() * 1000.0 / RESULTS_IN_MS));
        let scroll = self.scroll;
        let fade = [if scroll > 1.0 { 14.0 } else { 0.0 }, 18.0, 0.0, 0.0];
        let n = self.recent.len() + self.shown.len();
        let this = &*self;
        // a few DIPs of room either side so the selection's corners aren't cut
        ui.layer(-6.0, GRID_Y, cw + 12.0, view + 6.0, 0.2 + 0.8 * e, 0.97 + 0.03 * e, fade, |ui| {
            let base = GRID_Y - scroll;
            if !this.recent.is_empty() {
                let dy = base + 4.0 + CELL_H + DIVIDER_H / 2.0 - 2.0;
                if dy > GRID_Y - 2.0 && dy < GRID_Y + view {
                    ui.line(0.0, dy, cw, dy, 1.0, self.ink(0x1F));
                }
            }
            // only the rows in view
            for i in 0..n {
                let (x, y) = this.cell_xy(i, cw);
                let y = base + y;
                if y + CELL_H < GRID_Y - 2.0 {
                    continue;
                }
                if y > GRID_Y + view {
                    if i >= this.recent.len() {
                        break;
                    }
                    continue;
                }
                this.draw_cell(ui, i, x, y, cell_w);
            }
        });
        // a slim scroll bar while there is more than fits
        if self.max_scroll > 0.0 && (self.touch.is_some() || (self.scroll - self.target).abs() > 0.5) {
            let frac = view / (view + self.max_scroll);
            let bh = (view * frac).max(28.0);
            let by = GRID_Y + (view - bh) * (self.scroll / self.max_scroll).clamp(0.0, 1.0);
            ui.fill(cw + 6.0, by, 4.0, bh, 2.0, self.ink(0x55));
        }
    }

    fn scroll_by(&mut self, d: f32, touch: bool) {
        if touch {
            self.touch = Some(Instant::now());
            // past the ends it gives, like a rubber band, and springs back when the fingers lift
            let over = self.scroll < 0.0 || self.scroll > self.max_scroll;
            let k = if over { 0.3 } else { 1.0 };
            self.scroll = (self.scroll + d * k).clamp(-80.0, self.max_scroll + 80.0);
            self.target = self.scroll;
        } else {
            self.target = (self.target + d).clamp(0.0, self.max_scroll);
            if !self.motion {
                self.scroll = self.target;
            }
        }
        self.last_tick = None;
    }
}

impl Content for Apps {
    fn width(&self) -> f32 {
        self.width + 2.0 * MARGIN
    }

    fn centered_work_area(&self) -> Option<(windows::Win32::Foundation::RECT, f64)> {
        self.work_area
    }

    fn padding(&self) -> (f32, f32) {
        PAD
    }

    fn glass(&self) -> Option<f32> {
        Some(self.height)
    }

    fn motion(&self) -> bool {
        self.motion
    }

    fn animating(&self) -> bool {
        self.touch.is_some()
            || (self.scroll - self.target).abs() > 0.3
            || (self.focus_t - if self.focus { 1.0 } else { 0.0 }).abs() > 0.001
            || self.changed.is_some()
    }

    fn period_ms(&self) -> u32 {
        BLINK_MS as u32 / 2
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.theme_checked).as_millis() >= 250 {
            self.dark = crate::sysctl::dark();
            self.theme_checked = now;
        }
        let dt = self.last_tick.map_or(0.016, |t| (now - t).as_secs_f32()).min(0.05);
        self.last_tick = Some(now);
        let goal = if self.focus { 1.0 } else { 0.0 };
        self.focus_t = if !self.motion || (self.focus_t - goal).abs() < 0.001 { goal } else { goal + (self.focus_t - goal) * (-dt * 16.0).exp() };
        if self.changed.is_some_and(|c| c.elapsed().as_secs_f32() * 1000.0 > RESULTS_IN_MS) {
            self.changed = None;
        }
        if self.touch.is_some_and(|t| t.elapsed().as_millis() > TOUCH_END_MS) {
            self.touch = None;
            self.target = self.scroll.clamp(0.0, self.max_scroll);
        }
        if self.touch.is_none() {
            if (self.scroll - self.target).abs() <= 0.3 {
                self.scroll = self.target;
            } else {
                self.scroll = self.target + (self.scroll - self.target) * (-dt * 15.0).exp();
            }
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
        let tint = if self.dark { (24, 27, 32, 202) } else { (248, 249, 252, 222) };
        ui.fill(-PAD.0, -PAD.1, self.width, self.height + 2.0 * PAD.1, crate::flyout::GLASS_R, tint);
        self.draw_header(ui);
        self.draw_chips(ui);
        self.draw_grid(ui);
        self.height
    }

    fn hover(&mut self, id: Option<u32>) {
        self.over_chips = id.is_some_and(|i| (ID_CHIP..ID_APP).contains(&i));
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_SEARCH => {
                self.focus = true;
                self.blink = Instant::now();
                Cmd::None
            }
            ID_MORE => Cmd::Settings,
            _ if (ID_CHIP..ID_APP).contains(&id) => {
                let c = (id - ID_CHIP) as usize;
                self.set_cat(if self.cat == Some(c) { None } else { Some(c) });
                Cmd::None
            }
            _ => match id.checked_sub(ID_APP).and_then(|i| self.app_of(i as usize)) {
                Some(ai) => self.launch(ai),
                None => Cmd::None,
            },
        }
    }

    fn right_click(&mut self, id: u32) -> Cmd {
        match id.checked_sub(ID_APP).filter(|_| id >= ID_APP).and_then(|i| self.app_of(i as usize)) {
            Some(ai) => {
                if self.apps[ai].spec == "macdock:settings" {
                    Cmd::Settings
                } else {
                    Cmd::Pin(self.apps[ai].spec.clone())
                }
            }
            None => Cmd::None,
        }
    }

    fn wheel(&mut self, delta: i32) {
        let touch = delta % 120 != 0 || self.touch.is_some();
        if self.over_chips && self.chip_max > 0.0 {
            self.chip_scroll = (self.chip_scroll - delta as f32 * if touch { TOUCH } else { NOTCH / 120.0 }).clamp(0.0, self.chip_max);
            return;
        }
        let d = -delta as f32 * if touch { TOUCH } else { NOTCH / 120.0 };
        self.scroll_by(d, touch && self.motion);
    }

    fn hwheel(&mut self, delta: i32) {
        if self.chip_max > 0.0 {
            self.chip_scroll = (self.chip_scroll + delta as f32 * TOUCH).clamp(0.0, self.chip_max);
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
                if let Some(ai) = self.app_of(self.sel).filter(|_| self.kb || !self.query.is_empty()) {
                    return self.launch(ai);
                }
            }
            '\t' => {}
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
            0x25 => self.move_sel(-1),
            0x27 => self.move_sel(1),
            0x26 => self.move_sel(-(self.cols as i32)),
            0x28 => self.move_sel(self.cols as i32),
            0x21 => self.scroll_by(-(self.height - GRID_Y) * 0.8, false),
            0x22 => self.scroll_by((self.height - GRID_Y) * 0.8, false),
            0x24 => self.scroll_by(-self.max_scroll - 100.0, false),
            0x23 => self.scroll_by(self.max_scroll + 100.0, false),
            _ => {}
        }
        Cmd::None
    }

    fn escape(&mut self) -> Cmd {
        if !self.query.is_empty() {
            self.set_query(String::new());
            self.focus = true;
            Cmd::None
        } else if self.cat.is_some() {
            self.set_cat(None);
            Cmd::None
        } else if self.focus {
            self.focus = false;
            Cmd::None
        } else {
            Cmd::Close
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(names: &[&str], recent: &[&str]) -> Apps {
        let list = names.iter().map(|n| App { name: n.to_string(), spec: format!("shell:AppsFolder\\{n}"), icon: None }).collect();
        let rec = recent.iter().map(|n| format!("shell:AppsFolder\\{n}")).collect();
        let mut a = Apps::empty(list, rec, false);
        a.sort_out();
        a
    }

    #[test]
    fn categories_from_names() {
        assert_eq!(CATS[category("Visual Studio Code")].0, "Developer Tools");
        assert_eq!(CATS[category("Discord")].0, "Social");
        assert_eq!(CATS[category("Spotify")].0, "Entertainment");
        assert_eq!(CATS[category("Steam")].0, "Games");
        assert_eq!(CATS[category("Google Chrome")].0, "Productivity & Finance");
        assert_eq!(CATS[category("Adobe Photoshop 2025")].0, "Creativity");
        assert_eq!(CATS[category("Task Manager")].0, "Utilities");
        assert_eq!(CATS[category("Weather")].0, "Information & Reading");
        // short keys match whole words only
        assert_eq!(CATS[category("Activity Tracker")].0, "Other");
    }

    #[test]
    fn recents_first_then_all_and_categories_filter() {
        let mut a = apps(&["Discord", "Notepad", "Spotify", "Steam", "Zed"], &["Spotify", "Gone App"]);
        assert_eq!(a.recent.len(), 1, "unknown recents are skipped");
        assert_eq!(a.apps[a.recent[0]].name, "Spotify");
        assert_eq!(a.shown.len(), 5);
        // only categories with apps get a chip
        assert!(!a.cats.contains(&3), "no Information & Reading apps");
        a.set_cat(Some(4));
        assert!(a.recent.is_empty());
        assert_eq!(a.shown.iter().map(|&i| a.apps[i].name.as_str()).collect::<Vec<_>>(), ["Discord"]);
        assert!(matches!(a.escape(), Cmd::None));
        assert_eq!(a.cat, None);
        a.set_query("st".into());
        assert_eq!(a.apps[a.shown[0]].name, "Steam");
        assert!(matches!(a.char('\r'), Cmd::Launch(s) if s.ends_with("Steam")));
    }

    #[test]
    fn quarter_screen_layout_and_theme() {
        use windows::Win32::Foundation::RECT;
        let mut a = apps(&["Calculator"], &[]);
        let area = RECT { left: -2560, top: 0, right: 0, bottom: 1440 };
        a.fit_work_area(area, 1.5);
        assert_eq!(a.centered_work_area(), Some((area, 1.5)));
        assert_eq!(((a.width() - 2.0 * MARGIN) * 1.5).round(), 1280.0);
        assert_eq!(((a.glass().unwrap() + 2.0 * PAD.1) * 1.5).round(), 720.0);
        assert!(a.cols > COLS);
        // Sample the composited background away from text and icons.
        let sample = |a: &mut Apps, dark| {
            a.dark = dark;
            let (w, _, px) = crate::flyout::snapshot_glass(a, 1.0);
            (px[((350 * w + 100) * 4) as usize], a.ink(255).0)
        };
        let (paper, ink) = sample(&mut a, false);
        assert!(paper > ink + 100);
        let (paper, ink) = sample(&mut a, true);
        assert!(ink > paper + 100);
    }

    #[test]
    fn scrolling_stays_in_range() {
        let names: Vec<String> = (0..80).map(|i| format!("App {i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut a = apps(&refs, &[]);
        assert!(a.max_scroll > 0.0);
        a.wheel(-120 * 50);
        assert_eq!(a.target, a.max_scroll);
        a.wheel(120 * 100);
        assert_eq!(a.target, 0.0);
        // keyboard selection scrolls the selected row into view
        for _ in 0..40 {
            a.key(0x28);
        }
        let (_, y) = a.cell_xy(a.sel, a.width - 2.0 * PAD.0);
        assert!(y + CELL_H <= a.target + (a.height - GRID_Y) + 8.5 && y >= a.target - 0.5, "y {y} target {}", a.target);
    }

    /// The Apps window with the real app list on a dark backdrop: target/apps.png
    /// (cargo test apps_snapshot -- --ignored).
    #[test]
    #[ignore]
    fn apps_snapshot() {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        crate::launchpad::load_now();
        let mut a = Apps::empty(Vec::new(), load_recents(), false);
        a.pick_up();
        assert!(a.apps.len() > 10, "only {} apps", a.apps.len());
        let (w, h, px) = crate::flyout::snapshot_glass(&mut a, 1.25);
        let path = format!(r"{}\target\apps.png", env!("CARGO_MANIFEST_DIR"));
        assert!(crate::gfx::save_png(&path, w, h, &px));
    }
}
