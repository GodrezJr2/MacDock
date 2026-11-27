//! MacDock in Rust: dock (and, as the port goes on, menu bar and genie) on plain Win32. Every window's
//! procedure only queues events; the App below handles them and runs one frame loop paced by DwmFlush
//! that sleeps in GetMessage once nothing moves.

#![windows_subsystem = "windows"]

mod backdrop;
mod badges;
mod config;
mod display;
mod dock;
mod dropfiles;
mod ai_usage;
mod apps;
mod spotlight;
mod claude_oauth;
mod fly_ai;
mod fly_battery;
mod fly_cal;
mod fly_cc;
mod fly_settings;
mod fly_tray;
mod launchpad;
mod fly_wifi;
mod flyout;
mod genie;
mod i18n;
mod genie_gpu;
mod gfx;
mod menu;
mod menubar;
mod power;
mod shell;
mod switcher;
mod sysctl;
mod taskbar;
mod tile;
mod tray;
mod update;
mod widgets;

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

use config::Config;
use dock::{Act, Dock, DockEv};

pub enum Ev {
    Dock(HWND, DockEv),
    Genie(genie::GenieEv),
    Bar(menubar::BarEv),
    /// the flyout window it came from (a closing one still sends a deactivate)
    Fly(isize, flyout::FlyEv),
    Foreground(isize),
    MinimizeStart(isize),
    MoveSizeEnd(isize),
    Settings(fly_settings::Change),
    /// Alt+Space
    Spotlight,
}

thread_local! {
    static EVENTS: RefCell<Vec<Ev>> = const { RefCell::new(Vec::new()) };
}

pub fn push(e: Ev) {
    EVENTS.with(|q| q.borrow_mut().push(e));
}

/// Our windows must never take focus from the user's app; a menu may have lifted the flag.
pub fn ensure_no_activate(h: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        if ex & WS_EX_NOACTIVATE.0 as isize == 0 {
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
        }
    }
}

const EVENT_SYSTEM_FOREGROUND: u32 = 3;
const EVENT_SYSTEM_MOVESIZEEND: u32 = 0xB;
const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x16;
const WINEVENT_SKIPOWNPROCESS: u32 = 2;

unsafe extern "system" fn on_win_event(_: HWINEVENTHOOK, ev: u32, h: HWND, obj: i32, _: i32, _: u32, _: u32) {
    if obj != 0 || h.0.is_null() {
        return;
    }
    match ev {
        EVENT_SYSTEM_FOREGROUND => push(Ev::Foreground(h.0 as isize)),
        EVENT_SYSTEM_MINIMIZESTART => push(Ev::MinimizeStart(h.0 as isize)),
        EVENT_SYSTEM_MOVESIZEEND => push(Ev::MoveSizeEnd(h.0 as isize)),
        _ => {}
    }
}

struct App {
    dock: Dock,
    secondary: Vec<Dock>,
    genie: Option<genie::Genie>,
    bar: Option<menubar::MenuBar>,
    fly: Option<flyout::Flyout>,
    /// the Settings window, which lives beside the drop-down flyouts
    win: Option<flyout::Flyout>,
    fullscreen: bool,
    explorer_pid: Option<u32>,
    /// menu bar, widgets, Alt+Tab, Claude login as this process started them (changing them needs a restart)
    started: [bool; 4],
}

impl App {
    fn awake(&self) -> bool {
        self.dock.awake || self.secondary.iter().any(|d| d.awake) || self.genie.as_ref().is_some_and(|g| g.busy())
    }

    /// The dock icon a window belongs to, in screen pixels.
    fn icon_for(&mut self, h: HWND, source: Option<HWND>) -> genie::R {
        use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
        let mon = unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) };
        let dock = self.secondary.iter_mut().find(|d| source.map_or(d.display.id == mon, |src| d.hwnd == src)).unwrap_or(&mut self.dock);
        let (x, y, w, hh) = dock.icon_rect_for(h);
        let d = dock.dpi;
        genie::R { x: x * d, y: y * d, w: w * d, h: hh * d }
    }

    /// Show desktop (Win+D, the three-finger swipe down) lifts the desktop window into the topmost band,
    /// above topmost windows that were lower in it: dock, menu bar and widgets vanish and only the wallpaper
    /// is left. Put ours back on top of it while the desktop is up, and the widgets back down after.
    fn keep_over_desktop(&mut self, fg: HWND) {
        let up = is_desktop(fg) && desktop_raised();
        unsafe {
            if up {
                let f = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
                for d in std::iter::once(&self.dock).chain(&self.secondary) {
                    let _ = SetWindowPos(d.hwnd, HWND_TOPMOST, 0, 0, 0, 0, f);
                }
                if let Some(b) = self.bar.as_ref() {
                    let _ = SetWindowPos(b.hwnd, HWND_TOPMOST, 0, 0, 0, 0, f);
                }
            }
        }
        widgets::set_raised(up);
    }

    /// Returns false to quit.
    fn act(&mut self, a: Act, source: HWND) -> bool {
        match a {
            Act::Quit => return false,
            Act::OpenSettings => self.open_settings(),
            Act::OpenLaunchpad => self.open_launchpad(),
            Act::Fullscreen => self.update_fullscreen(),
            Act::Minimize { fg, wins } => {
                let icon = self.icon_for(fg, Some(source));
                match self.genie.as_mut() {
                    Some(g) => g.minimize_from_dock(fg, wins, icon),
                    None => genie::minimize_all(&wins),
                }
            }
            Act::Restore { front, minimized } => {
                let icon = self.icon_for(front, Some(source));
                match self.genie.as_mut() {
                    Some(g) => g.restore(front, minimized, icon),
                    None => genie::restore_async(front, minimized),
                }
            }
        }
        true
    }

    fn update_fullscreen(&mut self) {
        let full_rect = foreground_fullscreen_rect();
        for d in &mut self.secondary {
            d.set_fullscreen_hidden(full_rect == Some(d.display.rect));
        }
        let on = full_rect == Some(self.dock.display.rect);
        if on != self.fullscreen {
            self.fullscreen = on;
            self.dock.set_fullscreen_hidden(on);
            if let Some(g) = self.genie.as_mut() {
                g.pause(on); // game mode: nothing of ours in the input path
            }
            ai_usage::set_paused(on);
            if let Some(b) = self.bar.as_mut() {
                b.set_hidden(on);
            }
            if !on {
                taskbar::keep_hidden(); // Explorer re-shows it as the fullscreen app leaves
            }
        }
    }

    fn close_flyout(&mut self) {
        if let Some(mut f) = self.fly.take() {
            f.close();
        }
        if let Some(b) = self.bar.as_mut() {
            b.set_open_extra(None);
        }
    }

    /// Open a menu bar extra under the item ending at `right_px`, or close it if it is the one open.
    fn toggle_flyout(&mut self, kind: menubar::Extra, right_px: i32) {
        let same = self.fly.as_ref().is_some_and(|f| f.kind == kind);
        self.close_flyout();
        if same {
            return;
        }
        let content: Box<dyn flyout::Content> = match kind {
            menubar::Extra::Battery => Box::new(fly_battery::BatteryFly::new()),
            menubar::Extra::Wifi => Box::new(fly_wifi::WifiFly::new()),
            menubar::Extra::ControlCenter => Box::new(fly_cc::ControlCenter::new()),
            menubar::Extra::Tray => Box::new(fly_tray::TrayFly::new()),
            menubar::Extra::Ai => Box::new(fly_ai::AiFly::new()),
            menubar::Extra::Calendar => Box::new(fly_cal::CalFly::new()),
            menubar::Extra::Settings => return self.open_settings(),
            menubar::Extra::Launchpad => self.all_apps(),
            menubar::Extra::Spotlight => Box::new(spotlight::Spotlight::new(!self.dock.config().reduce_motion)),
        };
        let Some(b) = self.bar.as_mut() else { return };
        b.set_open_extra(Some(kind));
        let top = b.bottom_px() + if kind.centred() { (40.0 * b.dpi()) as i32 } else { 0 };
        self.fly = Some(flyout::Flyout::open(kind, content, right_px, top, b.dpi()));
    }

    /// The Settings panel, centred near the top of the screen. It stays up until closed with Esc or a click
    /// outside, so the dock can be watched while sliders move.
    fn open_settings(&mut self) {
        if let Some(w) = self.win.as_mut() {
            return w.raise();
        }
        self.close_flyout();
        let mut s = fly_settings::Settings::new(self.dock.config());
        s.set_started(self.started);
        let (top, dpi) = match self.bar.as_ref() {
            Some(b) => (b.bottom_px() + (40.0 * b.dpi()) as i32, b.dpi()),
            None => ((80.0 * self.dock.dpi) as i32, self.dock.dpi),
        };
        self.win = Some(flyout::Flyout::open(menubar::Extra::Settings, Box::new(s), 0, top, dpi));
    }

    /// Spotlight from Alt+Space or the menu bar's magnifier (works without the menu bar too).
    fn open_spotlight(&mut self) {
        if self.bar.is_some() {
            return self.toggle_flyout(menubar::Extra::Spotlight, 0);
        }
        self.close_flyout();
        let content = Box::new(spotlight::Spotlight::new(!self.dock.config().reduce_motion));
        self.fly = Some(flyout::Flyout::open(menubar::Extra::Spotlight, content, 0, (80.0 * self.dock.dpi) as i32, self.dock.dpi));
    }

    /// Launchpad from the dock's menu (works without the menu bar too).
    fn open_launchpad(&mut self) {
        if self.bar.is_some() {
            return self.toggle_flyout(menubar::Extra::Launchpad, 0);
        }
        self.close_flyout();
        let dpi = self.dock.dpi;
        let content = self.all_apps();
        self.fly = Some(flyout::Flyout::open(menubar::Extra::Launchpad, content, 0, (80.0 * dpi) as i32, dpi));
    }

    /// Every app, in the style picked in Settings: the Apps window (macOS 26) or the full-screen Launchpad.
    fn all_apps(&self) -> Box<dyn flyout::Content> {
        let c = self.dock.config();
        if c.launchpad_style == "classic" {
            Box::new(launchpad::Launchpad::new(!c.reduce_motion))
        } else {
            Box::new(apps::Apps::new(!c.reduce_motion))
        }
    }

    fn on_settings(&mut self, ch: fly_settings::Change) -> bool {
        use fly_settings::{Change, Key};
        let old = self.dock.config().clone();
        let mut done = true;
        match ch {
            Change::Open => {
                self.open_settings();
                return true;
            }
            Change::Restart => {
                restart();
                return false;
            }
            Change::OpenConfig => {
                shell::launch(&config::path());
                return true;
            }
            Change::ResetAnimations => {
                self.dock.update_config(fly_settings::reset_animations);
            }
            Change::Set(k, v, d) => {
                done = d;
                let mut changed = false;
                self.dock.update_config(|c| changed = fly_settings::apply(c, k, v));
                if !changed && !done {
                    return true;
                }
                let c = self.dock.config().clone();
                match k {
                    Key::Genie => {
                        if c.genie && self.genie.is_none() {
                            let mut g = genie::Genie::new();
                            g.start();
                            self.genie = Some(g);
                        } else if !c.genie {
                            if let Some(mut g) = self.genie.take() {
                                g.stop();
                            }
                        }
                    }
                    Key::InterfaceFont => {
                        gfx::set_interface_font(&c.interface_font);
                        if let Some(b) = self.bar.as_mut() { b.redraw(); }
                    }
                    Key::Language => {
                        i18n::set(&c.language);
                        if let Some(w) = &self.win {
                            let title = shell::wide(i18n::t("MacDock Settings"));
                            unsafe { let _ = SetWindowTextW(w.hwnd, windows::core::PCWSTR(title.as_ptr())); }
                        }
                        if let Some(b) = self.bar.as_mut() {
                            b.redraw();
                        }
                    }
                    Key::HideTaskbar => {
                        if c.hide_taskbar {
                            taskbar::hide();
                        } else {
                            taskbar::restore(false);
                        }
                    }
                    Key::BatteryPct | Key::AiUsage | Key::TrayIcons => {
                        let (o, on) = match k {
                            Key::BatteryPct => (menubar::BarOpt::BatteryPercent, c.battery_percent),
                            Key::AiUsage => (menubar::BarOpt::AiUsage, c.ai_usage),
                            _ => (menubar::BarOpt::TrayIcons, c.tray_icons),
                        };
                        if let Some(b) = self.bar.as_mut() {
                            b.set_option(o, on);
                        }
                        if k == Key::AiUsage && on && c.menu_bar {
                            ai_usage::start(c.claude_limit_5h, c.claude_usage_oauth, || {});
                        }
                    }
                    _ => {}
                }
            }
        }
        self.dock.settings_changed(&old, done);
        if old.secondary_docks != self.dock.cfg.secondary_docks { self.reconcile_displays(); }
        self.sync_config(self.dock.hwnd, done);
        self.apply_genie_timing();
        true
    }

    fn reconcile_displays(&mut self) {
        self.dock.display = display::Display::primary();
        let wanted = if self.dock.cfg.secondary_docks { display::secondary() } else { Vec::new() };
        self.secondary.retain_mut(|dock| {
            if let Some(d) = wanted.iter().find(|d| d.id == dock.display.id) {
                if *d != dock.display { dock.display = *d; dock.apply_display(); }
                true
            } else {
                destroy_dock(dock);
                false
            }
        });
        for d in wanted {
            if self.secondary.iter().any(|dock| dock.display.id == d.id) { continue; }
            let hwnd = dock::create_window();
            dropfiles::register(hwnd);
            let mut dock = Dock::new(hwnd, self.dock.cfg.clone(), d);
            dock.start();
            self.secondary.push(dock);
        }
    }

    // Propagate a saved change before another screen handles input and saves its copy.
    fn sync_config(&mut self, source: HWND, relayout: bool) {
        let cfg = if source == self.dock.hwnd {
            if !std::mem::take(&mut self.dock.config_dirty) || self.secondary.is_empty() { return; }
            self.dock.cfg.clone()
        } else {
            let Some(d) = self.secondary.iter_mut().find(|d| d.hwnd == source) else { return };
            if !std::mem::take(&mut d.config_dirty) { return; }
            self.dock.mirror_config(&d.cfg, relayout);
            self.dock.cfg.clone()
        };
        for d in &mut self.secondary { if d.hwnd != source { d.mirror_config(&cfg, relayout); } }
    }

    fn apply_genie_timing(&mut self) {
        let c = self.dock.config();
        flyout::set_motion(!c.reduce_motion);
        let secs = (c.genie_secs(false), c.genie_secs(true));
        if let Some(g) = self.genie.as_mut() {
            g.secs = secs;
        }
    }

    fn handle(&mut self, e: Ev) -> bool {
        match e {
            Ev::Dock(source, de) => {
                let primary = source == self.dock.hwnd;
                if !primary && matches!(de, DockEv::DisplayChanged) {
                    // A DPI change can be delivered only to the window on the affected monitor.
                    self.dock.handle(DockEv::DisplayChanged);
                    return true;
                }
                if primary && matches!(de, DockEv::DisplaySettled) { self.reconcile_displays(); }
                if primary && matches!(de, DockEv::Backdrop(true)) {
                    for d in &mut self.secondary { d.handle(DockEv::Backdrop(true)); }
                }
                if primary && matches!(de, DockEv::Refresh) {
                    if let Some(pid) = taskbar::explorer_pid() {
                        if self.explorer_pid != Some(pid) {
                            self.explorer_pid = Some(pid);
                            if let Some(b) = self.bar.as_mut() {
                                b.shell_restarted();
                            }
                            self.dock.shell_restarted();
                            for d in &mut self.secondary { d.shell_restarted(); }
                        }
                        if let Some(b) = self.bar.as_mut() {
                            b.ensure_reservation();
                        }
                        self.dock.ensure_reservation();
                        for d in &mut self.secondary { d.ensure_reservation(); }
                    }
                    // Explorer's ABN_FULLSCREENAPP is unreliable with its taskbar hidden: ask on every refresh
                    self.update_fullscreen();
                }
                let action = if primary { self.dock.handle(de) } else {
                    let Some(d) = self.secondary.iter_mut().find(|d| d.hwnd == source) else { return true };
                    d.handle(de)
                };
                self.sync_config(source, true);
                if let Some(a) = action { return self.act(a, source); }
            }
            Ev::Foreground(h) => {
                for d in &mut self.secondary { d.on_foreground(HWND(h as *mut _)); }
                self.keep_over_desktop(HWND(h as *mut _));
                self.dock.on_foreground(HWND(h as *mut _));
                if let Some(b) = self.bar.as_mut() {
                    b.on_foreground(HWND(h as *mut _));
                }
                if let Some(g) = self.genie.as_mut() {
                    g.on_foreground_or_moved(HWND(h as *mut _));
                }
            }
            Ev::MoveSizeEnd(h) => {
                if let Some(g) = self.genie.as_mut() {
                    g.on_foreground_or_moved(HWND(h as *mut _));
                }
            }
            Ev::MinimizeStart(h) => {
                if self.genie.is_some() {
                    let icon = self.icon_for(HWND(h as *mut _), None);
                    self.genie.as_mut().unwrap().on_minimize_start(HWND(h as *mut _), icon);
                }
            }
            Ev::Bar(be) => {
                let Some(b) = self.bar.as_mut() else { return true };
                b.auto_hide_dock = self.dock.auto_hide();
                match b.handle(be) {
                    Some(menubar::BarAct::Quit) => return false,
                    Some(menubar::BarAct::Restart) => {
                        restart();
                        return false;
                    }
                    Some(menubar::BarAct::ToggleAutoHide) => {
                        let on = !self.dock.auto_hide();
                        self.dock.set_auto_hide(on);
                    }
                    Some(menubar::BarAct::Fullscreen) => self.update_fullscreen(),
                    Some(menubar::BarAct::Option(o, on)) => {
                        let c = self.dock.update_config(|c| match o {
                            menubar::BarOpt::BatteryPercent => c.battery_percent = on,
                            menubar::BarOpt::AiUsage => c.ai_usage = on,
                            menubar::BarOpt::TrayIcons => c.tray_icons = on,
                        });
                        if o == menubar::BarOpt::AiUsage && on {
                            ai_usage::start(c.claude_limit_5h, c.claude_usage_oauth, || {});
                        }
                    }
                    Some(menubar::BarAct::Flyout(kind, _, r)) => self.toggle_flyout(kind, r),
                    Some(menubar::BarAct::OpenSettings) => self.open_settings(),
                    None => {}
                }
            }
            Ev::Fly(h, fe) if self.win.as_ref().is_some_and(|w| w.hwnd.0 as isize == h) => {
                // the Settings window: only its red light (or Esc) ends it
                if let Some(cmd) = self.win.as_mut().and_then(|w| w.handle(fe)) {
                    if matches!(cmd, flyout::Cmd::Close) {
                        if let Some(mut w) = self.win.take() {
                            w.close();
                        }
                    }
                }
            }
            Ev::Fly(h, fe) => {
                if let Some(cmd) = self.fly.as_mut().filter(|f| f.hwnd.0 as isize == h).and_then(|f| f.handle(fe)) {
                    self.close_flyout();
                    match cmd {
                        flyout::Cmd::Launch(t) => shell::launch(&t),
                        flyout::Cmd::Pin(t) => self.dock.pin(&t),
                        flyout::Cmd::Settings => self.open_settings(),
                        flyout::Cmd::OpenWifi => {
                            if let Some((_, r)) = self.bar.as_ref().and_then(|b| b.extra_span(menubar::Extra::Wifi)) {
                                self.toggle_flyout(menubar::Extra::Wifi, r);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ev::Settings(ch) => return self.on_settings(ch),
            Ev::Spotlight => self.open_spotlight(),
            Ev::Genie(ge) => {
                if let Some(g) = self.genie.as_mut() {
                    g.handle(ge);
                }
            }
        }
        self.sync_config(self.dock.hwnd, true);
        true
    }
}

fn destroy_dock(d: &mut Dock) {
    d.shutdown();
    dropfiles::unregister(d.hwnd);
    unsafe { let _ = DestroyWindow(d.hwnd); }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive Windows desktop with an extended display"]
    fn secondary_toggle_preserves_primary_and_does_not_quit() {
        let displays = display::secondary();
        assert!(!displays.is_empty(), "connect an extended display");
        unsafe { let _ = windows::Win32::System::Ole::OleInitialize(None); }
        let hwnd = dock::create_window();
        let mut cfg = Config::default();
        cfg.auto_hide = true; // do not reserve work area or alter the running shell in this test
        cfg.hide_taskbar = false;
        cfg.badges = false;
        let mut app = App { dock: Dock::new(hwnd, cfg, display::Display::primary()), secondary: Vec::new(),
            genie: None, bar: None, fly: None, win: None, fullscreen: false, explorer_pid: None, started: [false; 4] };
        app.reconcile_displays();
        assert_eq!(app.secondary.len(), displays.len());
        let first = app.secondary[0].hwnd;
        app.reconcile_displays();
        assert_eq!(app.secondary[0].hwnd, first);
        app.dock.cfg.secondary_docks = false;
        app.reconcile_displays();
        assert!(app.secondary.is_empty());
        unsafe {
            assert!(!IsWindow(first).as_bool());
            assert!(IsWindow(hwnd).as_bool());
            assert!(!PeekMessageW(&mut MSG::default(), HWND::default(), WM_QUIT, WM_QUIT, PM_NOREMOVE).as_bool());
            DestroyWindow(hwnd).unwrap();
            windows::Win32::System::Ole::OleUninitialize();
        }
    }
}

/// Start a fresh copy that waits for this one to quit (the caller then returns false to end the loop).
fn restart() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--restart").spawn();
    }
}

/// Below medium integrity (a Low Mandatory Label on the exe or its folder carries over to the process).
fn low_integrity() -> bool {
    use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut buf = [0u8; 128];
        let mut n = 0u32;
        let ok = GetTokenInformation(tok, TokenIntegrityLevel, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, &mut n).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(tok);
        if !ok {
            return false;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid) as u32;
        *GetSidSubAuthority(label.Label.Sid, count - 1) < 0x2000
    }
}

fn class_of(h: HWND) -> String {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut cls) } as usize;
    String::from_utf16_lossy(&cls[..n])
}

fn is_desktop(h: HWND) -> bool {
    matches!(class_of(h).as_str(), "Progman" | "WorkerW")
}

/// The desktop sits above every app window (show desktop is on), not at the bottom as usual. The first
/// visible, uncloaked, non-topmost window from the top tells: the desktop itself, or an app.
fn desktop_raised() -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    unsafe {
        let me = std::process::id();
        let mut h = GetTopWindow(None).unwrap_or_default();
        while !h.0.is_null() {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
            if IsWindowVisible(h).as_bool() && cloaked == 0 && shell::window_pid(h) != me {
                if is_desktop(h) {
                    return true;
                }
                if ex & WS_EX_TOPMOST.0 as isize == 0 && ex & WS_EX_TOOLWINDOW.0 as isize == 0 {
                    return false;
                }
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
        false
    }
}

fn foreground_fullscreen_rect() -> Option<RECT> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || shell::window_pid(fg) == std::process::id() {
            return None;
        }
        // screenshot / screen tool overlays cover the screen for a moment; hiding for them made dock and bar
        // blink away on every Win+Shift+S. Tool windows (no taskbar button) are never a game either.
        if GetWindowLongPtrW(fg, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0 {
            return None;
        }
        const OVERLAYS: [&str; 9] = [
            "ScreenClippingHost.exe", "SnippingTool.exe", "ScreenSketch.exe", "ShareX.exe", "Lightshot.exe",
            "Greenshot.exe", "PowerToys.ScreenRuler.exe", "PowerToys.PowerOCR.exe", "PowerToys.ColorPickerUI.exe",
        ];
        if shell::exe_name(shell::window_pid(fg)).is_some_and(|n| OVERLAYS.iter().any(|o| shell::eq_ic(o, &n))) {
            return None;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(fg, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if ["Progman", "WorkerW", "Shell_TrayWnd"].contains(&cls.as_str()) {
            return None;
        }
        // anything with a title bar is not fullscreen, even when it covers the whole screen because no AppBar
        // space is reserved; a fullscreen app sits exactly on the monitor without one. Not IsZoomed: Chrome and
        // Edge keep WS_MAXIMIZE when a maximized window goes fullscreen (F11, YouTube), only the caption goes.
        if GetWindowLongPtrW(fg, GWL_STYLE) & WS_CAPTION.0 as isize == WS_CAPTION.0 as isize {
            return None;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return None;
        }
        Some(r)
    }
}

fn main() {
    if std::env::args().any(|a| a == "--restore") {
        // escape hatch if the dock was killed while the taskbar was hidden
        taskbar::restore(true);
        genie::restore_system_animation();
        return;
    }
    // no console in a windows-subsystem exe: keep the last panic where it can be read
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::create_dir_all(config::dir());
        let bt = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(format!("{}\\panic.log", config::dir()), format!("{info}\n{bt}"));
        taskbar::restore(true);
        genie::restore_system_animation();
    }));
    // one dock at a time; the C# build uses the same name, so the two never run together
    // after "Restart MacDock" the old process may still be shutting down: wait for it a few seconds
    let tries = if std::env::args().any(|a| a == "--restart") { 200 } else { 1 }; // up to 20 s
    // Owning the mutex is what counts, not its name existing: a copy stuck on its way out (one thread caught in a
    // driver, 25 Nov 2026) still holds the name open after the thread that owned it is gone, and that is
    // WAIT_ABANDONED here, free to take, where testing the name alone kept every new copy out until a reboot.
    let mut _single = None;
    for i in 0..tries {
        if let Ok(m) = unsafe { CreateMutexW(None, false, w!("MacDock.Singleton")) } {
            use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
            let w = unsafe { windows::Win32::System::Threading::WaitForSingleObject(m, 0) };
            if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
                _single = Some(Ok::<_, windows::core::Error>(m));
                break;
            }
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(m);
            }
        }
        if i + 1 < tries {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    if _single.is_none() {
        return;
    }
    let cfg = Config::load();
    i18n::set(&cfg.language);
    gfx::set_interface_font(&cfg.interface_font);
    if low_integrity() {
        // Explorer refuses AppBars from a low-integrity process and %APPDATA% is read-only to it: this happens
        // when the exe sits in a folder with a Low Mandatory Label (e.g. a sandboxed build folder)
        unsafe {
            MessageBoxW(
                None,
                windows::core::PCWSTR(shell::wide(i18n::t("MacDock is running at low integrity, so it cannot reserve screen space or save settings.

Copy macdock.exe to a normal folder (for example %LOCALAPPDATA%\\Programs\\MacDock) and start it from there.")).as_ptr()),
                w!("MacDock"),
                MB_OK | MB_ICONWARNING,
            );
        }
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = windows::Win32::System::Ole::OleInitialize(None); // drag and drop onto the dock
    }
    if cfg.hide_taskbar {
        taskbar::hide();
    }
    let hwnd = dock::create_window();
    dropfiles::register(hwnd);
    let genie = cfg.genie.then(genie::Genie::new);
    let bar = cfg.menu_bar.then(|| menubar::MenuBar::new(menubar::create_window(), cfg.tray_icons, cfg.ai_usage, cfg.battery_percent));
    if cfg.menu_bar && cfg.ai_usage {
        ai_usage::start(cfg.claude_limit_5h, cfg.claude_usage_oauth, || {});
    }
    let want_widgets = cfg.desktop_widgets;
    let want_switcher = cfg.app_switcher;
    let started = [cfg.menu_bar, want_widgets, want_switcher, cfg.claude_usage_oauth];
    let mut app = App { dock: Dock::new(hwnd, cfg, display::Display::primary()), secondary: Vec::new(), genie, bar, fly: None, win: None, fullscreen: false, explorer_pid: taskbar::explorer_pid(), started };
    app.apply_genie_timing();
    if app.dock.config().launchpad_style == "classic" {
        flyout::warm_backdrop(); // only the full-screen Launchpad draws over it (~8 MB at 1080p)
    }
    if let Some(b) = app.bar.as_mut() {
        b.start(); // reserve the top strip before the dock measures the work area
    }
    app.dock.start();
    app.reconcile_displays();
    if want_widgets {
        widgets::start(); // after the bar and dock reserved their strips, so the work area is final
    }
    if want_switcher {
        switcher::start();
    }
    update::cleanup();
    update::start();
    if std::env::args().any(|a| a == "--settings") {
        app.open_settings();
    }
    if let Some(g) = app.genie.as_mut() {
        g.start();
    }
    let hooks: Vec<HWINEVENTHOOK> = [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MINIMIZESTART]
        .iter()
        .map(|&e| unsafe { SetWinEventHook(e, e, None, Some(on_win_event), 0, 0, WINEVENT_SKIPOWNPROCESS) })
        .collect();

    unsafe {
        // Spotlight on Alt+Space (macOS has Cmd+Space; Win+Space is taken by the keyboard layouts)
        use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_SPACE};
        let _ = RegisterHotKey(HWND::default(), 1, MOD_ALT | MOD_NOREPEAT, VK_SPACE.0 as u32);
        let mut msg = MSG::default();
        'run: loop {
            if !app.awake() {
                // asleep: block until something happens
                if !GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                    break;
                }
                if msg.message == WM_HOTKEY {
                    push(Ev::Spotlight);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
                }
                if msg.message == WM_HOTKEY {
                    push(Ev::Spotlight);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            loop {
                let events: Vec<Ev> = EVENTS.with(|q| q.borrow_mut().drain(..).collect());
                if events.is_empty() {
                    break;
                }
                for e in events {
                    if !app.handle(e) {
                        break 'run;
                    }
                }
            }
            if app.awake() {
                let t0 = std::time::Instant::now();
                if app.dock.awake {
                    app.dock.frame();
                }
                for d in &mut app.secondary { if d.awake { d.frame(); } }
                let t1 = std::time::Instant::now();
                if let Some(g) = app.genie.as_mut() {
                    g.frame();
                }
                let t2 = std::time::Instant::now();
                let _ = DwmFlush(); // pace to the compositor, one frame per refresh
                genie::loop_times((t1 - t0).as_secs_f64() * 1e3, t2.elapsed().as_secs_f64() * 1e3);
            }
        }
        for h in hooks {
            let _ = UnhookWinEvent(h);
        }
        if let Some(g) = app.genie.as_mut() {
            g.stop();
        }
        if let Some(b) = app.bar.as_mut() {
            b.shutdown();
        }
        for d in &mut app.secondary { destroy_dock(d); }
        app.dock.shutdown();
        dropfiles::unregister(hwnd);
        taskbar::restore(false);
        let _ = DestroyWindow(hwnd);
    }
    // Our own clean-up is done (taskbar back, AppBars removed, config saved as it changed), so end here without the
    // DLL detach a return from main runs. On 25 Nov 2026 a quit for an update left this thread spinning at 100% of
    // a core in user mode after main returned, unkillable ("no running instance of the task") and holding the
    // mutex, so neither the update nor any later start could run until a reboot. ExitProcess kills the other
    // threads before DLL_PROCESS_DETACH, and a graphics driver's detach waiting on one of them is the usual way
    // that happens. Release the mutex first so a restart waiting on it starts at once.
    if let Some(Ok(m)) = _single {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(m);
            let _ = windows::Win32::Foundation::CloseHandle(m);
        }
    }
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), 0);
    }
}
