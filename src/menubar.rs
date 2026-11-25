//! macOS-style menu bar (port of MenuBarWindow.cs): Windows logo menu, the active app's name, generic app
//! menus that send real shortcuts to it, then status items (tray chevron, battery, network, search, Control
//! Center) and the clock. One layered window across the top of the primary screen with an acrylic accent
//! behind it, reserved as an AppBar so maximized windows stop under it. Drawn only when something changes.

use std::collections::HashMap;
use std::time::Duration;

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{GetSystemPowerStatus, SetSuspendState, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, TrackMouseEvent, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Font, Surface};
use crate::menu::{self, Anchor, Entry};
use crate::shell::{self, Shell};
use crate::{push, Ev};

const H: f64 = 30.0;
const FONT_PX: f64 = 13.5;
const GLYPH_PX: f64 = 15.0;
const PAD_X: f64 = 9.0; // inside each button
const GAP: f64 = 1.0; // between buttons
const SIDE: f64 = 10.0; // from the screen edges
const CB_APPBAR: u32 = WM_APP + 21;
/// an update worker finished (update::take_install has its result)
const WM_UPDATE_DONE: u32 = WM_APP + 22;
const TIMER_TICK: usize = 1;
const TIMER_DISPLAY: usize = 2;
/// a new wallpaper has faded in: read the desktop again
const TIMER_WALLPAPER: usize = 3;
const WM_MOUSELEAVE: u32 = 0x02A3;

const VK_CONTROL: u8 = 0x11;
const VK_SHIFT: u8 = 0x10;
const VK_LWIN: u8 = 0x5B;
const VK_F11: u8 = 0x7A;

pub enum BarEv {
    UpdateDone,
    Move(i32),
    Leave,
    Up(i32),
    RightUp(i32),
    Timer(usize),
    AppBar(usize, isize),
    Display,
    Wallpaper,
}

/// What the bar asks the app to do.
pub enum BarAct {
    Quit,
    /// an update was put in place: start it and quit
    Restart,
    ToggleAutoHide,
    Fullscreen,
    /// a menu bar setting was switched; the app saves it to config.json
    Option(BarOpt, bool),
    /// open (or close, if it is the open one) a menu bar extra under the item spanning these screen px
    Flyout(Extra, i32, i32),
    OpenSettings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarOpt {
    BatteryPercent,
    AiUsage,
    TrayIcons,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extra {
    Tray,
    Ai,
    Battery,
    Wifi,
    ControlCenter,
    Calendar,
    Settings,
    Launchpad,
    Spotlight,
}

impl Extra {
    /// A panel of its own in the middle of the screen, not a drop-down under its menu bar item.
    pub fn centred(self) -> bool {
        matches!(self, Extra::Settings | Extra::Launchpad | Extra::Spotlight)
    }

    /// A window of its own (traffic lights, stays open, can be dragged), not a drop-down.
    pub fn is_window(self) -> bool {
        self == Extra::Settings
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Logo,
    App,
    Menu(&'static str),
    Chevron,
    Ai,
    Battery,
    Net,
    Apps,
    Search,
    Cc,
    Clock,
}

struct Slot {
    item: Item,
    x: f64, // DIPs, button left
    w: f64, // button width (with padding)
}

// ------------------------------------------------------------------ status sources

fn power() -> Option<(u8, bool, bool)> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).ok()? };
    if ps.BatteryFlag == 128 || ps.BatteryLifePercent > 100 {
        return None; // no battery
    }
    let on_ac = ps.ACLineStatus == 1;
    Some((ps.BatteryLifePercent, on_ac, on_ac && ps.BatteryFlag & 8 != 0))
}

/// What the network icon shows.
#[derive(Clone, Copy, PartialEq)]
pub enum Net {
    Wifi(u8), // bars 0-3
    Wired,
    Off,
}

pub mod net {
    use windows::Networking::Connectivity::{NetworkConnectivityLevel, NetworkInformation};

    /// Connected SSID and its signal bars (0-3). Cheap: no scan.
    pub fn current() -> Option<(String, u8)> {
        let p = NetworkInformation::GetInternetConnectionProfile().ok()?;
        if !p.IsWlanConnectionProfile().ok()? {
            return None;
        }
        let ssid = p.WlanConnectionProfileDetails().ok()?.GetConnectedSsid().ok()?.to_string();
        let bars = p.GetSignalBars().ok().and_then(|r| r.Value().ok()).map(bars).unwrap_or(3);
        (!ssid.is_empty()).then_some((ssid, bars))
    }

    pub fn bars(signal: u8) -> u8 {
        match signal {
            4.. => 3,
            2 | 3 => 2,
            1 => 1,
            _ => 0,
        }
    }

    /// On a wired (or other non-Wi-Fi) connection.
    pub fn wired() -> bool {
        let Ok(p) = NetworkInformation::GetInternetConnectionProfile() else { return false };
        !p.IsWlanConnectionProfile().unwrap_or(true)
            && !p.IsWwanConnectionProfile().unwrap_or(true)
            && p.GetNetworkConnectivityLevel().map(|l| l != NetworkConnectivityLevel::None).unwrap_or(false)
    }
}

fn read_net() -> Net {
    match net::current() {
        Some((_, b)) => Net::Wifi(b),
        None if net::wired() => Net::Wired,
        None => Net::Off,
    }
}

fn clock_text() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = unsafe { GetLocalTime() };
    format!(
        "{} {} {}   {:02}:{:02}",
        DAYS[t.wDayOfWeek as usize % 7], t.wDay, MONTHS[(t.wMonth as usize + 11) % 12], t.wHour, t.wMinute
    )
}

// ------------------------------------------------------------------ input and system helpers

/// Press the keys in order, release them in reverse.
pub fn key(vks: &[u8]) {
    unsafe {
        for &vk in vks {
            keybd_event(vk, 0, KEYBD_EVENT_FLAGS(0), 0);
        }
        for &vk in vks.iter().rev() {
            keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        }
    }
}

fn confirm(owner: HWND, q: &str) -> bool {
    let t = shell::wide(q);
    unsafe {
        MessageBoxW(owner, windows::core::PCWSTR(t.as_ptr()), w!("MacDock"), MB_OKCANCEL | MB_ICONQUESTION | MB_TOPMOST) == IDOK
    }
}

fn inform(owner: HWND, msg: &str) {
    let t = shell::wide(msg);
    unsafe {
        MessageBoxW(owner, windows::core::PCWSTR(t.as_ptr()), w!("MacDock"), MB_OK | MB_ICONINFORMATION | MB_TOPMOST);
    }
}

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

fn at_login() -> bool {
    unsafe {
        let mut len = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, w!("MacDock"), RRF_RT_REG_SZ, None, None, Some(&mut len)).is_ok()
    }
}

fn toggle_login() {
    unsafe {
        let mut k = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE | KEY_QUERY_VALUE, &mut k).is_err() {
            return;
        }
        if at_login() {
            let _ = RegDeleteValueW(k, w!("MacDock"));
        } else if let Ok(exe) = std::env::current_exe() {
            let v = shell::wide(&format!("\"{}\"", exe.display()));
            let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
            let _ = RegSetValueExW(k, w!("MacDock"), 0, REG_SZ, Some(bytes));
        }
        let _ = RegCloseKey(k);
    }
}

// ------------------------------------------------------------------ window

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let x = (l.0 & 0xFFFF) as i16 as i32;
    match msg {
        WM_MOUSEMOVE => push(Ev::Bar(BarEv::Move(x))),
        WM_MOUSELEAVE => push(Ev::Bar(BarEv::Leave)),
        WM_LBUTTONUP => push(Ev::Bar(BarEv::Up(x))),
        WM_RBUTTONUP => push(Ev::Bar(BarEv::RightUp(x))),
        WM_TIMER => push(Ev::Bar(BarEv::Timer(w.0))),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE => push(Ev::Bar(BarEv::Display)),
        WM_SETTINGCHANGE if w.0 == SPI_SETDESKWALLPAPER.0 as usize => push(Ev::Bar(BarEv::Wallpaper)),
        WM_DPICHANGED => {
            push(Ev::Bar(BarEv::Display));
            return LRESULT(0); // we place ourselves
        }
        CB_APPBAR => push(Ev::Bar(BarEv::AppBar(w.0, l.0))),
        WM_UPDATE_DONE => push(Ev::Bar(BarEv::UpdateDone)),
        _ => {}
    }
    DefWindowProcW(h, msg, w, l)
}

pub fn create_window() -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockMenuBar"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("MacDockMenuBar"), w!("MacDock Menu Bar"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        )
        .unwrap()
    }
}

// ------------------------------------------------------------------ the bar

pub struct MenuBar {
    pub hwnd: HWND,
    shell: Shell,
    dpi: f64,
    screen_w: f64,
    screen_h: f64,
    appbar: bool,
    hidden: bool,
    tray: bool,
    ai: bool,
    battery_pct: bool,
    ai_gen: u64,
    ai_label: String,
    ai_pct: Option<f64>,
    ai_live: bool,
    surface: Option<Surface>,
    dark_text: bool, // the wallpaper behind the bar is bright, so the content is drawn black
    slots: Vec<Slot>,
    hover: Option<usize>,
    open: Option<usize>,  // its menu or flyout is up
    target: HWND,         // the last foreground window that isn't ours
    app_name: String,
    names: HashMap<u32, String>,
    clock: String,
    update: bool, // a newer release is out: dot on the logo, item in its menu
    battery: Option<(u8, bool, bool)>,
    net: Net,
    ticks: u32,
    tracking: bool,
    dirty: bool,
    pub auto_hide_dock: bool,
}

impl MenuBar {
    pub fn new(hwnd: HWND, tray: bool, ai: bool, battery_pct: bool) -> MenuBar {
        let (sw, sh, dpi) = crate::dock::primary_screen();
        let b = power();
        let n = read_net();
        MenuBar {
            hwnd, shell: Shell::default(), dpi, screen_w: sw, screen_h: sh, appbar: false, hidden: false, tray, battery_pct,
            ai, ai_gen: u64::MAX, ai_label: String::new(), ai_pct: None, ai_live: false,
            surface: None, dark_text: false, slots: Vec::new(), hover: None, open: None, target: HWND::default(),
            app_name: "File Explorer".into(), names: HashMap::new(), clock: clock_text(), update: false, battery: b, net: n,
            ticks: 0, tracking: false, dirty: true, auto_hide_dock: false,
        }
    }

    pub fn start(&mut self) {
        self.load_frost();
        unsafe {
            SetTimer(self.hwnd, TIMER_TICK, 1000, None);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.place();
        self.on_foreground(unsafe { GetForegroundWindow() });
        self.render();
    }

    pub fn shutdown(&mut self) {
        self.unreserve();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    fn px(&self, v: f64) -> i32 {
        (v * self.dpi).round() as i32
    }

    /// Stretch across the primary screen and reserve the strip.
    fn place(&mut self) {
        let (sw, sh, dpi) = crate::dock::primary_screen();
        (self.screen_w, self.screen_h, self.dpi) = (sw, sh, dpi);
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, self.px(sw), self.px(H), SWP_NOACTIVATE);
        }
        self.surface = None;
        self.dirty = true;
        if !self.hidden {
            self.reserve();
        }
    }

    fn appbar_data(&self) -> APPBARDATA {
        APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uCallbackMessage: CB_APPBAR,
            uEdge: ABE_TOP,
            ..Default::default()
        }
    }

    fn reserve(&mut self) {
        let mut d = self.appbar_data();
        unsafe {
            if !self.appbar {
                if crate::taskbar::appbar_message(ABM_NEW, &mut d) == 0 {
                    return; // refused (e.g. we run at low integrity); maximized windows will slide under us
                }
                self.appbar = true;
            }
            d.rc = RECT { left: 0, top: 0, right: self.px(self.screen_w), bottom: self.px(H) };
            crate::taskbar::appbar_message(ABM_QUERYPOS, &mut d);
            d.rc.bottom = d.rc.top + self.px(H);
            crate::taskbar::appbar_message(ABM_SETPOS, &mut d);
        }
    }

    /// Explorer owns AppBar registrations; a new shell starts with an empty list.
    pub fn shell_restarted(&mut self) {
        self.appbar = false;
        self.reserve();
    }

    pub fn ensure_reservation(&mut self) {
        if !self.appbar {
            self.reserve();
        }
    }

    fn unreserve(&mut self) {
        if self.appbar {
            let mut d = self.appbar_data();
            unsafe {
                crate::taskbar::appbar_message(ABM_REMOVE, &mut d);
            }
            self.appbar = false;
        }
    }

    /// Out of the way while a fullscreen app (a game, a video) is in front.
    pub fn set_hidden(&mut self, on: bool) {
        if on == self.hidden {
            return;
        }
        self.hidden = on;
        unsafe {
            if on {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                let _ = KillTimer(self.hwnd, TIMER_TICK);
            } else {
                SetTimer(self.hwnd, TIMER_TICK, 1000, None);
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.tick(true);
                self.dirty = true;
                self.render();
            }
        }
    }

    pub fn on_foreground(&mut self, h: HWND) {
        if h.0.is_null() {
            return;
        }
        let pid = shell::window_pid(h);
        if pid == std::process::id() {
            return;
        }
        self.target = h;
        let name = match self.names.get(&pid) {
            Some(n) => n.clone(),
            None => {
                let (exe, aumid) = self.shell.process_info(pid);
                let aumid = shell::window_aumid(h).or(aumid);
                let is = |suffix: &str| exe.as_deref().is_some_and(|e| e.to_lowercase().ends_with(suffix));
                let name = if is("\\explorer.exe") {
                    "File Explorer".to_string()
                } else {
                    shell::friendly_name(exe.as_deref(), aumid.as_deref(), "File Explorer")
                };
                // a bare exe name ("notepad") reads better capitalized, like a real app name
                let mut c = name.chars();
                let name = match c.next() {
                    Some(f) if f.is_lowercase() => f.to_uppercase().chain(c).collect(),
                    _ => name,
                };
                // ApplicationFrameHost hosts many UWP apps under one pid: don't cache those
                if !is("applicationframehost.exe") {
                    self.names.insert(pid, name.clone());
                }
                name
            }
        };
        if name != self.app_name {
            self.app_name = name;
            self.dirty = true;
            self.render();
        }
    }

    /// Clock every second; battery too; the network every 5 s.
    fn tick(&mut self, all: bool) {
        self.ticks += 1;
        let c = clock_text();
        if c != self.clock {
            self.clock = c;
            self.dirty = true;
        }
        let u = crate::update::available().is_some();
        if u != self.update {
            self.update = u;
            self.dirty = true;
        }
        let b = power();
        if b != self.battery {
            self.battery = b;
            self.dirty = true;
        }
        if self.ai {
            self.read_ai();
        }
        if all || self.ticks % 5 == 1 {
            let n = read_net();
            if n != self.net {
                self.net = n;
                self.dirty = true;
            }
        }
    }

    /// Pick up a new AI usage summary: ring = the limit closest to running out, text = burn rate while busy.
    fn read_ai(&mut self) {
        let g = crate::ai_usage::generation();
        if g == self.ai_gen {
            return;
        }
        self.ai_gen = g;
        let Some(s) = crate::ai_usage::summary() else { return };
        let r = s.rate5();
        let pct = s.worst_pct();
        let label = if r > 0.0 {
            format!("{}/s", crate::ai_usage::compact(r))
        } else {
            pct.map(|p| format!("{p:.0}%")).unwrap_or_default()
        };
        let live = s.providers().next().is_some();
        if label != self.ai_label || pct != self.ai_pct || live != self.ai_live {
            self.ai_label = label;
            self.ai_pct = pct;
            self.ai_live = live;
            self.dirty = true;
        }
    }

    pub fn handle(&mut self, e: BarEv) -> Option<BarAct> {
        match e {
            BarEv::Move(x) => {
                if !self.tracking {
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut t);
                    }
                    self.tracking = true;
                }
                let h = self.slot_at(x);
                if h != self.hover {
                    self.hover = h;
                    self.dirty = true;
                }
            }
            BarEv::Leave => {
                self.tracking = false;
                if self.hover.is_some() {
                    self.hover = None;
                    self.dirty = true;
                }
            }
            BarEv::Up(x) => {
                let i = self.slot_at(x)?;
                return self.click(i);
            }
            BarEv::RightUp(x) => {
                let i = self.slot_at(x)?;
                match self.slots[i].item {
                    Item::Battery => shell::launch("ms-settings:batterysaver"),
                    Item::Net => shell::launch("ms-settings:network-wifi"),
                    _ => {}
                }
            }
            BarEv::Timer(TIMER_TICK) => self.tick(false),
            BarEv::Timer(TIMER_DISPLAY) => {
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_DISPLAY);
                }
                self.place();
            }
            BarEv::Timer(TIMER_WALLPAPER) => {
                unsafe { let _ = KillTimer(self.hwnd, TIMER_WALLPAPER); }
                crate::gfx::forget_desktop();
                crate::flyout::forget_backdrop();
                self.load_frost();
            }
            BarEv::Timer(_) => {}
            BarEv::Display => unsafe {
                // let Windows settle (several messages arrive per change), then refit once
                SetTimer(self.hwnd, TIMER_DISPLAY, 400, None);
            },
            BarEv::AppBar(1 /* ABN_POSCHANGED */, _) => {
                if !self.hidden {
                    self.reserve();
                }
            }
            BarEv::AppBar(2 /* ABN_FULLSCREENAPP */, _) => return Some(BarAct::Fullscreen),
            BarEv::AppBar(..) => {}
            BarEv::UpdateDone => return self.update_done(),
            BarEv::Wallpaper => {
                crate::gfx::forget_desktop();
                crate::flyout::forget_backdrop();
                self.load_frost();
                unsafe { SetTimer(self.hwnd, TIMER_WALLPAPER, 3500, None) };
                self.render();
            }
        }
        self.render();
        None
    }

    /// Draw again from scratch (the interface language changed).
    pub fn redraw(&mut self) {
        self.dirty = true;
        self.render();
    }

    /// A menu bar option changed in the Settings panel.
    pub fn set_option(&mut self, o: BarOpt, on: bool) {
        let f = match o {
            BarOpt::BatteryPercent => &mut self.battery_pct,
            BarOpt::AiUsage => &mut self.ai,
            BarOpt::TrayIcons => &mut self.tray,
        };
        if *f == on {
            return;
        }
        *f = on;
        if o == BarOpt::AiUsage && on {
            self.ai_gen = u64::MAX;
        }
        self.dirty = true;
        self.render();
    }

    /// The extra under this item has closed (or opened): keep its button lit while it is up.
    pub fn set_open_extra(&mut self, e: Option<Extra>) {
        let want = e.and_then(|e| {
            self.slots.iter().position(|s| {
                matches!((s.item, e), (Item::Chevron, Extra::Tray) | (Item::Ai, Extra::Ai) | (Item::Battery, Extra::Battery) | (Item::Net, Extra::Wifi) | (Item::Cc, Extra::ControlCenter) | (Item::Clock, Extra::Calendar) | (Item::Apps, Extra::Launchpad) | (Item::Search, Extra::Spotlight))
            })
        });
        if want != self.open {
            self.open = want;
            self.dirty = true;
            self.render();
        }
    }

    /// Screen px span of an extra's button, for placing its flyout.
    pub fn extra_span(&self, e: Extra) -> Option<(i32, i32)> {
        let item = match e {
            Extra::Tray => Item::Chevron,
            Extra::Ai => Item::Ai,
            Extra::Battery => Item::Battery,
            Extra::Wifi => Item::Net,
            Extra::ControlCenter => Item::Cc,
            Extra::Calendar => Item::Clock,
            Extra::Settings => Item::Logo,
            Extra::Launchpad => Item::Apps,
            Extra::Spotlight => Item::Search,
        };
        self.slots.iter().find(|s| s.item == item).map(|s| (self.px(s.x), self.px(s.x + s.w)))
    }

    pub fn dpi(&self) -> f64 {
        self.dpi
    }

    pub fn bottom_px(&self) -> i32 {
        self.px(H)
    }

    fn slot_at(&self, x_px: i32) -> Option<usize> {
        let x = x_px as f64 / self.dpi;
        self.slots.iter().position(|s| x >= s.x - GAP && x < s.x + s.w + GAP)
    }

    fn click(&mut self, i: usize) -> Option<BarAct> {
        let s = &self.slots[i];
        let (l, r) = (self.px(s.x), self.px(s.x + s.w));
        match s.item {
            Item::Logo | Item::App | Item::Menu(_) => return self.show_menu(i),
            Item::Chevron => return Some(BarAct::Flyout(Extra::Tray, l, r)),
            Item::Ai => return Some(BarAct::Flyout(Extra::Ai, l, r)),
            Item::Battery => return Some(BarAct::Flyout(Extra::Battery, l, r)),
            Item::Net => return Some(BarAct::Flyout(Extra::Wifi, l, r)),
            Item::Cc => return Some(BarAct::Flyout(Extra::ControlCenter, l, r)),
            Item::Search => return Some(BarAct::Flyout(Extra::Spotlight, l, r)),
            Item::Apps => return Some(BarAct::Flyout(Extra::Launchpad, l, r)),
            Item::Clock => return Some(BarAct::Flyout(Extra::Calendar, l, r)),
        }
    }

    // -------------------------------------------------------------- menus

    fn show_menu(&mut self, i: usize) -> Option<BarAct> {
        let item = self.slots[i].item;
        let app = self.app_name.clone();
        let (entries, acts): (Vec<Entry>, Vec<Do>) = match item {
            Item::Logo => self.system_menu(),
            Item::App => split(vec![
                (Entry::item(crate::i18n::fill("Hide {}", &app)), Do::HideApp),
                (Entry::item("Hide Others"), Do::HideOthers),
                (Entry::Sep, Do::None),
                (Entry::item(crate::i18n::fill("Quit {}", &app)), Do::QuitApp),
            ]),
            Item::Menu("File") => split(vec![
                (Entry::item("New Window").gesture("Ctrl+N"), Do::Keys(&[VK_CONTROL, b'N'])),
                (Entry::item("New Tab").gesture("Ctrl+T"), Do::Keys(&[VK_CONTROL, b'T'])),
                (Entry::item("Open…").gesture("Ctrl+O"), Do::Keys(&[VK_CONTROL, b'O'])),
                (Entry::Sep, Do::None),
                (Entry::item("Save").gesture("Ctrl+S"), Do::Keys(&[VK_CONTROL, b'S'])),
                (Entry::item("Save As…").gesture("Ctrl+Shift+S"), Do::Keys(&[VK_CONTROL, VK_SHIFT, b'S'])),
                (Entry::item("Print…").gesture("Ctrl+P"), Do::Keys(&[VK_CONTROL, b'P'])),
                (Entry::Sep, Do::None),
                (Entry::item("Close Window").gesture("Alt+F4"), Do::CloseWindow),
            ]),
            Item::Menu("Edit") => split(vec![
                (Entry::item("Undo").gesture("Ctrl+Z"), Do::Keys(&[VK_CONTROL, b'Z'])),
                (Entry::item("Redo").gesture("Ctrl+Y"), Do::Keys(&[VK_CONTROL, b'Y'])),
                (Entry::Sep, Do::None),
                (Entry::item("Cut").gesture("Ctrl+X"), Do::Keys(&[VK_CONTROL, b'X'])),
                (Entry::item("Copy").gesture("Ctrl+C"), Do::Keys(&[VK_CONTROL, b'C'])),
                (Entry::item("Paste").gesture("Ctrl+V"), Do::Keys(&[VK_CONTROL, b'V'])),
                (Entry::item("Select All").gesture("Ctrl+A"), Do::Keys(&[VK_CONTROL, b'A'])),
                (Entry::Sep, Do::None),
                (Entry::item("Find…").gesture("Ctrl+F"), Do::Keys(&[VK_CONTROL, b'F'])),
                (Entry::Sep, Do::None),
                (Entry::item("Emoji & Symbols").gesture("Win+."), Do::Keys(&[VK_LWIN, 0xBE])),
            ]),
            Item::Menu("View") => split(vec![
                (Entry::item("Enter Full Screen").gesture("F11"), Do::Keys(&[VK_F11])),
                (Entry::Sep, Do::None),
                (Entry::item("Zoom In").gesture("Ctrl+="), Do::Keys(&[VK_CONTROL, 0xBB])),
                (Entry::item("Zoom Out").gesture("Ctrl+-"), Do::Keys(&[VK_CONTROL, 0xBD])),
                (Entry::item("Actual Size").gesture("Ctrl+0"), Do::Keys(&[VK_CONTROL, b'0'])),
            ]),
            Item::Menu("Window") => split(vec![
                (Entry::item("Minimize").gesture("Win+↓"), Do::Minimize),
                (Entry::item("Zoom"), Do::Zoom),
                (Entry::Sep, Do::None),
                (Entry::item("Move to Left Half").gesture("Win+←"), Do::Keys(&[VK_LWIN, 0x25])),
                (Entry::item("Move to Right Half").gesture("Win+→"), Do::Keys(&[VK_LWIN, 0x27])),
                (Entry::Sep, Do::None),
                (Entry::item("Mission Control").gesture("Win+Tab"), Do::KeysNow(&[VK_LWIN, 0x09])),
            ]),
            Item::Menu(_) => split(vec![
                (Entry::item(crate::i18n::fill("{} Help", &app)).gesture("F1"), Do::Keys(&[0x70])),
                (Entry::item("Windows Tips"), Do::Launch("ms-get-started:")),
            ]),
            _ => return None,
        };
        self.open = Some(i);
        self.dirty = true;
        self.render();
        let s = &self.slots[i];
        let chosen = menu::popup(self.hwnd, &entries, Anchor::Below(self.px(s.x), self.px(H) + 1), self.dpi);
        crate::ensure_no_activate(self.hwnd);
        self.open = None;
        self.hover = None;
        self.dirty = true;
        self.render();
        self.run(acts.into_iter().nth(chosen?)?)
    }

    fn system_menu(&self) -> (Vec<Entry>, Vec<Do>) {
        split(vec![
            (Entry::item("About This PC"), Do::Launch("ms-settings:about")),
            match crate::update::available() {
                Some((v, _)) => (Entry::item(crate::i18n::fill("Update Available: MacDock {}…", &v.to_string())), Do::OpenUpdate),
                None => (Entry::item("Check for MacDock Updates…"), Do::CheckUpdate),
            },
            (Entry::Sep, Do::None),
            (Entry::item("System Settings…"), Do::Launch("ms-settings:")),
            (Entry::item("Microsoft Store…"), Do::Launch("ms-windows-store:")),
            (Entry::Sep, Do::None),
            (Entry::item("Force Quit…").gesture("Ctrl+Shift+Esc"), Do::Launch("taskmgr.exe")),
            (Entry::Sep, Do::None),
            (Entry::item("Sleep"), Do::Sleep),
            (Entry::item("Restart…"), Do::Restart),
            (Entry::item("Shut Down…"), Do::ShutDown),
            (Entry::Sep, Do::None),
            (Entry::item("Lock Screen").gesture("Win+L"), Do::Lock),
            (Entry::Sep, Do::None),
            (Entry::item("Show Battery Percentage").checked(self.battery_pct), Do::Opt(BarOpt::BatteryPercent)),
            (Entry::item("Show AI Usage").checked(self.ai), Do::Opt(BarOpt::AiUsage)),
            (Entry::item("Show Tray Icons").checked(self.tray), Do::Opt(BarOpt::TrayIcons)),
            (Entry::Sep, Do::None),
            (Entry::item("Automatically Hide Dock").checked(self.auto_hide_dock), Do::AutoHide),
            (Entry::item("Open MacDock at Login").checked(at_login()), Do::Login),
            (Entry::item("MacDock Settings…"), Do::Settings),
            (Entry::item("Quit MacDock"), Do::Quit),
        ])
    }

    fn target_ok(&self) -> bool {
        !self.target.0.is_null() && unsafe { IsWindow(self.target) }.as_bool()
    }

    fn run(&mut self, d: Do) -> Option<BarAct> {
        match d {
            Do::None => {}
            Do::Keys(k) => {
                // hand focus back to the app the user was in, then send it the shortcut
                if self.target_ok() {
                    shell::activate(self.target);
                    std::thread::sleep(Duration::from_millis(90));
                    key(k);
                }
            }
            Do::KeysNow(k) => key(k),
            Do::Launch(t) => shell::launch(t),
            Do::HideApp | Do::QuitApp | Do::HideOthers => {
                if self.target_ok() {
                    let pid = shell::window_pid(self.target);
                    for w in self.shell.app_windows() {
                        match d {
                            Do::HideApp if w.pid == pid => shell::minimize(w.hwnd),
                            Do::HideOthers if w.pid != pid => shell::minimize(w.hwnd),
                            Do::QuitApp if w.pid == pid => shell::close(w.hwnd),
                            _ => {}
                        }
                    }
                }
            }
            Do::CloseWindow => {
                if self.target_ok() {
                    shell::close(self.target);
                }
            }
            Do::Minimize => {
                if self.target_ok() {
                    shell::minimize(self.target);
                }
            }
            Do::Zoom => {
                if self.target_ok() {
                    let cmd = if unsafe { IsZoomed(self.target) }.as_bool() { SC_RESTORE } else { SC_MAXIMIZE };
                    unsafe {
                        let _ = PostMessageW(self.target, WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
                    }
                }
            }
            Do::Sleep => unsafe {
                let _ = SetSuspendState(false, false, false);
            },
            Do::Restart => {
                if confirm(self.hwnd, "Restart now?") {
                    shell::run_hidden("shutdown.exe", "/r /t 0");
                }
            }
            Do::ShutDown => {
                if confirm(self.hwnd, "Shut down now?") {
                    shell::run_hidden("shutdown.exe", "/s /t 0");
                }
            }
            Do::Lock => unsafe {
                let _ = LockWorkStation();
            },
            Do::AutoHide => return Some(BarAct::ToggleAutoHide),
            Do::Opt(o) => {
                let f = match o {
                    BarOpt::BatteryPercent => &mut self.battery_pct,
                    BarOpt::AiUsage => &mut self.ai,
                    BarOpt::TrayIcons => &mut self.tray,
                };
                *f = !*f;
                let on = *f;
                if o == BarOpt::AiUsage && on {
                    self.ai_gen = u64::MAX; // read the summary again on the next tick
                }
                self.dirty = true;
                self.render();
                return Some(BarAct::Option(o, on));
            }
            Do::Login => toggle_login(),
            Do::Settings => return Some(BarAct::OpenSettings),
            Do::OpenUpdate => {
                if let Some((v, url)) = crate::update::available() {
                    return self.offer_update(&v, &url);
                }
            }
            Do::CheckUpdate => {
                let cur = crate::update::CURRENT;
                match crate::update::check() {
                    Ok(Some((v, url))) => {
                        self.update = true;
                        return self.offer_update(&v, &url);
                    }
                    Ok(None) => inform(self.hwnd, &format!("MacDock {cur} is the latest version.")),
                    Err(e) => inform(self.hwnd, &format!("Could not check for updates: {e}.")),
                }
                self.update = crate::update::available().is_some();
                self.dirty = true;
                self.render();
            }
            Do::Quit => return Some(BarAct::Quit),
        }
        None
    }

    /// "Install and restart" (the exe is swapped in place, see update::install), or the release page.
    fn offer_update(&mut self, v: &str, url: &str) -> Option<BarAct> {
        let cur = crate::update::CURRENT;
        let q = format!("MacDock {v} is available (you have {cur}).

Install it now? MacDock downloads it, checks it and restarts.

Yes: install and restart
No: open the release page");
        let t = shell::wide(&q);
        let answer = unsafe { MessageBoxW(self.hwnd, windows::core::PCWSTR(t.as_ptr()), w!("MacDock"), MB_YESNOCANCEL | MB_ICONQUESTION | MB_TOPMOST) };
        if answer == IDNO {
            shell::launch(url);
        }
        if answer != IDYES {
            return None;
        }
        // the download takes seconds: a worker does it and posts WM_UPDATE_DONE back, so the bar and dock keep going
        let (h, v, url) = (self.hwnd.0 as isize, v.to_string(), url.to_string());
        if !crate::update::begin_install() {
            return None; // one is already on its way
        }
        std::thread::spawn(move || {
            let r = crate::update::install(&v);
            crate::update::finish_install(r.map(|()| v.clone()).map_err(|e| (v, url, e)));
            unsafe {
                let _ = PostMessageW(HWND(h as *mut _), WM_UPDATE_DONE, WPARAM(0), LPARAM(0));
            }
        });
        None
    }

    fn update_done(&mut self) -> Option<BarAct> {
        match crate::update::take_install()? {
            Ok(_) => Some(BarAct::Restart),
            Err((v, url, e)) => {
                if confirm(self.hwnd, &format!("Could not install MacDock {v}: {e}.

Open the release page to update by hand?")) {
                    shell::launch(&url);
                }
                None
            }
        }
    }

    // -------------------------------------------------------------- layout and drawing

    fn font(&self, weight: i32) -> Font {
        Font { face: gfx::UI, px: (FONT_PX * self.dpi).round() as i32, weight }
    }

    fn glyph_font(&self, px: f64) -> Font {
        Font { face: gfx::ICONS, px: (px * self.dpi).round() as i32, weight: 400 }
    }

    fn text_w(&self, s: &str, f: Font) -> f64 {
        gfx::measure(s, f).0 as f64 / self.dpi
    }

    fn battery_text(&self) -> String {
        match self.battery {
            // macOS hides the number by default; we still show it when the battery runs low
            Some((pct, ac, _)) if self.battery_pct || (pct <= 20 && !ac) => format!("{pct}%"),
            _ => String::new(),
        }
    }

    /// Content width (DIPs) of each item.
    fn content_w(&self, it: Item) -> f64 {
        match it {
            Item::Logo => 13.0,
            Item::App => self.text_w(&self.app_name, self.font(700)),
            Item::Menu(m) => self.text_w(crate::i18n::t(m), self.font(500)),
            Item::Chevron => self.text_w("\u{E70E}", self.glyph_font(11.0)),
            Item::Ai => 14.0 + if self.ai_label.is_empty() { 0.0 } else { 5.0 + self.text_w(&self.ai_label, self.font(400)) },
            Item::Battery => {
                let t = self.battery_text();
                if t.is_empty() { 26.0 } else { self.text_w(&t, self.font(400)) + 6.0 + 26.0 }
            }
            Item::Net => 17.0,
            Item::Search => self.text_w("\u{E721}", self.glyph_font(GLYPH_PX)),
            Item::Apps => self.text_w("\u{F0E2}", self.glyph_font(GLYPH_PX)),
            Item::Cc => 17.0,
            Item::Clock => self.text_w(&self.clock, self.font(400)),
        }
    }

    fn layout(&mut self) {
        let mut slots = Vec::new();
        let mut x = SIDE;
        for it in [Item::Logo, Item::App, Item::Menu("File"), Item::Menu("Edit"), Item::Menu("View"), Item::Menu("Window"), Item::Menu("Help")] {
            let w = self.content_w(it) + PAD_X * 2.0;
            slots.push(Slot { item: it, x: x + GAP, w });
            x += w + GAP * 2.0;
        }
        let mut right = vec![];
        if self.tray {
            right.push(Item::Chevron);
        }
        if self.ai && self.ai_live {
            right.push(Item::Ai);
        }
        if self.battery.is_some() {
            right.push(Item::Battery);
        }
        right.extend([Item::Net, Item::Apps, Item::Search, Item::Cc, Item::Clock]);
        let mut x = self.screen_w - SIDE;
        for it in right.into_iter().rev() {
            let w = self.content_w(it) + PAD_X * 2.0;
            x -= w + GAP;
            slots.push(Slot { item: it, x, w });
            x -= GAP;
        }
        self.slots = slots;
    }

    pub fn render(&mut self) {
        if !self.dirty || self.hidden {
            return;
        }
        self.dirty = false;
        self.layout();
        let (w, h) = (self.px(self.screen_w).max(1) as u32, self.px(H).max(1) as u32);
        if self.surface.as_ref().is_none_or(|s| s.w != w || s.h != h) {
            self.surface = Surface::new(w, h);
        }
        let dpi = self.dpi as f32;
        let mut pm = Pixmap::new(w, h).unwrap();
        // macOS 26: no bar at all, the content sits on the wallpaper. The bar's own pixels stay at alpha 1 so
        // the whole strip still takes clicks; hover / open highlights go on that layer, under the content.
        pm.fill(Color::from_rgba8(0, 0, 0, 1));
        let mut content = Pixmap::new(w, h).unwrap();
        for (i, s) in self.slots.iter().enumerate() {
            let lit = if self.open == Some(i) { 0x40 } else if self.hover == Some(i) { 0x30 } else { 0 };
            if lit > 0 {
                if let Some(p) = gfx::rrect(s.x as f32 * dpi, 3.0 * dpi, s.w as f32 * dpi, (H as f32 - 6.0) * dpi, 5.0 * dpi) {
                    let mut paint = Paint::default();
                    if self.dark_text {
                        paint.set_color_rgba8(0, 0, 0, lit / 2);
                    } else {
                        paint.set_color_rgba8(255, 255, 255, lit);
                    }
                    paint.anti_alias = true;
                    pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                }
            }
            let cx = (s.x + PAD_X) as f32 * dpi; // content left
            let mid = H as f32 / 2.0 * dpi;
            match s.item {
                Item::Logo => {
                    draw_logo(&mut content, cx, mid, dpi);
                    if self.update {
                        // update available: a small blue dot on the logo's top-right corner, like a badge
                        if let Some(p) = tiny_skia::PathBuilder::from_circle(cx + 13.0 * dpi, mid - 6.5 * dpi, 3.2 * dpi) {
                            let mut paint = Paint::default();
                            paint.set_color_rgba8(0xFF, 0x84, 0x0A, 0xFF); // BGRA: #0A84FF
                            paint.anti_alias = true;
                            content.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                        }
                    }
                }
                Item::App => self.draw_text(&mut content, &self.app_name, self.font(700), cx, mid),
                Item::Menu(m) => self.draw_text(&mut content, crate::i18n::t(m), self.font(500), cx, mid),
                Item::Chevron => self.draw_text(&mut content, "\u{E70E}", self.glyph_font(11.0), cx, mid),
                Item::Ai => {
                    draw_ai_ring(&mut content, cx + 7.0 * dpi, mid, 6.0 * dpi, dpi, self.ai_pct, self.ai_label.ends_with("/s"));
                    if !self.ai_label.is_empty() {
                        self.draw_text(&mut content, &self.ai_label, self.font(400), cx + 19.0 * dpi, mid);
                    }
                }
                Item::Battery => {
                    let t = self.battery_text();
                    self.draw_text(&mut content, &t, self.font(400), cx, mid);
                    let gx = if t.is_empty() { cx } else { cx + (self.text_w(&t, self.font(400)) as f32 + 6.0) * dpi };
                    if let Some((pct, ac, charging)) = self.battery {
                        draw_battery(&mut content, gx, mid - 6.25 * dpi + 1.0 * dpi, 26.0 * dpi, 12.5 * dpi, pct, ac, charging);
                    }
                }
                Item::Net => match self.net {
                    Net::Wired => self.draw_text(&mut content, "\u{E839}", self.glyph_font(GLYPH_PX), cx, mid),
                    Net::Wifi(b) => draw_wifi(&mut content, cx, mid - 6.25 * dpi, 17.0 * dpi, 12.5 * dpi, b, false),
                    Net::Off => draw_wifi(&mut content, cx, mid - 6.25 * dpi, 17.0 * dpi, 12.5 * dpi, 0, true),
                },
                Item::Search => self.draw_text(&mut content, "\u{E721}", self.glyph_font(GLYPH_PX), cx, mid),
                Item::Apps => self.draw_text(&mut content, "\u{F0E2}", self.glyph_font(GLYPH_PX), cx, mid),
                Item::Cc => draw_cc(&mut content, cx, mid, dpi),
                Item::Clock => self.draw_text(&mut content, &self.clock, self.font(400), cx, mid),
            }
        }
        if self.dark_text {
            // bright wallpaper: the white glyphs turn black (coloured ones, like a charging battery, stay)
            for px in content.pixels_mut() {
                let (r, g, b) = (px.red(), px.green(), px.blue());
                if r.max(g).max(b) - r.min(g).min(b) < 24 {
                    *px = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, px.alpha()).unwrap();
                }
            }
        } else {
            // dark wallpaper: white content with a soft shadow, like macOS
            let mut shadow = content.clone();
            for px in shadow.pixels_mut() {
                *px = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, (px.alpha() as u16 * 110 / 255) as u8).unwrap();
            }
            gfx::blur(&mut shadow, 2.0 * dpi);
            pm.draw_pixmap(0, (0.5 * dpi).round() as i32, shadow.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
        pm.draw_pixmap(0, 0, content.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        if let Some(sf) = self.surface.as_mut() {
            // replace, not blend: the surface still holds the last frame
            let copy = PixmapPaint { blend_mode: tiny_skia::BlendMode::Source, ..Default::default() };
            sf.pixmap().draw_pixmap(0, 0, pm.as_ref(), &copy, Transform::identity(), None);
            sf.present(self.hwnd, 0, 0);
        }
    }

    /// Look at the wallpaper's top strip: bright means black content, as macOS does.
    fn load_frost(&mut self) {
        let fw = 128u32;
        let fh = (fw as f64 * self.screen_h / self.screen_w).round().max(1.0) as u32;
        if let Some(fr) = gfx::wallpaper(fw, fh) {
            let rows = ((fh as f64 * H / self.screen_h).ceil() as u32 + 1).min(fh);
            let d = fr.data();
            let n = (rows * fw) as usize;
            let sum: f64 = (0..n).map(|p| p * 4).map(|i| 0.0722 * d[i] as f64 + 0.7152 * d[i + 1] as f64 + 0.2126 * d[i + 2] as f64).sum();
            self.dark_text = n > 0 && sum / n as f64 / 255.0 > 0.62;
        }
        self.dirty = true;
    }

    fn draw_text(&self, pm: &mut Pixmap, s: &str, f: Font, x: f32, mid: f32) {
        if let Some(t) = gfx::text_ex(s, f, (255, 255, 255)) {
            let y = (mid - t.height() as f32 / 2.0).round() as i32;
            pm.draw_pixmap(x.round() as i32, y, t.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
    }
}

enum Do {
    None,
    Keys(&'static [u8]),
    KeysNow(&'static [u8]),
    Launch(&'static str),
    HideApp,
    HideOthers,
    QuitApp,
    CloseWindow,
    Minimize,
    Zoom,
    Sleep,
    Restart,
    ShutDown,
    Lock,
    AutoHide,
    Opt(BarOpt),
    Login,
    Settings,
    OpenUpdate,
    CheckUpdate,
    Quit,
}

fn split(v: Vec<(Entry, Do)>) -> (Vec<Entry>, Vec<Do>) {
    v.into_iter().unzip()
}

// ------------------------------------------------------------------ glyphs (pixels)

fn white(a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(255, 255, 255, a);
    p.anti_alias = true;
    p
}

fn draw_logo(pm: &mut Pixmap, x: f32, mid: f32, dpi: f32) {
    let s = 13.0 * dpi;
    let cell = s / 2.0;
    let m = 0.6 * dpi;
    for (i, j) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        if let Some(r) = Rect::from_xywh(x + i * cell + m, mid - s / 2.0 + j * cell + m, cell - 2.0 * m, cell - 2.0 * m) {
            pm.fill_rect(r, &white(255), Transform::identity(), None);
        }
    }
}

/// Two switch pills, knob left in the top one and right in the bottom one.
fn draw_cc(pm: &mut Pixmap, x: f32, mid: f32, dpi: f32) {
    let (w, h, gap) = (17.0 * dpi, 7.0 * dpi, 2.0 * dpi);
    let top = mid - (h * 2.0 + gap) / 2.0;
    let stroke = Stroke { width: 1.3 * dpi, ..Default::default() };
    for i in 0..2 {
        let y = top + i as f32 * (h + gap);
        let sw = stroke.width / 2.0;
        if let Some(p) = gfx::rrect(x + sw, y + sw, w - stroke.width, h - stroke.width, h / 2.0 - sw) {
            pm.stroke_path(&p, &white(255), &stroke, Transform::identity(), None);
        }
        let r = 1.8 * dpi;
        let cx = if i == 0 { x + 1.2 * dpi + stroke.width + r } else { x + w - 1.2 * dpi - stroke.width - r };
        if let Some(c) = PathBuilder::from_circle(cx, y + h / 2.0, r) {
            pm.fill_path(&c, &white(255), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// Wi-Fi as a dot and two arcs (macOS style); missing bars dimmed.
pub fn draw_wifi(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, bars: u8, off: bool) {
    let (cx, cy) = (x + w / 2.0, y + h * 0.95);
    let width = (h * 0.14).max(1.2);
    let stroke = Stroke { width, line_cap: LineCap::Round, ..Default::default() };
    let s45 = std::f32::consts::FRAC_1_SQRT_2;
    for i in 1..=2u8 {
        let r = h * (0.08 + 0.40 * i as f32);
        // a quarter circle from -45° to +45° around straight up, as one cubic
        let (x0, y0, x1) = (cx - r * s45, cy - r * s45, cx + r * s45);
        let k = 0.5523 * r;
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.cubic_to(x0 + k * s45, y0 - k * s45, x1 - k * s45, y0 - k * s45, x1, y0);
        let lit = !off && bars > i;
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &white(if lit { 255 } else { 0x4D }), &stroke, Transform::identity(), None);
        }
    }
    let dot = (h * 0.12).max(1.2);
    if let Some(c) = PathBuilder::from_circle(cx, cy - dot, dot) {
        pm.fill_path(&c, &white(if !off && bars >= 1 { 255 } else { 0x4D }), FillRule::Winding, Transform::identity(), None);
    }
}

/// AI usage: a ring filled to the worst limit's percentage (colour by level), a dot in the middle while tokens burn.
fn draw_ai_ring(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, dpi: f32, pct: Option<f64>, live: bool) {
    let sw = 1.9 * dpi;
    let ring = |pm: &mut Pixmap, from: f32, to: f32, paint: &Paint| {
        let mut pb = PathBuilder::new();
        let n = ((to - from) / 0.12).ceil().max(2.0) as usize;
        for i in 0..=n {
            let t = from + (to - from) * i as f32 / n as f32;
            let (x, y) = (cx + r * t.sin(), cy - r * t.cos());
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        if let Some(p) = pb.finish() {
            let st = Stroke { width: sw, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
            pm.stroke_path(&p, paint, &st, Transform::identity(), None);
        }
    };
    let tau = std::f32::consts::TAU;
    ring(pm, 0.0, tau, &white(0x55));
    if let Some(p) = pct.filter(|p| *p > 0.0) {
        let (r_, g, b) = crate::fly_ai::level(p);
        let mut paint = Paint::default();
        paint.set_color_rgba8(b, g, r_, 255); // surfaces are BGRA
        paint.anti_alias = true;
        ring(pm, 0.0, tau * (p.min(100.0) / 100.0) as f32, &paint);
    }
    if live {
        if let Some(c) = PathBuilder::from_circle(cx, cy, 2.2 * dpi) {
            pm.fill_path(&c, &white(0xF0), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// macOS battery: outline, fill by charge, terminal nub, bolt while plugged in.
pub fn draw_battery(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, pct: u8, plugged: bool, charging: bool) {
    let nub_w = (w * 0.07).max(1.5);
    let body_w = w - nub_w - w * 0.03;
    let stroke = (h * 0.085).max(1.0);
    let r = h * 0.3;
    if let Some(p) = gfx::rrect(x + stroke / 2.0, y + stroke / 2.0, body_w - stroke, h - stroke, r) {
        pm.stroke_path(&p, &white(0x8C), &Stroke { width: stroke, ..Default::default() }, Transform::identity(), None);
    }
    let nub_h = h * 0.36;
    if let Some(p) = gfx::rrect(x + w - nub_w, y + (h - nub_h) / 2.0, nub_w, nub_h, nub_w / 2.0) {
        pm.fill_path(&p, &white(0x8C), FillRule::Winding, Transform::identity(), None);
    }
    let inset = stroke + (h * 0.09).max(1.0);
    let (iw, ih) = (body_w - 2.0 * inset, h - 2.0 * inset);
    let fill_w = (iw * pct as f32 / 100.0).max(if pct > 0 { ih * 0.5 } else { 0.0 });
    let (cr, cg, cb) = if charging {
        (0x34, 0xC7, 0x59)
    } else if pct <= 10 && !plugged {
        (0xFF, 0x45, 0x3A)
    } else if pct <= 20 && !plugged {
        (0xFF, 0x9F, 0x0A)
    } else {
        (255, 255, 255)
    };
    let ir = (r - inset + stroke / 2.0).max(0.5);
    if fill_w > 0.0 {
        if let Some(p) = gfx::rrect(x + inset, y + inset, fill_w, ih, ir) {
            let mut paint = Paint::default();
            paint.set_color_rgba8(cb, cg, cr, 255);
            paint.anti_alias = true;
            pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }
    if plugged {
        let (bh, bw) = (h * 0.86, h * 0.86 * 0.56);
        let (cx, cy) = (x + stroke / 2.0 + (body_w - stroke) / 2.0, y + h / 2.0);
        let mut pb = PathBuilder::new();
        pb.move_to(cx + bw * 0.12, cy - bh / 2.0);
        pb.line_to(cx - bw * 0.50, cy + bh * 0.08);
        pb.line_to(cx - bw * 0.02, cy + bh * 0.08);
        pb.line_to(cx - bw * 0.12, cy + bh / 2.0);
        pb.line_to(cx + bw * 0.50, cy - bh * 0.08);
        pb.line_to(cx + bw * 0.02, cy - bh * 0.08);
        pb.close();
        if let Some(p) = pb.finish() {
            let mut rim = Paint::default();
            rim.set_color_rgba8(0, 0, 0, 0xA0);
            rim.anti_alias = true;
            let s = Stroke { width: (h * 0.14).max(1.2), line_join: tiny_skia::LineJoin::Round, ..Default::default() };
            pm.stroke_path(&p, &rim, &s, Transform::identity(), None);
            pm.fill_path(&p, &white(255), FillRule::Winding, Transform::identity(), None);
        }
    }
}
