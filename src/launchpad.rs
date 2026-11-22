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

#[derive(Clone)]
struct App {
    name: String,
    /// shell:AppsFolder\<id>, what launching and pinning use
    spec: String,
    icon: Option<Arc<Pixmap>>,
}

static CATALOG: Mutex<Vec<App>> = Mutex::new(Vec::new());
static LOADING: AtomicBool = AtomicBool::new(false);
/// the open panel's window, which the worker tells about new data (0 = none)
static NOTIFY: AtomicIsize = AtomicIsize::new(0);

const ICON_PX: u32 = 96;

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
fn refresh() {
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
            if let Some(pm) = crate::shell::icon(spec, ICON_PX) {
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

const W: f32 = 940.0;
const COLS: usize = 7;
const ROWS: usize = 4;
const PER_PAGE: usize = COLS * ROWS;
const CELL_H: f32 = 118.0;
const ICON: f32 = 64.0;
const SEARCH_H: f32 = 34.0;
const ID_APP: u32 = 1; // ids 1..=PER_PAGE: the cells of the page shown
const ID_DOT: u32 = 100; // 100.. the page dots

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
}

impl Launchpad {
    pub fn new() -> Launchpad {
        let mut l = Launchpad { apps: Vec::new(), query: String::new(), shown: Vec::new(), page: 0, sel: 0, wheel: 0 };
        l.pick_up();
        refresh();
        l
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
                .filter_map(|(i, a)| {
                    let n = a.name.to_lowercase();
                    let rank = if n.starts_with(&q) {
                        0
                    } else if n.split([' ', '-', '.', '_']).any(|w| w.starts_with(&q)) {
                        1
                    } else if n.contains(&q) {
                        2
                    } else {
                        return None;
                    };
                    Some((rank, i))
                })
                .collect();
            hits.sort();
            self.shown = hits.into_iter().map(|h| h.1).collect();
        }
        let pages = self.pages();
        self.page = self.page.min(pages - 1);
        self.sel = self.sel.min(self.shown.len().saturating_sub(1));
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
        self.page = 0;
        self.sel = 0;
        self.filter();
    }

    fn turn(&mut self, d: i32) {
        let p = (self.page as i32 + d).clamp(0, self.pages() as i32 - 1) as usize;
        if p != self.page {
            self.page = p;
            self.sel = p * PER_PAGE;
        }
    }

    fn move_sel(&mut self, d: i32) {
        if self.shown.is_empty() {
            return;
        }
        let s = (self.sel as i32 + d).clamp(0, self.shown.len() as i32 - 1) as usize;
        self.sel = s;
        self.page = s / PER_PAGE;
    }
}

impl Content for Launchpad {
    fn width(&self) -> f32 {
        W
    }

    fn padding(&self) -> (f32, f32) {
        (22.0, 20.0)
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
        let cw = ui.w;
        // search field, centred
        let sw = 300.0f32.min(cw);
        let sx = (cw - sw) / 2.0;
        ui.fill(sx, 0.0, sw, SEARCH_H, SEARCH_H / 2.0, white(0x1C));
        ui.outline(sx, 0.0, sw, SEARCH_H, SEARCH_H / 2.0, 1.0, white(0x26));
        ui.glyph(sx + 18.0, SEARCH_H / 2.0, '\u{E721}', 13.0, white(0x8C));
        let tx = sx + 34.0;
        if self.query.is_empty() {
            ui.text_mid(tx, SEARCH_H / 2.0, "Search", 14.0, 400, white(0x73), 0.0);
            ui.fill(tx - 1.0, 9.0, 1.5, SEARCH_H - 18.0, 0.0, (ACCENT.0, ACCENT.1, ACCENT.2, 255));
        } else {
            let end = ui.text_mid(tx, SEARCH_H / 2.0, &self.query, 14.0, 400, white(0xF2), sw - 50.0);
            let cx = tx + end.min(sw - 50.0) + 1.0;
            ui.fill(cx, 9.0, 1.5, SEARCH_H - 18.0, 0.0, (ACCENT.0, ACCENT.1, ACCENT.2, 255));
        }

        let top = SEARCH_H + 24.0;
        let cell_w = cw / COLS as f32;
        let grid_h = CELL_H * ROWS as f32;
        if self.shown.is_empty() {
            let msg = if self.apps.is_empty() { "Loading apps…" } else { "No Results" };
            let w = ui.text_w(msg, 18.0, 500);
            ui.text_mid((cw - w) / 2.0, top + grid_h / 2.0, msg, 18.0, 500, white(0x80), 0.0);
        }
        let start = self.page * PER_PAGE;
        for cell in 0..PER_PAGE {
            let Some(&ai) = self.shown.get(start + cell) else { break };
            let (r, c) = (cell / COLS, cell % COLS);
            let x = c as f32 * cell_w;
            let y = top + r as f32 * CELL_H;
            let hov = ui.hit(ID_APP + cell as u32, x + 4.0, y, cell_w - 8.0, CELL_H - 6.0);
            let selected = !self.query.is_empty() && start + cell == self.sel;
            if hov || selected {
                ui.fill(x + 4.0, y, cell_w - 8.0, CELL_H - 6.0, 14.0, white(if selected { 0x26 } else { 0x16 }));
            }
            let a = &self.apps[ai];
            let ix = x + (cell_w - ICON) / 2.0;
            let iy = y + 12.0;
            match &a.icon {
                Some(img) => ui.image(img, ix, iy, ICON, ICON),
                None => ui.fill(ix + 6.0, iy + 6.0, ICON - 12.0, ICON - 12.0, 12.0, white(0x14)),
            }
            let max = cell_w - 16.0;
            let tw = ui.text_w(&a.name, 12.5, 400).min(max);
            ui.text(x + (cell_w - tw) / 2.0, iy + ICON + 10.0, &a.name, 12.5, 400, white(0xE6), max);
        }

        // page dots
        let pages = self.pages();
        let dy = top + grid_h + 14.0;
        if pages > 1 {
            let gap = 16.0;
            let x0 = (cw - gap * (pages - 1) as f32) / 2.0;
            for p in 0..pages {
                let cx = x0 + gap * p as f32;
                let hov = ui.hit(ID_DOT + p as u32, cx - 7.0, dy - 7.0, 14.0, 14.0);
                let a = if p == self.page { 0xE6 } else if hov { 0x80 } else { 0x4D };
                ui.circle(cx, dy, 3.5, white(a));
            }
        }
        dy + 10.0
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        if id >= ID_DOT {
            self.page = ((id - ID_DOT) as usize).min(self.pages() - 1);
            return Cmd::None;
        }
        match self.app_at(id) {
            Some(a) => Cmd::Launch(a.spec.clone()),
            None => Cmd::None,
        }
    }

    fn right_click(&mut self, id: u32) -> Cmd {
        match self.app_at(id) {
            Some(a) => Cmd::Pin(a.spec.clone()),
            None => Cmd::None,
        }
    }

    fn wheel(&mut self, delta: i32) {
        self.wheel += delta;
        if self.wheel.abs() >= 120 {
            self.turn(if self.wheel < 0 { 1 } else { -1 });
            self.wheel = 0;
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
        let mut l = Launchpad { apps, query: String::new(), shown: Vec::new(), page: 0, sel: 0, wheel: 0 };
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
    fn junk_is_skipped() {
        assert!(junk("Uninstall Foo", "x"));
        assert!(junk("Foo website", "x"));
        assert!(junk("Foo", "https://foo"));
        assert!(!junk("Visual Studio Code", "Microsoft.VisualStudioCode"));
    }

    #[test]
    fn paging_and_selection() {
        let names: Vec<String> = (0..60).map(|i| format!("App {i:02}")).collect();
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
        let mut l = Launchpad { apps, query: String::new(), shown: Vec::new(), page: 0, sel: 0, wheel: 0 };
        l.filter();
        let (w, h, px) = crate::flyout::snapshot(&mut l, 1.25);
        let path = format!(r"{}\target\launchpad.png", env!("CARGO_MANIFEST_DIR"));
        assert!(crate::gfx::save_png(&path, w, h, &px));
    }
}
