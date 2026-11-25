//! macOS-style app switcher on Alt+Tab (like Cmd+Tab): one big icon per running app in a dark frosted panel
//! centred on the screen, the selected app named under its icon. Hold Alt and press Tab / Shift+Tab or the
//! arrow keys to move, release Alt to switch; Esc cancels, Q quits the selected app, H minimizes it; the mouse
//! selects on hover and switches on click. A quick Alt+Tab tap switches to the previous app without ever
//! showing the panel.
//!
//! Runs on its own thread, and the low-level keyboard hook (WH_KEYBOARD_LL) on another one that does nothing
//! else, since it must answer within Windows' timeout. The hook only swallows keys and posts messages; the
//! window's thread does the work.
//! The glass is a one-time screen capture under the panel, blurred, through gfx::liquid_glass, with a dark veil.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use tiny_skia::{Color, FillRule, Paint, Pixmap, PixmapPaint, Transform};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};
use crate::shell::{self, AppWindow, Shell};

// DIPs
const ICON: f32 = 92.0;
const CELL_PAD: f32 = 12.0; // selection square around the icon
const PAD: f32 = 16.0; // panel edge to the first cell
const NAME_H: f32 = 30.0; // row under the cells for the selected app's name
const RADIUS: f32 = 30.0;
const MARGIN: f32 = 40.0; // shadow room around the panel
const SHOW_DELAY_MS: u32 = 90; // a quicker Alt+Tab tap switches without flashing the panel

const TIMER_SHOW: usize = 1;
const TIMER_POLL: usize = 2; // Alt released but its key-up missed (hook timed out, secure desktop): commit anyway

const WM_SW_TAB: u32 = WM_APP + 1; // wparam 1 = backwards
const WM_SW_MOVE: u32 = WM_APP + 2; // wparam 0 = left, 1 = right
const WM_SW_COMMIT: u32 = WM_APP + 3;
const WM_SW_CANCEL: u32 = WM_APP + 4;
const WM_SW_QUIT: u32 = WM_APP + 5;
const WM_SW_HIDE: u32 = WM_APP + 6;

static HWND_SW: AtomicIsize = AtomicIsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false); // an Alt+Tab session is running: the hook eats our keys

struct App {
    key: String,
    name: String,
    wins: Vec<HWND>, // front first
    icon: Option<Pixmap>,
}

struct Switcher {
    hwnd: HWND,
    shell: Shell,
    icons: HashMap<String, (String, Option<Pixmap>)>, // key -> (name, icon at the current size)
    icon_px: u32,
    apps: Vec<App>,
    sel: usize,
    shown: bool,
    s: f32,
    pos: (i32, i32),
    size: (u32, u32),
    cell: f32, // device px
    glass: Option<Pixmap>,
    surface: Option<Surface>,
}

fn paint(rgb: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(rgb.2, rgb.1, rgb.0, a)); // BGRA surfaces
    p.anti_alias = true;
    p
}

/// The selected app's name for a panel this wide: at most 39 characters, then cut with an ellipsis
/// until it fits `room` px. A name wider than the panel has nowhere to go, so it is shortened instead.
fn fit_name(name: &str, font: Font, room: f32) -> (String, Option<Pixmap>) {
    let mut keep: Vec<char> = name.chars().take(39).collect();
    let mut text: String =
        if name.chars().count() > 39 { keep.iter().chain(Some(&'…')).collect() } else { keep.iter().collect() };
    let mut t = gfx::text_ex(&text, font, (255, 255, 255));
    while keep.len() > 1 && t.as_ref().is_some_and(|p| p.width() as f32 > room) {
        keep.pop();
        text = keep.iter().chain(Some(&'…')).collect();
        t = gfx::text_ex(&text, font, (255, 255, 255));
    }
    (text, t)
}

/// The x of the selected app's name: centred under its icon, kept `12 * s` inside the panel edges.
/// `.max(left)` on the right edge keeps a name as wide as the panel from clamping with min > max.
fn name_x(centre: f32, tw: f32, px: f32, pw: f32, s: f32) -> f32 {
    let left = px + 12.0 * s;
    (centre - tw / 2.0).clamp(left, (px + pw - 12.0 * s - tw).max(left))
}

fn app_key(w: &AppWindow) -> String {
    match (&w.aumid, &w.exe) {
        (Some(a), _) => format!("aumid:{}", a.to_lowercase()),
        (None, Some(e)) => format!("exe:{}", e.to_lowercase()),
        _ => format!("hwnd:{}", w.hwnd.0 as isize),
    }
}

impl Switcher {
    fn hwnd() -> HWND {
        HWND(HWND_SW.load(Ordering::Relaxed) as *mut _)
    }

    /// Running apps, most recently used first (the z-order), one entry per app.
    fn collect(&mut self) {
        let wins = self.shell.app_windows();
        let mut apps: Vec<App> = Vec::new();
        let px = self.icon_px;
        for w in &wins {
            let key = app_key(w);
            if let Some(a) = apps.iter_mut().find(|a| a.key == key) {
                a.wins.push(w.hwnd);
                continue;
            }
            let (name, icon) = self.icons.entry(key.clone()).or_insert_with(|| {
                let mut name = shell::friendly_name(w.exe.as_deref(), w.aumid.as_deref(), &w.title);
                if name.eq_ignore_ascii_case("explorer") {
                    name = "File Explorer".into();
                } else if name.chars().next().is_some_and(|c| c.is_lowercase()) {
                    let mut c = name.chars();
                    name = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default(); // exe stem "code" -> "Code"
                }
                let src = match (&w.aumid, &w.exe) {
                    (Some(a), _) => format!("shell:AppsFolder\\{a}"),
                    (None, Some(e)) => e.clone(),
                    _ => String::new(),
                };
                let raw = shell::icon(&src, 256).or_else(|| w.exe.as_deref().and_then(|e| shell::icon(e, 256)));
                let icon = raw.and_then(|r| crate::tile::make(&r, px).or_else(|| crate::tile::fit(&r, px)));
                (name, icon)
            }).clone();
            apps.push(App { key, name, wins: vec![w.hwnd], icon });
        }
        self.apps = apps;
    }

    /// Size and centre the panel on the monitor of the foreground window.
    fn place(&mut self) {
        unsafe {
            let mon = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let s = dx as f32 / 96.0;
            let r = mi.rcMonitor;
            let (mw, mh) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
            let n = self.apps.len().max(1) as f32;
            let mut cell = (ICON + 2.0 * CELL_PAD) * s;
            let max_w = mw - 2.0 * (MARGIN + 40.0) * s;
            if 2.0 * PAD * s + n * cell > max_w {
                cell = ((max_w - 2.0 * PAD * s) / n).max(40.0 * s); // many apps: smaller icons, like macOS
            }
            let icon_px = (cell - 2.0 * CELL_PAD * s * (cell / ((ICON + 2.0 * CELL_PAD) * s))).round().max(24.0) as u32;
            if icon_px != self.icon_px || (s - self.s).abs() > 0.01 {
                self.icon_px = icon_px;
                self.icons.clear();
            }
            self.s = s;
            self.cell = cell;
            let pw = 2.0 * PAD * s + n * cell;
            let ph = PAD * s + cell + NAME_H * s + PAD * s * 0.5;
            self.size = ((pw + 2.0 * MARGIN * s).ceil() as u32, (ph + 2.0 * MARGIN * s).ceil() as u32);
            self.pos = (
                r.left + ((mw - self.size.0 as f32) / 2.0).round() as i32,
                r.top + ((mh - self.size.1 as f32) / 2.0).round() as i32,
            );
        }
    }

    fn panel(&self) -> (f32, f32, f32, f32) {
        let m = MARGIN * self.s;
        (m, m, self.size.0 as f32 - 2.0 * m, self.size.1 as f32 - 2.0 * m)
    }

    fn cell_origin(&self, i: usize) -> (f32, f32) {
        let (px, py, _, _) = self.panel();
        (px + PAD * self.s + i as f32 * self.cell, py + PAD * self.s)
    }

    fn cell_at(&self, x: f32, y: f32) -> Option<usize> {
        (0..self.apps.len()).find(|&i| {
            let (cx, cy) = self.cell_origin(i);
            x >= cx && x < cx + self.cell && y >= cy && y < cy + self.cell
        })
    }

    /// Shadow + frosted dark glass: what is on screen under the panel right now, blurred hard.
    fn build_glass(&self) -> Option<Pixmap> {
        let (w, h) = self.size;
        let mut under = gfx::capture_screen(self.pos.0, self.pos.1, w, h).or_else(|| {
            let mut p = Pixmap::new(w, h)?;
            p.fill(Color::from_rgba8(0x30, 0x30, 0x30, 0xFF));
            Some(p)
        })?;
        gfx::blur(&mut under, 30.0 * self.s);
        let mut pm = Pixmap::new(w, h)?;
        let (x, y, pw, ph) = self.panel();
        let r = RADIUS * self.s;
        if let Some(path) = gfx::rrect(x, y + 6.0 * self.s, pw, ph, r) {
            if let Some((sh, ox, oy)) = gfx::shadow(&path, 18.0 * self.s, (0, 0, 0, 0x60)) {
                pm.draw_pixmap(ox as i32, oy as i32, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
        let (mut g, gx, gy) = gfx::liquid_glass(&under, x, y, pw, ph, r, 16.0 * self.s, (0.0, 0.0), (0.0, 0.0))?;
        if let Some(p) = gfx::rrect(x - gx as f32, y - gy as f32, pw, ph, r) {
            let mut veil = Pixmap::new(g.width(), g.height())?;
            veil.fill_path(&p, &paint((0x16, 0x16, 0x18), 0x78), FillRule::Winding, Transform::identity(), None);
            g.draw_pixmap(0, 0, veil.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
        pm.draw_pixmap(gx, gy, g.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        Some(pm)
    }

    fn render(&mut self) {
        if !self.shown {
            return;
        }
        if self.glass.is_none() {
            self.glass = self.build_glass();
        }
        let Some(glass) = &self.glass else { return };
        let mut pm = glass.clone();
        let s = self.s;
        let cell = self.cell;
        let inset = cell * CELL_PAD / (ICON + 2.0 * CELL_PAD);
        // selection square behind the chosen icon
        let (sx, sy) = self.cell_origin(self.sel);
        if let Some(p) = gfx::rrect(sx + 2.0 * s, sy + 2.0 * s, cell - 4.0 * s, cell - 4.0 * s, 20.0 * s * cell / ((ICON + 2.0 * CELL_PAD) * s)) {
            pm.fill_path(&p, &paint((255, 255, 255), 0x30), FillRule::Winding, Transform::identity(), None);
        }
        for (i, a) in self.apps.iter().enumerate() {
            let (cx, cy) = self.cell_origin(i);
            if let Some(ic) = &a.icon {
                let target = cell - 2.0 * inset;
                let k = target / ic.width().max(ic.height()) as f32;
                let (iw, ih) = (ic.width() as f32 * k, ic.height() as f32 * k);
                let t = Transform::from_row(k, 0.0, 0.0, k, cx + (cell - iw) / 2.0, cy + (cell - ih) / 2.0);
                let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
                pm.draw_pixmap(0, 0, ic.as_ref(), &paint, t, None);
            }
        }
        // the selected app's name, centred under it, kept inside the panel. With one app the panel is
        // barely wider than its icon, so a long name is cut to fit first: the room to place it in must
        // come out positive, and one that is not is a panic (`min > max`, 25 Nov 2026).
        if let Some(a) = self.apps.get(self.sel) {
            let (px, _, pw, _) = self.panel();
            let font = Font { face: gfx::UI, px: (14.0 * s).round() as i32, weight: 600 };
            let (_, t) = fit_name(&a.name, font, (pw - 24.0 * s).max(1.0));
            if let Some(t) = t {
                let x = name_x(sx + cell / 2.0, t.width() as f32, px, pw, s);
                let y = sy + cell + (NAME_H * s - t.height() as f32) / 2.0;
                pm.draw_pixmap(x.round() as i32, y.round() as i32, t.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
        if self.surface.as_ref().is_none_or(|sf| (sf.w, sf.h) != self.size) {
            self.surface = Surface::new(self.size.0, self.size.1);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.bytes().copy_from_slice(pm.data());
            sf.present(self.hwnd, self.pos.0, self.pos.1);
        }
    }

    fn show(&mut self) {
        if self.shown || self.apps.is_empty() {
            return;
        }
        self.place();
        if self.apps.iter().any(|a| a.icon.is_none()) || self.icons.is_empty() {
            self.collect(); // icon size changed with the layout
        }
        self.shown = true;
        self.glass = None;
        self.render();
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }

    fn end(&mut self) {
        unsafe {
            // a new Alt+Tab may already be waiting (the hook runs on its own thread): leave its session on
            let mut m = MSG::default();
            if !PeekMessageW(&mut m, self.hwnd, WM_SW_TAB, WM_SW_TAB, PM_NOREMOVE).as_bool() {
                ACTIVE.store(false, Ordering::Relaxed);
            }
            let _ = KillTimer(self.hwnd, TIMER_SHOW);
            let _ = KillTimer(self.hwnd, TIMER_POLL);
            if self.shown {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.shown = false;
        self.glass = None;
        self.surface = None;
        self.apps.clear();
    }

    fn start(&mut self, back: bool) {
        if self.icon_px == 0 {
            self.place();
        }
        self.collect();
        if self.apps.is_empty() {
            self.end();
            return;
        }
        let n = self.apps.len();
        self.sel = if n == 1 { 0 } else if back { n - 1 } else { 1 };
        // an unrelated key between Alt down and Alt up, or the foreground app opens its menu bar on Alt up
        dummy_key();
        unsafe {
            SetTimer(self.hwnd, TIMER_SHOW, SHOW_DELAY_MS, None);
            SetTimer(self.hwnd, TIMER_POLL, 50, None);
        }
    }

    fn step(&mut self, back: bool) {
        let n = self.apps.len();
        if n == 0 {
            return;
        }
        self.sel = if back { (self.sel + n - 1) % n } else { (self.sel + 1) % n };
        self.render();
    }

    fn commit(&mut self) {
        let wins = self.apps.get(self.sel).map(|a| a.wins.clone()).unwrap_or_default();
        self.end();
        let Some(&front) = wins.first() else { return };
        // A dock click minimizes all of an app's windows together, and this switcher shows one icon per app:
        // bring them all back the way the dock does, else only the front one returns and the rest look lost.
        // Windows minimized one by one while others stay open are left alone.
        if wins.len() > 1 && wins.iter().all(|&h| unsafe { IsIconic(h).as_bool() }) {
            crate::genie::restore_all(front, &wins);
        } else {
            shell::activate(front);
        }
    }

    fn quit_selected(&mut self) {
        if self.sel >= self.apps.len() {
            return;
        }
        let a = self.apps.remove(self.sel);
        for h in a.wins {
            shell::close(h);
        }
        if self.apps.is_empty() {
            self.end();
            return;
        }
        self.sel = self.sel.min(self.apps.len() - 1);
        if self.shown {
            // fewer cells: resize around the same glass capture
            self.shown = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.show();
        }
    }

    fn hide_selected(&mut self) {
        if let Some(a) = self.apps.get(self.sel) {
            for &h in &a.wins {
                shell::minimize(h);
            }
        }
    }
}

thread_local! {
    static STATE: std::cell::RefCell<Option<Switcher>> = const { std::cell::RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Switcher) -> R) -> Option<R> {
    STATE.with(|st| st.borrow_mut().as_mut().map(f))
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
        },
    }
}

/// VK 0xE8 is unassigned: it only tells the foreground app that Alt was used for a shortcut.
fn dummy_key() {
    unsafe {
        SendInput(&[key_input(0xE8, false), key_input(0xE8, true)], size_of::<INPUT>() as i32);
    }
}

fn down(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) < 0 }
}

fn post(msg: u32, wp: usize) {
    unsafe {
        let _ = PostMessageW(Switcher::hwnd(), msg, WPARAM(wp), LPARAM(0));
    }
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(HHOOK::default(), code, wp, lp);
    }
    let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
    let vk = k.vkCode as u16;
    if vk == 0xE8 && k.flags.0 & LLKHF_INJECTED.0 != 0 {
        return CallNextHookEx(HHOOK::default(), code, wp, lp); // our own dummy key
    }
    let is_down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let alt = k.flags.0 & LLKHF_ALTDOWN.0 != 0 || down(VK_MENU.0 as i32);
    let active = ACTIVE.load(Ordering::Relaxed);

    if vk == VK_TAB.0 && alt && !down(VK_LWIN.0 as i32) && !down(VK_RWIN.0 as i32) {
        if is_down {
            if !active {
                ACTIVE.store(true, Ordering::Relaxed);
            }
            post(WM_SW_TAB, down(VK_SHIFT.0 as i32) as usize);
        }
        return LRESULT(1);
    }
    if !active {
        return CallNextHookEx(HHOOK::default(), code, wp, lp);
    }
    if !is_down && (vk == VK_LMENU.0 || vk == VK_RMENU.0 || vk == VK_MENU.0) {
        ACTIVE.store(false, Ordering::Relaxed);
        post(WM_SW_COMMIT, 0);
        return CallNextHookEx(HHOOK::default(), code, wp, lp);
    }
    let msg = match VIRTUAL_KEY(vk) {
        VK_ESCAPE => Some((WM_SW_CANCEL, 0)),
        VK_LEFT => Some((WM_SW_MOVE, 0)),
        VK_RIGHT => Some((WM_SW_MOVE, 1)),
        VK_RETURN => Some((WM_SW_COMMIT, 0)),
        VK_Q => Some((WM_SW_QUIT, 0)),
        VK_H => Some((WM_SW_HIDE, 0)),
        _ => None,
    };
    if let Some((m, w)) = msg {
        if is_down {
            if m == WM_SW_CANCEL || m == WM_SW_COMMIT {
                ACTIVE.store(false, Ordering::Relaxed);
            }
            post(m, w);
        }
        return LRESULT(1);
    }
    CallNextHookEx(HHOOK::default(), code, wp, lp)
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SW_TAB => {
            with(|s| if s.apps.is_empty() { s.start(wp.0 == 1) } else { s.step(wp.0 == 1) });
            return LRESULT(0);
        }
        WM_SW_MOVE => {
            with(|s| s.step(wp.0 == 0));
            return LRESULT(0);
        }
        WM_SW_COMMIT => {
            with(|s| s.commit());
            return LRESULT(0);
        }
        WM_SW_CANCEL => {
            with(|s| s.end());
            return LRESULT(0);
        }
        WM_SW_QUIT => {
            with(|s| s.quit_selected());
            return LRESULT(0);
        }
        WM_SW_HIDE => {
            with(|s| s.hide_selected());
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_SHOW => {
            let _ = KillTimer(h, TIMER_SHOW);
            with(|s| s.show());
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_POLL => {
            if !down(VK_MENU.0 as i32) {
                with(|s| if !s.apps.is_empty() { s.commit() });
            }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            with(|s| {
                if let Some(i) = s.cell_at(x, y) {
                    if i != s.sel {
                        s.sel = i;
                        s.render();
                    }
                }
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            with(|s| {
                if let Some(i) = s.cell_at(x, y) {
                    s.sel = i;
                    s.commit();
                }
            });
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Start the switcher thread: its window, the keyboard hook and a message loop.
pub fn start() {
    std::thread::spawn(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockSwitcher"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            w!("MacDockSwitcher"), w!("MacDock Switcher"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return;
        };
        HWND_SW.store(hwnd.0 as isize, Ordering::Relaxed);
        let sw = Switcher {
            hwnd, shell: Shell::default(), icons: HashMap::new(), icon_px: 0, apps: Vec::new(), sel: 0, shown: false,
            s: 1.0, pos: (0, 0), size: (1, 1), cell: 1.0, glass: None, surface: None,
        };
        STATE.with(|st| *st.borrow_mut() = Some(sw));
        with(|s| {
            // fetch the icons now so the first Alt+Tab is not slow
            s.place();
            s.collect();
            s.apps.clear();
        });
        hook_thread();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

/// The keyboard hook on a thread of its own that does nothing else. Windows calls a low-level hook through
/// its thread's message loop and passes the key on when the hook does not answer in time; while the switcher
/// thread was busy (listing windows, loading icons, capturing the glass, switching apps) a quick second
/// Alt+Tab reached Windows and opened its own switcher. Windows also drops a hook that keeps timing out.
fn hook_thread() {
    std::thread::spawn(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hinst, 0) else { return };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // 125% display scale, the laptop screen: the panel inset is 40 DIPs and the name sits 12 DIPs in
    const S: f32 = 1.25;
    const PX: f32 = 40.0 * S;

    #[test]
    fn name_x_keeps_a_long_name_inside_a_one_app_panel() {
        // the 25 Nov 2026 crash: one app comes to a 185 px panel, its name to 165 px, and
        // `clamp(px + 12s, px + pw - 12s - tw)` was `clamp(65.0, 55.0)`
        let pw = 185.0;
        assert_eq!(name_x(PX + 16.0 * S + 72.5, 165.0, PX, pw, S), 65.0);
        // a name wider than the whole panel lands on the left inset instead of panicking
        assert_eq!(name_x(PX + 100.0, 300.0, PX, pw, S), 65.0);
    }

    #[test]
    fn name_x_centres_a_short_name_and_stops_at_both_edges() {
        let pw = 500.0;
        assert_eq!(name_x(PX + 100.0, 100.0, PX, pw, S), PX + 50.0); // room to centre: untouched
        assert_eq!(name_x(PX + 20.0, 100.0, PX, pw, S), 65.0); // hugging the left edge
        assert_eq!(name_x(PX + 460.0, 100.0, PX, pw, S), 435.0); // hugging the right edge
    }
}
