//! MacDock in Rust: dock (and, as the port goes on, menu bar and genie) on plain Win32. Every window's
//! procedure only queues events; the App below handles them and runs one frame loop paced by DwmFlush
//! that sleeps in GetMessage once nothing moves.

#![windows_subsystem = "windows"]

mod badges;
mod config;
mod dock;
mod genie;
mod gfx;
mod menu;
mod menubar;
mod shell;
mod taskbar;
mod tile;

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

use config::Config;
use dock::{Act, Dock, DockEv};

pub enum Ev {
    Dock(DockEv),
    Genie(genie::GenieEv),
    Bar(menubar::BarEv),
    Foreground(isize),
    MinimizeStart(isize),
    MoveSizeEnd(isize),
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
    genie: Option<genie::Genie>,
    bar: Option<menubar::MenuBar>,
    fullscreen: bool,
}

impl App {
    fn awake(&self) -> bool {
        self.dock.awake || self.genie.as_ref().is_some_and(|g| g.busy())
    }

    /// The dock icon a window belongs to, in screen pixels.
    fn icon_for(&mut self, h: HWND) -> genie::R {
        let (x, y, w, hh) = self.dock.icon_rect_for(h);
        let d = self.dock.dpi;
        genie::R { x: x * d, y: y * d, w: w * d, h: hh * d }
    }

    /// Returns false to quit.
    fn act(&mut self, a: Act) -> bool {
        match a {
            Act::Quit => return false,
            Act::Fullscreen(on) => self.set_fullscreen(on),
            Act::Minimize { fg, wins } => {
                let icon = self.icon_for(fg);
                match self.genie.as_mut() {
                    Some(g) => g.minimize_from_dock(fg, wins, icon),
                    None => genie::minimize_all(&wins),
                }
            }
            Act::Restore { front, minimized } => {
                let icon = self.icon_for(front);
                match self.genie.as_mut() {
                    Some(g) => g.restore(front, minimized, icon),
                    None => genie::restore_all(front, &minimized),
                }
            }
        }
        true
    }

    fn set_fullscreen(&mut self, on: bool) {
        if on && !foreground_is_fullscreen() {
            return; // e.g. our own overlays; only hide for a real fullscreen app
        }
        if on != self.fullscreen {
            self.fullscreen = on;
            self.dock.set_fullscreen_hidden(on);
            if let Some(g) = self.genie.as_mut() {
                g.pause(on); // game mode: nothing of ours in the input path
            }
            if let Some(b) = self.bar.as_mut() {
                b.set_hidden(on);
            }
        }
    }

    fn handle(&mut self, e: Ev) -> bool {
        match e {
            Ev::Dock(de) => {
                if let DockEv::Refresh = de {
                    // Explorer's ABN_FULLSCREENAPP is unreliable with its taskbar hidden: ask on every refresh
                    let fs = foreground_is_fullscreen();
                    if fs != self.fullscreen {
                        self.set_fullscreen(fs);
                    }
                }
                if let Some(a) = self.dock.handle(de) {
                    return self.act(a);
                }
            }
            Ev::Foreground(h) => {
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
                    let icon = self.icon_for(HWND(h as *mut _));
                    self.genie.as_mut().unwrap().on_minimize_start(HWND(h as *mut _), icon);
                }
            }
            Ev::Bar(be) => {
                let Some(b) = self.bar.as_mut() else { return true };
                b.auto_hide_dock = self.dock.auto_hide();
                match b.handle(be) {
                    Some(menubar::BarAct::Quit) => return false,
                    Some(menubar::BarAct::ToggleAutoHide) => {
                        let on = !self.dock.auto_hide();
                        self.dock.set_auto_hide(on);
                    }
                    Some(menubar::BarAct::Fullscreen(on)) => self.set_fullscreen(on),
                    Some(menubar::BarAct::Flyout(kind, _, _)) => {
                        // flyouts not ported yet: open the matching Settings page
                        shell::launch(match kind {
                            menubar::Extra::Tray => "ms-settings:taskbar",
                            menubar::Extra::Battery => "ms-settings:batterysaver",
                            menubar::Extra::Wifi => "ms-settings:network-wifi",
                            menubar::Extra::ControlCenter => "ms-actioncenter:controlcenter",
                        });
                    }
                    None => {}
                }
            }
            Ev::Genie(ge) => {
                if let Some(g) = self.genie.as_mut() {
                    g.handle(ge);
                }
            }
        }
        true
    }
}

fn foreground_is_fullscreen() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || shell::window_pid(fg) == std::process::id() {
            return false;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(fg, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if ["Progman", "WorkerW", "Shell_TrayWnd"].contains(&cls.as_str()) {
            return false;
        }
        // a maximized window (or anything with a title bar) is not fullscreen, even when it covers the whole
        // screen because no AppBar space is reserved; a fullscreen app sits exactly on the monitor without one
        if IsZoomed(fg).as_bool() || GetWindowLongPtrW(fg, GWL_STYLE) & WS_CAPTION.0 as isize == WS_CAPTION.0 as isize {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        r.left == 0 && r.top == 0 && r.right == GetSystemMetrics(SM_CXSCREEN) && r.bottom == GetSystemMetrics(SM_CYSCREEN)
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
    let _single = unsafe { CreateMutexW(None, true, w!("MacDock.Singleton")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let cfg = Config::load();
    if cfg.hide_taskbar {
        taskbar::hide();
    }
    let hwnd = dock::create_window();
    let genie = cfg.genie.then(genie::Genie::new);
    let bar = cfg.menu_bar.then(|| menubar::MenuBar::new(menubar::create_window(), cfg.tray_icons));
    let mut app = App { dock: Dock::new(hwnd, cfg), genie, bar, fullscreen: false };
    if let Some(b) = app.bar.as_mut() {
        b.start(); // reserve the top strip before the dock measures the work area
    }
    app.dock.start();
    if let Some(g) = app.genie.as_mut() {
        g.start();
    }
    let hooks: Vec<HWINEVENTHOOK> = [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MINIMIZESTART]
        .iter()
        .map(|&e| unsafe { SetWinEventHook(e, e, None, Some(on_win_event), 0, 0, WINEVENT_SKIPOWNPROCESS) })
        .collect();

    unsafe {
        let mut msg = MSG::default();
        'run: loop {
            if !app.awake() {
                // asleep: block until something happens
                if !GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
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
                if app.dock.awake {
                    app.dock.frame();
                }
                if let Some(g) = app.genie.as_mut() {
                    g.frame();
                }
                let _ = DwmFlush(); // pace to the compositor, one frame per refresh
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
        app.dock.shutdown();
        taskbar::restore(false);
        let _ = DestroyWindow(hwnd);
    }
}
