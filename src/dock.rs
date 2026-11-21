//! The dock (port of DockWindow.cs): frosted plate, cosine magnification, launch and attention bounces,
//! running dots, unread badges, hover label, Trash, drag to reorder or remove, context menus, auto-hide and
//! an AppBar so maximized windows stop above it. One layered window that tiny-skia paints straight into.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, Pattern, PathBuilder, Pixmap, PixmapPaint,
    Point, Rect, SpreadMode, Stroke, Transform,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{SHAppBarMessage, SHEmptyRecycleBinW, ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::badges;
use crate::config::Config;
use crate::gfx::{self, Font, Surface};
use crate::menu::{self, Anchor, Entry};
use crate::shell::{self, eq_ic, AppWindow, Shell};
use crate::{push, taskbar, tile, Ev};

// macOS: the glass above and below a tile is ~35% of the tile (tiles fill ~88% of their box)
const PAD: f64 = 13.0;
const BOTTOM_GAP: f64 = 6.0;
const RADIUS: f64 = 24.0;
const PAD_X: f64 = 8.0;
const CB_APPBAR: u32 = WM_APP + 1;
const TIMER_REFRESH: usize = 1;
const TIMER_DISPLAY: usize = 2;
const TIMER_EDGE: usize = 3;
const TIMER_TOASTS: usize = 4;
const TIMER_BACKDROP: usize = 5;
const TIMER_BACKDROP_RECHECK: usize = 6;
/// changes behind the plate (video, a window dragged) repaint at most this often; hover and magnify stay at full rate
const BACKDROP_FPS: f64 = 30.0;
const ABN_POSCHANGED: usize = 1;
const ABN_FULLSCREENAPP: usize = 2;
const HSHELL_FLASH: usize = 0x8006;
const SPI_SETDESKWALLPAPER: usize = 0x14;
const TRASH: &str = "shell:RecycleBinFolder";
const WM_MOUSELEAVE: u32 = 0x02A3;
const ATTENTION_HOP: f64 = 0.5;
const ATTENTION_CYCLE: f64 = 1.1;
const LAUNCH_PERIOD: f64 = 0.62;

// ------------------------------------------------------------------ window messages → events
// The window procedure only queues events; the main loop handles them. That keeps re-entrant calls
// (UpdateLayeredWindow, ShowWindow… send messages synchronously) away from the dock's state.

pub enum DockEv {
    Wake,
    Down,
    Up,
    RightUp,
    Refresh,
    DisplayChanged,
    DisplaySettled,
    Edge,
    Toasts,
    Wallpaper,
    Flash(isize),
    AppBar(usize, isize),
    /// something behind the plate changed (true: windows moved; false: a window drew)
    Backdrop(bool),
    BackdropTick,
    /// look again shortly after windows changed: a window opening is still cloaked during its animation
    BackdropRecheck,
}

/// What the dock wants done by the app around it.
pub enum Act {
    Quit,
    Fullscreen(bool),
    /// minimize these windows (the genie animates `fg` into its icon first)
    Minimize { fg: HWND, wins: Vec<HWND> },
    /// restore these minimized windows (back to front) and bring `front` forward
    Restore { front: HWND, minimized: Vec<HWND> },
}

thread_local! {
    static TRACKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SHELLHOOK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn fresh_used(layer: &Option<([u32; 6], Pixmap, i32, i32)>, key: &[u32; 6]) -> bool {
    layer.as_ref().is_some_and(|l| &l.0 == key)
}


fn ev(e: DockEv) {
    push(Ev::Dock(e));
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            if !TRACKING.get() {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut t);
                TRACKING.set(true);
            }
            ev(DockEv::Wake);
        }
        WM_MOUSELEAVE => {
            TRACKING.set(false);
            ev(DockEv::Wake);
        }
        WM_LBUTTONDOWN => ev(DockEv::Down),
        WM_LBUTTONUP => ev(DockEv::Up),
        WM_RBUTTONUP => ev(DockEv::RightUp),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER => match w.0 {
            TIMER_REFRESH => ev(DockEv::Refresh),
            TIMER_DISPLAY => ev(DockEv::DisplaySettled),
            TIMER_EDGE => ev(DockEv::Edge),
            TIMER_TOASTS => ev(DockEv::Toasts),
            TIMER_BACKDROP_RECHECK => {
                let _ = KillTimer(h, TIMER_BACKDROP_RECHECK);
                ev(DockEv::BackdropRecheck);
            }
            TIMER_BACKDROP => {
                let _ = KillTimer(h, TIMER_BACKDROP);
                ev(DockEv::BackdropTick);
            }
            _ => {}
        },
        WM_DISPLAYCHANGE => ev(DockEv::DisplayChanged),
        // we size the window ourselves on every frame; don't let Windows apply its suggested rect
        WM_DPICHANGED => {
            ev(DockEv::DisplayChanged);
            return LRESULT(0);
        }
        WM_SETTINGCHANGE if w.0 == SPI_SETDESKWALLPAPER => ev(DockEv::Wallpaper),
        CB_APPBAR => ev(DockEv::AppBar(w.0, l.0)),
        crate::backdrop::WM_BACKDROP => ev(DockEv::Backdrop(w.0 == 1)),
        // the session is ending and we may not get another chance: put the taskbar back now
        WM_ENDSESSION if w.0 != 0 => {
            taskbar::restore(false);
            crate::genie::restore_system_animation();
        }
        WM_DESTROY => PostQuitMessage(0),
        // HSHELL_FLASH: an app flashes its taskbar button (FlashWindowEx) asking for attention
        m if m != 0 && m == SHELLHOOK.get() => {
            if w.0 & 0x7FFF == HSHELL_FLASH & 0x7FFF {
                ev(DockEv::Flash(l.0));
            }
        }
        _ => {}
    }
    DefWindowProcW(h, msg, w, l)
}

pub fn create_window() -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("MacDockRs");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class, w!("MacDock"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        )
        .unwrap()
    }
}

// ------------------------------------------------------------------ dock items

struct Item {
    key: String,
    name: String,
    launch: Option<String>,
    aumid: Option<String>,
    exe: Option<String>,
    trash: bool,
    stack: bool,       // the Downloads stack
    sep_before: bool,  // a divider goes before this item
    icon: Option<Rc<Pixmap>>,
    wins: Vec<AppWindow>,
    // animation state, DIPs
    scale: f64,
    presence: f64,
    leaving: bool,
    x: f64,
    size: f64,
    bounce_t: f64,
    launching: bool,
    launch_started: Instant,
    wins_at_launch: usize,
    attention: bool,
    attention_bounce: bool,
    attention_started: Instant,
    dot: f64,
    badge: u32,
    badge_text: Option<(u32, Pixmap)>,
}

impl Item {
    fn new(key: String, name: String, icon: Option<Rc<Pixmap>>, presence: f64) -> Item {
        Item {
            key, name, launch: None, aumid: None, exe: None, trash: false, stack: false, sep_before: false, icon, wins: Vec::new(),
            scale: 1.0, presence, leaving: false, x: 0.0, size: 0.0, bounce_t: 0.0,
            launching: false, launch_started: Instant::now(), wins_at_launch: 0,
            attention: false, attention_bounce: false, attention_started: Instant::now(),
            dot: 0.0, badge: 0, badge_text: None,
        }
    }

    fn running(&self) -> bool {
        !self.wins.is_empty()
    }

    fn pinned(&self) -> bool {
        self.key.starts_with("pin:")
    }

    fn owns(&self, w: &AppWindow) -> bool {
        if let (Some(a), Some(b)) = (&self.aumid, &w.aumid) {
            if eq_ic(a, b) {
                return true;
            }
        }
        if let (Some(a), Some(b)) = (&self.exe, &w.exe) {
            return eq_ic(a, b) || launcher_of(a, b);
        }
        false
    }
}

/// Squirrel apps are pinned through …\App\Update.exe while their windows belong to …\App\app-1.2.3\App.exe:
/// the launcher's folder owns everything below it (same rule as DockItem.LauncherOf).
fn launcher_of(pinned: &str, running: &str) -> bool {
    let p = std::path::Path::new(pinned);
    let (Some(dir), Some(file)) = (p.parent(), p.file_name()) else { return false };
    let prefix = format!("{}\\", dir.to_string_lossy());
    if running.len() <= prefix.len() || !running.is_char_boundary(prefix.len()) || !eq_ic(&running[..prefix.len()], &prefix) {
        return false;
    }
    let file = file.to_string_lossy();
    let run_file = std::path::Path::new(running).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    eq_ic(&file, "Update.exe") || eq_ic(&file, &run_file)
}

pub fn approach(v: f64, target: f64, k: f64) -> f64 {
    let v = v + (target - v) * k;
    if (target - v).abs() < 0.002 { target } else { v }
}

/// Primary screen size in DIPs and its scale. Read fresh each time: monitors come and go.
pub fn primary_screen() -> (f64, f64, f64) {
    unsafe {
        let (pw, ph) = (GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64);
        let mon = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let dpi = if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 { dx as f64 / 96.0 } else { 1.0 };
        (pw / dpi, ph / dpi, dpi)
    }
}

fn now_secs(t: Instant, base: Instant) -> f64 {
    t.saturating_duration_since(base).as_secs_f64()
}

// ------------------------------------------------------------------ the dock

pub struct Dock {
    pub hwnd: HWND,
    pub cfg: Config,
    shell: Shell,
    pub dpi: f64,
    screen_w: f64,
    screen_h: f64,
    fit_b: f64,
    win_w: f64,
    win_h: f64,
    items: Vec<Item>,
    icons: HashMap<String, (String, Option<Rc<Pixmap>>)>, // name + icon per item key, kept across app restarts
    surface: Option<Surface>,
    drawn: Vec<u64>, // everything the last rendered frame depended on; same again = skip the frame
    frost: Option<Pixmap>,
    glass: Option<Pixmap>, // the wallpaper at half the screen's pixels, lightly blurred: what Liquid Glass shows
    light_glass: bool,
    backdrop: Option<crate::backdrop::Backdrop>, // live captures of the windows behind the plate
    backdrop_gen: u64,
    bd_due: (bool, bool), // backdrop events waiting for the next allowed repaint: (frames, layout)
    bd_last: f64,
    bd_timer: bool,
    wall_strip: Option<(windows::Win32::Foundation::RECT, usize, Pixmap)>, // the wallpaper under the plate, scaled once
    over: Option<Pixmap>, // icons, rim, labels: everything drawn over the glass
    glass_layer: Option<([u32; 6], Pixmap, i32, i32)>, // the finished glass, kept while the plate and what is behind stay put
    behind: Option<(windows::Win32::Foundation::RECT, u64, usize, Pixmap)>, // the composed backdrop, kept while nothing behind changes // the wallpaper behind the dock is bright: dark running dots, else light ones
    label: Option<(String, Pixmap)>,
    label_alpha: f64,
    menu_up: bool, // a dock menu is open: no hover label over it
    label_at: (f64, f64, f64, f64),
    hovered: Option<usize>,
    plate_left: f64,
    plate_w: f64,
    seps: Vec<f64>,
    running_now: HashMap<String, serde_json::Value>, // unpinned apps running at the last refresh
    clock: Instant,
    last_t: f64,
    pub awake: bool,
    hidden: bool, // game mode: a fullscreen app is in front
    sig: String,
    refreshes: u32,
    appbar: bool,
    // auto-hide
    hide: f64,
    peek_until: Instant,
    last_near: Instant,
    // press / drag
    pressed: Option<String>,
    press_pt: (f64, f64),
    dragging: bool,
    drag_out: bool,
    drag_x: f64,
    // the last foreground window that isn't ours
    pub last_app: HWND,
    // badges
    seen: Option<badges::Seen>,
    toasts: Vec<(String, i64)>,
    toast_job: Option<Arc<Mutex<Option<Vec<(String, i64)>>>>>,
}

impl Dock {
    fn b(&self) -> f64 {
        self.cfg.icon_size.min(self.fit_b)
    }
    fn gap(&self) -> f64 {
        (self.b() * 0.14).round()
    }
    fn plate_h(&self) -> f64 {
        self.b() + PAD * 2.0
    }
    fn win_x(&self) -> f64 {
        (self.screen_w - self.win_w) / 2.0
    }
    fn win_y(&self) -> f64 {
        self.screen_h - self.win_h
    }
    fn icon_px(&self) -> u32 {
        (self.cfg.icon_size * self.cfg.magnify * self.dpi).ceil() as u32
    }

    pub fn new(hwnd: HWND, cfg: Config) -> Dock {
        let (screen_w, screen_h, dpi) = primary_screen();
        let now = Instant::now();
        Dock {
            hwnd, cfg, shell: Shell::default(), dpi, screen_w, screen_h, fit_b: f64::MAX,
            win_w: 1.0, win_h: 1.0, items: Vec::new(), icons: HashMap::new(), surface: None, drawn: Vec::new(), frost: None, glass: None, light_glass: false, backdrop: crate::backdrop::Backdrop::new(hwnd), backdrop_gen: 0, bd_due: (false, false), bd_last: 0.0, bd_timer: false, behind: None, glass_layer: None, over: None, wall_strip: None,
            label: None, label_alpha: 0.0, menu_up: false, label_at: (0.0, 0.0, 0.0, 0.0), hovered: None, plate_left: 0.0,
            plate_w: 0.0, seps: Vec::new(), running_now: HashMap::new(), clock: now, last_t: 0.0, awake: false, hidden: false,
            sig: String::new(), refreshes: 0, appbar: false,
            hide: 0.0, peek_until: now, last_near: now,
            pressed: None, press_pt: (0.0, 0.0), dragging: false, drag_out: false, drag_x: 0.0,
            last_app: unsafe { GetForegroundWindow() },
            seen: None, toasts: Vec::new(), toast_job: None,
        }
    }

    pub fn start(&mut self) {
        self.load_frost();
        if self.cfg.badges {
            self.seen = Some(badges::Seen::load());
        }
        self.refresh(true);
        unsafe {
            SetTimer(self.hwnd, TIMER_REFRESH, 700, None);
            if self.cfg.auto_hide {
                SetTimer(self.hwnd, TIMER_EDGE, 60, None);
            }
            if self.cfg.badges {
                SetTimer(self.hwnd, TIMER_TOASTS, 3000, None);
            }
            let m = RegisterWindowMessageW(w!("SHELLHOOK"));
            SHELLHOOK.set(m);
            let _ = RegisterShellHookWindow(self.hwnd);
        }
        self.wake();
        self.frame();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        if !self.cfg.auto_hide {
            self.reserve();
        }
    }

    pub fn shutdown(&mut self) {
        self.unreserve();
        unsafe {
            let _ = DeregisterShellHookWindow(self.hwnd);
        }
    }

    pub fn wake(&mut self) {
        if !self.awake {
            self.awake = true;
            self.last_t = self.clock.elapsed().as_secs_f64();
        }
    }

    // -------------------------------------------------------------- AppBar

    fn appbar_data(&self) -> APPBARDATA {
        APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uCallbackMessage: CB_APPBAR,
            uEdge: ABE_BOTTOM,
            ..Default::default()
        }
    }

    /// Reserve the strip at the bottom so maximized windows stop above the dock.
    fn reserve(&mut self) {
        let t = ((self.plate_h() + BOTTOM_GAP * 2.0) * self.dpi).round() as i32;
        let (sw, sh) = ((self.screen_w * self.dpi).round() as i32, (self.screen_h * self.dpi).round() as i32);
        let mut d = self.appbar_data();
        unsafe {
            if !self.appbar {
                SHAppBarMessage(ABM_NEW, &mut d);
                self.appbar = true;
            }
            d.rc.left = 0;
            d.rc.right = sw;
            d.rc.top = sh - t;
            d.rc.bottom = sh;
            SHAppBarMessage(ABM_QUERYPOS, &mut d);
            d.rc.top = d.rc.bottom - t;
            SHAppBarMessage(ABM_SETPOS, &mut d);
        }
    }

    fn unreserve(&mut self) {
        if self.appbar {
            let mut d = self.appbar_data();
            unsafe {
                SHAppBarMessage(ABM_REMOVE, &mut d);
            }
            self.appbar = false;
        }
    }

    // -------------------------------------------------------------- auto-hide

    pub fn auto_hide(&self) -> bool {
        self.cfg.auto_hide
    }

    /// Change a setting owned by another part of the app (the menu bar) through the dock's config, so the
    /// dock's next save doesn't write the old value back.
    pub fn update_config(&mut self, f: impl FnOnce(&mut Config)) -> &Config {
        f(&mut self.cfg);
        self.cfg.save();
        &self.cfg
    }

    pub fn set_auto_hide(&mut self, on: bool) {
        self.cfg.auto_hide = on;
        self.cfg.save();
        unsafe {
            if on {
                self.unreserve();
                SetTimer(self.hwnd, TIMER_EDGE, 60, None);
            } else {
                self.reserve();
                let _ = KillTimer(self.hwnd, TIMER_EDGE);
            }
        }
        self.wake();
    }

    /// The pointer is on the bottom edge of the primary screen (where the hidden dock waits).
    fn at_bottom_edge(&self) -> bool {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        p.y as f64 >= (self.screen_h * self.dpi).round() - 2.0 && p.x >= 0 && (p.x as f64) < (self.screen_w * self.dpi).round()
    }

    /// Show the dock for a moment even when hidden: a genie aiming at it, an app asking for attention.
    pub fn peek(&mut self, seconds: f64) {
        if !self.cfg.auto_hide {
            return;
        }
        let until = Instant::now() + Duration::from_secs_f64(seconds);
        if until > self.peek_until {
            self.peek_until = until;
        }
        self.wake();
    }

    fn slide(&self) -> f64 {
        self.hide * (self.plate_h() + BOTTOM_GAP + 3.0)
    }

    // -------------------------------------------------------------- display changes

    fn apply_display(&mut self) {
        self.drawn.clear();
        (self.screen_w, self.screen_h, self.dpi) = primary_screen();
        self.fit_b = f64::MAX;
        self.fit_icons();
        if !self.cfg.auto_hide {
            self.reserve();
        }
        self.fit_window();
        self.load_frost();
        // icons were cut for the old scale
        self.icons.clear();
        let px = self.icon_px();
        for i in 0..self.items.len() {
            let it = &self.items[i];
            let icon = if it.trash {
                shell::icon(TRASH, px).map(Rc::new)
            } else if it.stack {
                downloads_dir().and_then(|d| shell::icon(&d, px)).map(Rc::new)
            } else {
                it.launch.clone().and_then(|src| self.app_icon(&it.key, &src, it.exe.clone().as_deref()))
            };
            self.items[i].icon = icon;
            self.items[i].badge_text = None;
        }
        self.label = None;
        self.wake();
    }

    fn load_frost(&mut self) {
        // 240 px wide: light enough blur that the wallpaper still reads through the glass, as on macOS
        let fh = (240.0 * self.screen_h / self.screen_w).round().max(1.0) as u32;
        self.frost = gfx::wallpaper(240, fh);
        let gw = (self.screen_w * self.dpi / 2.0).round().max(1.0) as u32;
        let gh = (self.screen_h * self.dpi / 2.0).round().max(1.0) as u32;
        self.glass = gfx::wallpaper(gw, gh).map(|mut p| {
            gfx::blur(&mut p, 1.0);
            p
        });
        // how bright the bottom strip (behind the plate) is decides the dot colour
        if let Some(fr) = &self.frost {
            let (fw, fh) = (fr.width() as usize, fr.height() as usize);
            let rows = ((fh as f64 * 0.08).ceil() as usize).max(1);
            let d = fr.data();
            let mut sum = 0.0;
            for i in ((fh - rows) * fw..fh * fw).map(|p| p * 4) {
                sum += 0.0722 * d[i] as f64 + 0.7152 * d[i + 1] as f64 + 0.2126 * d[i + 2] as f64;
            }
            self.light_glass = sum / (rows * fw) as f64 / 255.0 > 0.6;
        }
        self.drawn.clear();
        self.wake();
    }

    pub fn set_fullscreen_hidden(&mut self, on: bool) {
        self.hidden = on;
        self.drawn.clear();
        unsafe {
            let _ = ShowWindow(self.hwnd, if on { SW_HIDE } else { SW_SHOWNOACTIVATE });
            // game mode: poll slowly until the fullscreen app goes away
            SetTimer(self.hwnd, TIMER_REFRESH, if on { 3000 } else { 700 }, None);
        }
        if on {
            self.awake = false;
            self.surface = None; // its pixels are big; it comes back on the next frame
        } else {
            self.wake();
        }
    }

    // -------------------------------------------------------------- items

    /// The shell icon for an item, put on a squircle tile when tiles are on (fetched big, tiled at draw size).
    fn app_icon(&self, key: &str, spec: &str, exe: Option<&str>) -> Option<Rc<Pixmap>> {
        let px = self.icon_px();
        // a picked icon is drawn as it is (already shaped, e.g. a macOS-style PNG), only trimmed and fitted
        if let Some(path) = self.cfg.icon_overrides.get(key).and_then(|v| v.as_str()) {
            let lower = path.to_lowercase();
            let raw = if lower.ends_with(".exe") || lower.ends_with(".dll") || lower.ends_with(".lnk") {
                shell::icon(path, 256)
            } else {
                gfx::image_file(path, 256)
            };
            if let Some(t) = raw.and_then(|r| tile::fit(&r, px)) {
                return Some(Rc::new(t));
            }
        }
        let ask = if self.cfg.icon_tiles { 256 } else { px };
        let raw = shell::icon(spec, ask).or_else(|| exe.and_then(|e| shell::icon(e, ask)))?;
        if self.cfg.icon_tiles {
            if let Some(t) = tile::make(&raw, px) {
                return Some(Rc::new(t));
            }
        }
        Some(Rc::new(raw))
    }

    fn pinned_item(&mut self, spec: &str) -> Item {
        let key = format!("pin:{}", spec.to_lowercase());
        let mut it = Item::new(key.clone(), String::new(), None, 1.0);
        it.launch = Some(spec.to_string());
        const APPS: &str = "shell:AppsFolder\\";
        if spec.len() > APPS.len() && spec.is_char_boundary(APPS.len()) && eq_ic(&spec[..APPS.len()], APPS) {
            let aumid = spec[APPS.len()..].to_string();
            if let Some(t) = shell::link_target(spec).filter(|t| t.to_lowercase().ends_with(".exe")) {
                it.exe = Some(t);
            }
            if eq_ic(&aumid, "Microsoft.Windows.Explorer") {
                it.exe = Some(format!("{}\\explorer.exe", std::env::var("WINDIR").unwrap_or("C:\\Windows".into())));
            }
            it.aumid = Some(aumid);
        } else if spec.to_lowercase().ends_with(".lnk") {
            it.aumid = shell::item_aumid(spec);
            it.exe = shell::link_target(spec);
        } else {
            it.exe = Some(spec.to_string());
        }
        let (name, icon) = self.icons.get(&key).cloned().unwrap_or_else(|| {
            let mut name = shell::display_name(spec)
                .unwrap_or_else(|| shell::friendly_name(it.exe.as_deref(), it.aumid.as_deref(), spec));
            if it.exe.is_some() && name.to_lowercase().ends_with(".exe") {
                name = shell::friendly_name(it.exe.as_deref(), None, &name);
            }
            let icon = self.app_icon(&key, spec, it.exe.as_deref());
            (name, icon)
        });
        self.icons.insert(key, (name.clone(), icon.clone()));
        it.name = name;
        it.icon = icon;
        it
    }

    fn running_item(&mut self, key: String, w: &AppWindow) -> Item {
        let src = match (&w.aumid, &w.exe) {
            (Some(a), _) => format!("shell:AppsFolder\\{a}"),
            (None, Some(e)) => e.clone(),
            _ => String::new(),
        };
        let (name, icon) = self.icons.get(&key).cloned().unwrap_or_else(|| {
            let name = shell::friendly_name(w.exe.as_deref(), w.aumid.as_deref(), &w.title);
            let icon = self.app_icon(&key, &src, w.exe.as_deref());
            (name, icon)
        });
        self.icons.insert(key.clone(), (name.clone(), icon.clone()));
        let mut it = Item::new(key, name, icon, 0.0);
        it.aumid = w.aumid.clone();
        it.exe = w.exe.clone();
        it.launch = if w.aumid.is_some() { Some(src) } else { w.exe.clone() };
        it
    }

    fn refresh(&mut self, initial: bool) {
        self.drawn.clear();
        let wins = self.shell.app_windows();
        let old_order: Vec<String> = self.items.iter().map(|i| i.key.clone()).collect();
        let mut old: HashMap<String, Item> = self.items.drain(..).map(|i| (i.key.clone(), i)).collect();

        let mut next: Vec<Item> = Vec::new();
        for spec in self.cfg.pinned.clone() {
            let key = format!("pin:{}", spec.to_lowercase());
            if next.iter().any(|i| i.key == key) {
                continue;
            }
            let mut it = old.remove(&key).unwrap_or_else(|| self.pinned_item(&spec));
            it.wins.clear();
            next.push(it);
        }
        let pinned_n = next.len();

        // recent apps: an unpinned app that was running and is gone joins the front of the list
        let mut cur: HashMap<String, serde_json::Value> = HashMap::new();
        for w in &wins {
            if next.iter().any(|i| i.owns(w)) {
                continue;
            }
            let id = w.aumid.clone().or(w.exe.clone()).unwrap_or_else(|| w.pid.to_string());
            let key = format!("run:{}", id.to_lowercase());
            if cur.contains_key(&key) {
                continue;
            }
            let rec = self.running_now.remove(&key).unwrap_or_else(|| {
                let launch = match (&w.aumid, &w.exe) {
                    (Some(a), _) => Some(format!("shell:AppsFolder\\{a}")),
                    (None, Some(e)) => Some(e.clone()),
                    _ => None,
                };
                serde_json::json!({
                    "Key": key, "Launch": launch, "Exe": w.exe, "Aumid": w.aumid,
                    "Name": shell::friendly_name(w.exe.as_deref(), w.aumid.as_deref(), &w.title),
                })
            });
            cur.insert(key, rec);
        }
        let quit: Vec<serde_json::Value> = self.running_now.drain().map(|(_, r)| r).filter(|r| r["Launch"].is_string()).collect();
        self.running_now = cur;
        let pinned_l: Vec<String> = self.cfg.pinned.iter().map(|p| p.to_lowercase()).collect();
        let before = self.cfg.recent.clone();
        for r in quit {
            self.cfg.recent.retain(|x| x["Key"] != r["Key"]);
            self.cfg.recent.insert(0, r);
        }
        self.cfg.recent.retain(|x| !x["Launch"].as_str().is_some_and(|l| pinned_l.contains(&l.to_lowercase())));
        self.cfg.recent.truncate(3);
        if self.cfg.recent != before {
            self.cfg.save();
        }
        if self.cfg.show_recents {
            for r in self.cfg.recent.clone() {
                let Some(key) = r["Key"].as_str().map(String::from) else { continue };
                if next.iter().any(|i| i.key == key) {
                    continue;
                }
                let mut it = old.remove(&key).unwrap_or_else(|| self.recent_item(&r));
                it.wins.clear();
                next.push(it);
            }
        }

        for w in wins {
            if let Some(owner) = next.iter_mut().find(|i| i.owns(&w)) {
                owner.wins.push(w);
                continue;
            }
            let id = w.aumid.clone().or(w.exe.clone()).unwrap_or_else(|| w.pid.to_string());
            let key = format!("run:{}", id.to_lowercase());
            let mut it = old.remove(&key).unwrap_or_else(|| self.running_item(key, &w));
            it.wins.clear();
            it.wins.push(w);
            next.push(it);
        }
        // running apps keep the order they first appeared in instead of reshuffling by focus
        let pos = |k: &str| old_order.iter().position(|o| o == k).unwrap_or(usize::MAX);
        next[pinned_n..].sort_by_key(|i| pos(&i.key));

        let mut trash = old.remove("trash").unwrap_or_else(|| {
            let mut t = Item::new("trash".into(), "Recycle Bin".into(), shell::icon(TRASH, self.icon_px()).map(Rc::new), 1.0);
            t.trash = true;
            t.launch = Some(TRASH.into());
            t
        });
        self.refreshes += 1;
        if self.refreshes % 40 == 0 {
            // empty ↔ full
            if let Some(ic) = shell::icon(TRASH, self.icon_px()) {
                trash.icon = Some(Rc::new(ic));
                self.wake();
            }
        }
        if self.cfg.downloads_stack {
            let dl = self.cfg.extra_str("DownloadsFolder").filter(|d| std::path::Path::new(d).is_dir()).or_else(downloads_dir);
            if let Some(dir) = dl {
                let mut d = old.remove("downloads").unwrap_or_else(|| {
                    let mut d = Item::new("downloads".into(), "Downloads".into(), shell::icon(&dir, self.icon_px()).map(Rc::new), 1.0);
                    d.stack = true;
                    d
                });
                d.launch = Some(dir);
                next.push(d);
            }
        }
        // dividers: after the pinned apps (before recent / running ones) and before Downloads + Trash
        let tail = next.len();
        next.push(trash);
        for it in &mut next {
            it.sep_before = false;
        }
        if pinned_n > 0 && pinned_n < tail {
            next[pinned_n].sep_before = true;
        }
        let first_tail = if tail > 0 && next[tail - 1].stack { tail - 1 } else { tail };
        next[first_tail].sep_before = true;

        for it in &mut next {
            it.leaving = false;
            if initial {
                it.presence = 1.0;
            }
            if it.launching && (it.wins.len() > it.wins_at_launch || it.launch_started.elapsed().as_secs_f64() > 10.0) {
                it.launching = false;
            }
        }
        // items that vanished shrink away in place before they are dropped
        for (i, key) in old_order.iter().enumerate() {
            if let Some(mut gone) = old.remove(key) {
                gone.leaving = true;
                gone.wins.clear();
                let at = i.min(next.len() - 1);
                next.insert(at, gone);
            }
        }
        self.items = next;

        let sig: String = self.items.iter().map(|i| {
            format!("{}{}{}{}|", i.key, if i.running() { "*" } else { "" }, if i.leaving { "-" } else { "" }, if i.launching { "^" } else { "" })
        }).collect();
        if sig != self.sig {
            self.sig = sig;
            self.fit_icons();
            self.fit_window();
            self.wake();
        }
        self.update_badges();
        if self.items.iter().any(|i| i.launching) {
            self.wake();
        }
    }

    /// Like macOS: the dock grows to the screen edges, then its icons shrink so everything still fits.
    fn fit_icons(&mut self) {
        let n = self.items.iter().filter(|i| !i.leaving).count();
        if n == 0 {
            return;
        }
        let b = (self.screen_w - 2.0 * 16.0 - PAD_X * 2.0 - 1.0) / (n as f64 * 1.14);
        let next = if b >= self.cfg.icon_size { f64::MAX } else { b.floor() };
        if next != self.fit_b {
            self.fit_b = next;
            if !self.cfg.auto_hide {
                self.reserve(); // the plate got shorter or taller
            }
            for it in &mut self.items {
                it.badge_text = None;
            }
        }
    }

    /// The window only needs to be as wide as a fully magnified dock plus room for the label.
    fn fit_window(&mut self) {
        let (b, gap) = (self.b(), self.gap());
        self.win_w = self
            .screen_w
            .min(self.items.len() as f64 * (b * self.cfg.magnify + gap) + PAD_X * 2.0 + 2.0 * gap + 320.0);
        self.win_h = BOTTOM_GAP + self.plate_h() + b * (self.cfg.magnify - 1.0) + 64.0;
    }

    // -------------------------------------------------------------- badges

    /// Read the Action Center off the UI thread; the result is picked up on a later refresh.
    fn poll_toasts(&mut self) {
        if let Some(job) = &self.toast_job {
            let done = job.lock().ok().and_then(|mut g| g.take());
            match done {
                Some(list) => {
                    self.toasts = list;
                    self.toast_job = None;
                    self.update_badges();
                }
                None => return, // still reading
            }
        }
        if self.hidden {
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        if std::thread::Builder::new()
            .name("toasts".into())
            .spawn(move || {
                let list = badges::toasts();
                if let Ok(mut g) = out.lock() {
                    *g = Some(list);
                }
            })
            .is_ok()
        {
            self.toast_job = Some(slot);
        }
    }

    /// Badge = the larger of a "(N)" window title and Action Center toasts since the app was last in front.
    fn update_badges(&mut self) {
        let Some(seen) = self.seen.as_mut() else { return };
        let fg = unsafe { GetForegroundWindow() };
        let mut changed = false;
        for it in &mut self.items {
            if it.trash || it.stack {
                continue;
            }
            let mut n = it.wins.iter().map(|w| badges::from_title(&w.title)).max().unwrap_or(0);
            if let Some(a) = &it.aumid {
                // the app is in front: whatever it showed has been seen
                let front = it.wins.iter().any(|w| w.hwnd == fg);
                let at = seen.at(a);
                let mut fresh = self.toasts.iter().filter(|t| t.1 > at && eq_ic(&t.0, a)).count() as u32;
                if front && fresh > 0 {
                    seen.mark(a);
                    fresh = 0;
                }
                n = n.max(fresh);
            }
            if n != it.badge {
                it.badge = n;
                changed = true;
            }
        }
        if changed {
            self.wake();
        }
    }

    // -------------------------------------------------------------- attention

    fn wants_attention(&mut self, h: HWND) {
        if h.0.is_null() || h == unsafe { GetForegroundWindow() } {
            return;
        }
        let pid = shell::window_pid(h);
        let (exe, aumid) = self.shell.process_info(pid);
        let probe = AppWindow { hwnd: h, pid, exe, aumid, title: String::new() };
        let Some(it) = self.items.iter_mut().find(|i| !i.leaving && !i.trash && !i.stack && (i.wins.iter().any(|x| x.hwnd == h) || i.owns(&probe))) else { return };
        if it.attention {
            return;
        }
        it.attention = true;
        it.attention_started = Instant::now();
        self.peek(ATTENTION_CYCLE * 5.0);
        self.wake();
    }

    // -------------------------------------------------------------- input

    fn cursor(&self) -> (f64, f64) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x as f64 / self.dpi - self.win_x(), p.y as f64 / self.dpi - self.win_y())
    }

    fn item_at(&self, x: f64) -> Option<usize> {
        let g = self.gap() / 2.0;
        self.items.iter().position(|i| !i.leaving && x >= i.x - g && x < i.x + i.size + g)
    }

    fn index_of(&self, key: &str) -> Option<usize> {
        self.items.iter().position(|i| i.key == key)
    }

    fn on_down(&mut self) {
        let p = self.cursor();
        self.press_pt = p;
        self.pressed = self.item_at(p.0).map(|i| self.items[i].key.clone());
        self.dragging = false;
        self.drag_out = false;
        if self.pressed.is_some() {
            unsafe {
                SetCapture(self.hwnd);
            }
        }
        self.wake();
    }

    /// Called every frame while an item is pressed: start a drag, follow it, reorder pinned items.
    fn track_drag(&mut self) {
        let Some(key) = self.pressed.clone() else { return };
        let Some(from) = self.index_of(&key) else {
            self.pressed = None;
            return;
        };
        let (mx, my) = self.cursor();
        if !self.dragging {
            let (dx, dy) = ((mx - self.press_pt.0).abs(), (my - self.press_pt.1).abs());
            if self.items[from].pinned() && (dx > 8.0 || dy > 8.0) {
                self.dragging = true;
            } else {
                return;
            }
        }
        self.drag_x = mx;
        self.drag_out = my < self.win_h - BOTTOM_GAP - self.plate_h() - 60.0;
        // reorder as the dragged icon passes a neighbour's centre
        for oi in 0..self.items.len() {
            let other = &self.items[oi];
            if oi == from || !other.pinned() || other.leaving {
                continue;
            }
            let c = other.x + other.size / 2.0;
            if (oi > from && self.drag_x > c) || (oi < from && self.drag_x < c) {
                let it = self.items.remove(from);
                self.items.insert(oi, it);
                break;
            }
        }
    }

    fn on_up(&mut self) -> Option<Act> {
        unsafe {
            let _ = ReleaseCapture();
        }
        let key = self.pressed.take()?;
        self.wake();
        let idx = self.index_of(&key)?;
        if self.dragging {
            self.dragging = false;
            if self.drag_out {
                self.unpin(idx);
            } else {
                self.cfg.pinned = self.items.iter().filter(|i| i.pinned() && !i.leaving).filter_map(|i| i.launch.clone()).collect();
                self.cfg.save();
            }
            return None;
        }
        let (mx, my) = self.cursor();
        if my < 0.0 || my > self.win_h + 2.0 || self.item_at(mx) != Some(idx) {
            return None;
        }
        self.click(idx)
    }

    fn click(&mut self, i: usize) -> Option<Act> {
        self.wake();
        let it = &mut self.items[i];
        if it.trash {
            shell::launch(TRASH);
            return None;
        }
        if it.stack {
            self.downloads_menu(i);
            return None;
        }
        let it = &mut self.items[i];
        if !it.running() {
            let l = it.launch.clone()?;
            shell::launch(&l);
            it.launching = true;
            it.launch_started = Instant::now();
            it.wins_at_launch = 0;
            return None;
        }
        // the app the user was in, even if the dock itself somehow got focus
        let mut fg = unsafe { GetForegroundWindow() };
        if shell::window_pid(fg) == std::process::id() {
            fg = self.last_app;
        }
        // a just-minimized last window can still be "foreground" while iconic: that one wants restoring
        if !unsafe { IsIconic(fg) }.as_bool() && it.wins.iter().any(|w| w.hwnd == fg) {
            return Some(Act::Minimize { fg, wins: it.wins.iter().map(|w| w.hwnd).collect() });
        }
        let minimized: Vec<HWND> = it.wins.iter().map(|w| w.hwnd).filter(|&h| unsafe { IsIconic(h) }.as_bool()).collect();
        let front = minimized.first().copied().unwrap_or(it.wins[0].hwnd);
        Some(Act::Restore { front, minimized })
    }

    /// Screen rect (DIPs) of the dock icon that owns a window, for the genie to aim at.
    pub fn icon_rect_for(&mut self, hwnd: HWND) -> (f64, f64, f64, f64) {
        self.peek(1.4); // a hidden dock slides up to catch the window
        let b = self.b();
        let plate_bottom = self.win_h - BOTTOM_GAP;
        let (wx, wy) = (self.win_x(), self.win_y());
        match self.items.iter().find(|i| !i.leaving && i.wins.iter().any(|w| w.hwnd == hwnd)) {
            Some(it) => (wx + it.x, wy + plate_bottom - PAD - it.size, it.size, it.size),
            None => (wx + self.win_w / 2.0 - b / 2.0, wy + plate_bottom - PAD - b, b, b),
        }
    }

    fn unpin(&mut self, i: usize) {
        let Some(l) = self.items[i].launch.clone() else { return };
        self.cfg.pinned.retain(|p| !eq_ic(p, &l));
        self.cfg.save();
        self.items[i].leaving = true;
        self.refresh(false);
    }

    fn keep(&mut self, i: usize) {
        let Some(l) = self.items[i].launch.clone() else { return };
        self.cfg.pinned.push(l);
        self.cfg.save();
        // it reappears as a pinned item right away, in the same spot of the running list's order
        self.items.remove(i);
        self.refresh(true);
    }

    fn context_menu(&mut self) -> Option<Act> {
        let (mx, my) = self.cursor();
        let on_plate = my >= self.win_h - BOTTOM_GAP - self.plate_h() - 4.0;
        let idx = self.item_at(mx).filter(|&i| self.items[i].size > 0.5 && (on_plate || my >= self.win_h - BOTTOM_GAP - PAD - self.items[i].size - 4.0));
        let dpi = self.dpi;
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        enum Do {
            Hide,
            QuitDock,
            OpenTrash,
            EmptyTrash,
            Activate(HWND),
            Launch,
            Unpin,
            Keep,
            Explorer,
            QuitApp,
            ChangeIcon,
            ResetIcon,
            HideStack,
            ForgetRecent,
            ToggleRecents,
            ToggleStack,
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut acts: Vec<Option<Do>> = Vec::new();
        let mut add = |e: Entry, d: Option<Do>| {
            entries.push(e);
            acts.push(d);
        };
        let anchor;
        match idx {
            None => {
                // the plate between icons (the divider on macOS): dock settings
                add(Entry::item(if self.cfg.auto_hide { "Turn Hiding Off" } else { "Turn Hiding On" }), Some(Do::Hide));
                add(Entry::Sep, None);
                add(Entry::item("Show Recent Apps").checked(self.cfg.show_recents), Some(Do::ToggleRecents));
                add(Entry::item("Show Downloads Stack").checked(self.cfg.downloads_stack), Some(Do::ToggleStack));
                if !self.cfg.menu_bar {
                    add(Entry::Sep, None);
                    add(Entry::item("Quit MacDock"), Some(Do::QuitDock));
                }
                anchor = Anchor::Point(p.x, p.y);
            }
            Some(i) => {
                let it = &self.items[i];
                if it.stack {
                    add(Entry::item("Open Downloads"), Some(Do::Launch));
                    add(Entry::item("Remove from Dock"), Some(Do::HideStack));
                } else if it.trash {
                    add(Entry::item("Open"), Some(Do::OpenTrash));
                    add(Entry::item("Empty Recycle Bin"), Some(Do::EmptyTrash));
                } else {
                    for w in it.wins.iter().take(12) {
                        let t: String = if w.title.chars().count() > 48 { w.title.chars().take(47).chain(Some('…')).collect() } else { w.title.clone() };
                        add(Entry::item(t), Some(Do::Activate(w.hwnd)));
                    }
                    if it.running() {
                        add(Entry::Sep, None);
                    }
                    if it.launch.is_some() {
                        add(Entry::item(if it.running() { "New Window" } else { "Open" }), Some(Do::Launch));
                    }
                    if !it.pinned() && !it.running() && self.cfg.recent.iter().any(|r| r["Key"] == it.key.as_str()) {
                        add(Entry::item("Remove from Recents"), Some(Do::ForgetRecent));
                    }
                    if it.pinned() {
                        add(Entry::item("Remove from Dock"), Some(Do::Unpin));
                    } else if it.launch.is_some() {
                        add(Entry::item("Keep in Dock"), Some(Do::Keep));
                    }
                    if it.exe.as_ref().is_some_and(|e| std::path::Path::new(e).exists()) {
                        add(Entry::item("Show in Explorer"), Some(Do::Explorer));
                    }
                    add(Entry::item("Change Icon…"), Some(Do::ChangeIcon));
                    if self.cfg.icon_overrides.contains_key(&it.key) {
                        add(Entry::item("Reset Icon"), Some(Do::ResetIcon));
                    }
                    if it.running() {
                        add(Entry::Sep, None);
                        add(Entry::item("Quit"), Some(Do::QuitApp));
                    }
                }
                // centred above the icon, its bottom a little over the icon's top
                let slide = self.slide();
                let top = self.win_y() + self.win_h - BOTTOM_GAP - PAD - it.size + slide - 6.0;
                anchor = Anchor::Above(((self.win_x() + it.x + it.size / 2.0) * dpi).round() as i32, (top * dpi).round() as i32);
            }
        }
        let chosen = self.popup(&entries, anchor, dpi);
        self.wake();
        let d = chosen.and_then(|c| acts.get_mut(c).and_then(|a| a.take()))?;
        let i = idx.unwrap_or(0);
        match d {
            Do::Hide => self.set_auto_hide(!self.cfg.auto_hide),
            Do::QuitDock => return Some(Act::Quit),
            Do::OpenTrash => shell::launch(TRASH),
            Do::EmptyTrash => {
                unsafe {
                    let _ = SHEmptyRecycleBinW(HWND::default(), PCWSTR::null(), 0);
                }
                if let Some(ic) = shell::icon(TRASH, self.icon_px()) {
                    if let Some(t) = self.items.iter_mut().find(|i| i.trash) {
                        t.icon = Some(Rc::new(ic));
                    }
                }
            }
            Do::Activate(h) => shell::activate(h),
            Do::Launch => {
                if let Some(l) = self.items[i].launch.clone() {
                    shell::launch(&l);
                }
            }
            Do::Unpin => self.unpin(i),
            Do::Keep => self.keep(i),
            Do::Explorer => {
                if let Some(e) = self.items[i].exe.clone() {
                    shell::show_in_explorer(&e);
                }
            }
            Do::QuitApp => {
                for w in &self.items[i].wins {
                    shell::close(w.hwnd);
                }
            }
            Do::ChangeIcon => {
                if let Some(path) = shell::pick_icon_file(self.hwnd) {
                    let key = self.items[i].key.clone();
                    self.cfg.icon_overrides.insert(key, path.into());
                    self.cfg.save();
                    self.reload_icon(i);
                }
            }
            Do::ToggleRecents => {
                self.cfg.show_recents = !self.cfg.show_recents;
                self.cfg.save();
                self.refresh(false);
            }
            Do::ToggleStack => {
                self.cfg.downloads_stack = !self.cfg.downloads_stack;
                self.cfg.save();
                self.refresh(false);
            }
            Do::HideStack => {
                self.cfg.downloads_stack = false;
                self.cfg.save();
                self.items[i].leaving = true;
                self.refresh(false);
            }
            Do::ForgetRecent => {
                let key = self.items[i].key.clone();
                self.cfg.recent.retain(|r| r["Key"] != key.as_str());
                self.cfg.save();
                self.refresh(false);
            }
            Do::ResetIcon => {
                let key = self.items[i].key.clone();
                self.cfg.icon_overrides.remove(&key);
                self.cfg.save();
                self.reload_icon(i);
            }
        }
        None
    }

    /// Take in what changed behind the plate; repaint only when the strip under it really changed.
    fn backdrop_tick(&mut self) {
        let (frames, layout) = std::mem::take(&mut self.bd_due);
        self.bd_last = self.clock.elapsed().as_secs_f64();
        let Some(b) = self.backdrop.as_mut() else { return };
        if frames {
            b.handled(false);
        }
        if layout {
            b.handled(true);
            unsafe { SetTimer(self.hwnd, TIMER_BACKDROP_RECHECK, 400, None) };
        }
        let ch = !self.hidden && b.update(b.region());
        if ch {
            self.backdrop_gen += 1;
            self.wake();
        }
    }

    fn recent_item(&mut self, r: &serde_json::Value) -> Item {
        let s = |k: &str| r[k].as_str().map(String::from);
        let key = s("Key").unwrap_or_default();
        let launch = s("Launch").unwrap_or_default();
        let (name, icon) = self.icons.get(&key).cloned().unwrap_or_else(|| {
            (s("Name").unwrap_or_default(), self.app_icon(&key, &launch, s("Exe").as_deref()))
        });
        self.icons.insert(key.clone(), (name.clone(), icon.clone()));
        let mut it = Item::new(key, name, icon, 0.0);
        it.aumid = s("Aumid");
        it.exe = s("Exe");
        it.launch = Some(launch);
        it
    }

    /// Show a dock menu with the hover label cleared first (it would show through behind the menu).
    fn popup(&mut self, entries: &[Entry], anchor: Anchor, dpi: f64) -> Option<usize> {
        self.menu_up = true;
        self.label_alpha = 0.0;
        self.frame();
        let chosen = menu::popup(self.hwnd, entries, anchor, dpi);
        self.menu_up = false;
        chosen
    }

    /// The Downloads stack: its newest files in a menu above the icon (macOS "List" view); click opens one.
    fn downloads_menu(&mut self, i: usize) {
        let Some(dir) = self.items[i].launch.clone() else { return };
        let mut files: Vec<(std::time::SystemTime, String, String)> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.eq_ignore_ascii_case("desktop.ini") {
                    return None;
                }
                let md = e.metadata().ok()?;
                let t = md.created().or_else(|_| md.modified()).ok()?;
                Some((t, name, e.path().to_string_lossy().into_owned()))
            }).collect())
            .unwrap_or_default();
        files.sort_by(|a, b| b.0.cmp(&a.0));
        files.truncate(15);
        let mut entries: Vec<Entry> = files.iter().map(|f| {
            let t: String = if f.1.chars().count() > 44 { f.1.chars().take(43).chain(Some('…')).collect() } else { f.1.clone() };
            Entry::item(t)
        }).collect();
        if entries.is_empty() {
            entries.push(Entry::item("No files").enabled(false));
        }
        entries.push(Entry::Sep);
        entries.push(Entry::item("Open in Explorer"));
        let it = &self.items[i];
        let top = self.win_y() + self.win_h - BOTTOM_GAP - PAD - it.size + self.slide() - 6.0;
        let anchor = Anchor::Above(((self.win_x() + it.x + it.size / 2.0) * self.dpi).round() as i32, (top * self.dpi).round() as i32);
        let n = entries.len();
        let chosen = self.popup(&entries, anchor, self.dpi);
        self.wake();
        match chosen {
            Some(c) if c == n - 1 => shell::launch(&dir),
            Some(c) if c < files.len() => shell::launch(&files[c].2),
            _ => {}
        }
    }

    /// Fetch item `i`'s icon again (after its override changed) and redraw.
    fn reload_icon(&mut self, i: usize) {
        let it = &self.items[i];
        let key = it.key.clone();
        let spec = it.launch.clone().or_else(|| it.exe.clone()).unwrap_or_default();
        let exe = it.exe.clone();
        let icon = self.app_icon(&key, &spec, exe.as_deref());
        if let Some(e) = self.icons.get_mut(&key) {
            e.1 = icon.clone();
        }
        self.items[i].icon = icon;
        self.drawn.clear();
        self.wake();
    }

    // -------------------------------------------------------------- frame

    pub fn frame(&mut self) {
        if self.hidden {
            self.awake = false;
            return;
        }
        if self.pressed.is_some() {
            self.track_drag();
        }
        let t = self.clock.elapsed().as_secs_f64();
        let dt = (t - self.last_t).clamp(0.0, 0.05);
        self.last_t = t;
        let now = Instant::now();

        let (b, gap, plate_h, mag) = (self.b(), self.gap(), self.plate_h(), self.cfg.magnify);
        let (mx, my) = self.cursor();
        let plate_bottom = self.win_h - BOTTOM_GAP;
        let plate_top = plate_bottom - plate_h;
        let tallest = self.items.iter().map(|i| i.size).fold(b, f64::max);
        let mut hovering = self.hide < 0.5
            && mx >= self.plate_left
            && mx <= self.plate_left + self.plate_w
            && my >= plate_bottom - PAD - tallest - 6.0
            && my <= self.win_h + 2.0;
        if self.dragging {
            hovering = my >= plate_top - 70.0;
        }

        let kh = 1.0 - (-dt * 13.0).exp();
        if self.cfg.auto_hide {
            if hovering || self.dragging || self.pressed.is_some() || self.at_bottom_edge() {
                self.last_near = now;
            }
            // linger a moment after the pointer leaves, like macOS
            let show = now < self.peek_until || now_secs(now, self.last_near) < 0.35;
            self.hide = approach(self.hide, if show { 0.0 } else { 1.0 }, kh);
        } else {
            self.hide = approach(self.hide, 0.0, kh);
        }

        // magnification targets come from the resting layout so they don't feed back on themselves
        let rest_w = self.width(|_| 1.0) + PAD_X * 2.0;
        let mut x = (self.win_w - rest_w) / 2.0 + PAD_X;
        let r = b * 2.9;
        let (k, kp) = (1.0 - (-dt * 17.0).exp(), 1.0 - (-dt * 9.0).exp());
        let mut settled = !self.dragging && self.pressed.is_none();
        let fg = unsafe { GetForegroundWindow() };
        for it in &mut self.items {
            if it.sep_before {
                x += gap + 1.0;
            }
            let center = x + b * it.presence / 2.0;
            x += (b + gap) * it.presence;
            let mut target = 1.0;
            if hovering {
                let d = (mx - center).abs();
                if d < r {
                    target = 1.0 + (mag - 1.0) * ((std::f64::consts::PI * d / r).cos() + 1.0) / 2.0;
                }
            }
            it.scale = approach(it.scale, target, k);
            it.presence = approach(it.presence, if it.leaving { 0.0 } else { 1.0 }, kp);
            if it.attention {
                // stop once the user goes to the app, or after a few bounces like macOS does for a request
                if it.wins.iter().any(|w| w.hwnd == fg) || it.attention_started.elapsed().as_secs_f64() > ATTENTION_CYCLE * 5.0 {
                    it.attention = false;
                }
            }
            if it.scale != target || (!it.leaving && it.presence != 1.0) || it.launching || it.attention || it.bounce_t > 0.0 {
                settled = false;
            }
        }
        self.items.retain(|i| !(i.leaving && i.presence < 0.02));

        let content_w = self.width(|i| i.scale);
        self.plate_w = content_w + PAD_X * 2.0;
        self.plate_left = (self.win_w - self.plate_w) / 2.0;

        // place icons left to right
        self.hovered = None;
        self.seps.clear();
        let mut x = self.plate_left + PAD_X;
        for (idx, it) in self.items.iter_mut().enumerate() {
            if it.sep_before {
                self.seps.push(x + gap * 0.5);
                x += gap + 1.0;
            }
            let size = b * it.scale * it.presence;
            it.x = x;
            it.size = size;
            if it.attention || it.attention_bounce {
                let prev = it.bounce_t % ATTENTION_CYCLE;
                it.bounce_t += dt;
                it.attention_bounce = true;
                let cur = it.bounce_t % ATTENTION_CYCLE;
                if !it.attention && cur < prev {
                    it.bounce_t = 0.0;
                    it.attention_bounce = false;
                }
            } else if it.launching || it.bounce_t > 0.0 {
                let prev = it.bounce_t % LAUNCH_PERIOD;
                it.bounce_t += dt;
                let cur = it.bounce_t % LAUNCH_PERIOD;
                if !it.launching && cur < prev {
                    it.bounce_t = 0.0;
                }
            }
            let want = if it.running() { 1.0 } else { 0.0 };
            it.dot = approach(it.dot, want, kp);
            if it.dot != want {
                settled = false;
            }
            if hovering && !self.dragging && mx >= x - gap / 2.0 && mx < x + size + gap / 2.0 {
                self.hovered = Some(idx);
            }
            x += (b * it.scale + gap) * it.presence;
        }

        let want = if self.hovered.is_some() && !self.menu_up { 1.0 } else { 0.0 };
        self.label_alpha = approach(self.label_alpha, want, 1.0 - (-dt * 22.0).exp());
        if self.label_alpha != want {
            settled = false;
        }

        self.render(plate_top, plate_bottom);

        // nothing moving and the pointer is away: stop ticking until something wakes us
        let hide_target = if self.cfg.auto_hide && now >= self.peek_until && now_secs(now, self.last_near) >= 0.35 { 1.0 } else { 0.0 };
        if settled && self.hide == hide_target && !hovering && self.items.iter().all(|i| !i.leaving) {
            self.awake = false; // sleep in GetMessage until the pointer, a timer or a launch wakes us
        }
    }

    fn frame_key(&self, plate_top: f64, plate_bottom: f64, win_x: f64, win_y: f64, slide: f64, px: (u32, u32)) -> Vec<u64> {
        let f = |v: f64| v.to_bits();
        let mut k = vec![
            px.0 as u64, px.1 as u64, f(win_x), f(win_y), f(slide), f(plate_top), f(plate_bottom), f(self.plate_left),
            f(self.plate_w), f(self.seps.iter().sum::<f64>() + self.seps.len() as f64 * 1e5), f(self.label_alpha), f(self.label_at.0), f(self.label_at.1),
            self.hovered.map_or(u64::MAX, |h| h as u64),
            self.label.as_ref().map_or(0, |l| l.1.data().as_ptr() as u64),
            self.frost.as_ref().map_or(0, |p| p.data().as_ptr() as u64),
            self.dragging as u64 | (self.drag_out as u64) << 1, f(self.drag_x),
        ];
        for it in &self.items {
            let pressed = self.pressed.as_deref() == Some(it.key.as_str());
            k.extend([
                f(it.x), f(it.size), f(it.presence), f(it.dot), f(it.bounce_t),
                it.attention_bounce as u64 | (pressed as u64) << 1 | (it.badge as u64) << 2,
                it.icon.as_ref().map_or(0, |i| Rc::as_ptr(i) as u64),
                it.badge_text.as_ref().map_or(0, |t| t.1.data().as_ptr() as u64),
            ]);
        }
        k.push(self.backdrop_gen); // last: a frame differing only here redraws just the glass
        k
    }

    fn width(&self, scale: impl Fn(&Item) -> f64) -> f64 {
        let (b, gap) = (self.b(), self.gap());
        let mut w = 0.0;
        for it in &self.items {
            if it.sep_before {
                w += gap + 1.0;
            }
            w += (b * scale(it) + gap) * it.presence;
        }
        (w - gap).max(0.0)
    }

    fn render(&mut self, plate_top: f64, plate_bottom: f64) {
        let dpi = self.dpi;
        let (pw, ph) = ((self.win_w * dpi).round() as u32, (self.win_h * dpi).round() as u32);
        if self.surface.as_ref().map(|s| (s.w, s.h)) != Some((pw, ph)) {
            self.surface = Surface::new(pw.max(1), ph.max(1));
            self.drawn.clear();
        }
        // the label text is rasterized by GDI at the real pixel size, once per name
        if let Some(i) = self.hovered {
            let name = &self.items[i].name;
            if self.label.as_ref().map(|l| &l.0) != Some(name) {
                self.label = gfx::text(name, (13.0 * dpi).round() as i32).map(|p| (name.clone(), p));
            }
        }
        // badge numbers, rasterized at the largest size they get drawn at
        let b = self.b();
        let badge_h = b * 0.38;
        let badge_px = (badge_h * 0.64 * dpi * self.cfg.magnify).round() as i32;
        for it in &mut self.items {
            if it.badge > 0 && it.badge_text.as_ref().map(|t| t.0) != Some(it.badge) {
                let s = if it.badge > 999 { "999+".to_string() } else { it.badge.to_string() };
                it.badge_text = gfx::text_ex(&s, Font { face: gfx::UI, px: badge_px.max(6), weight: 600 }, (255, 255, 255)).map(|p| (it.badge, p));
            }
        }

        let (win_x, win_y) = (self.win_x(), self.win_y());
        let slide = self.slide();
        let pressed_key = self.pressed.clone();

        // Re-rendering the plate costs ~10 ms of CPU on battery; when nothing it depends on has changed
        // (pointer resting on the dock, or the dock awake only because something else animates), skip it.
        let key = self.frame_key(plate_top, plate_bottom, win_x, win_y, slide, (pw, ph));
        if !self.drawn.is_empty() && key == self.drawn {
            return;
        }
        // only what is behind the plate changed: keep the icon layer, redraw just the glass under it
        let only_backdrop = self.over.is_some() && key.len() == self.drawn.len() && key[..key.len() - 1] == self.drawn[..key.len() - 1];
        self.drawn = key;
        let Some(surface) = self.surface.as_mut() else { return };
        let mut pm = surface.pixmap();
        pm.fill(Color::TRANSPARENT);

        let base = Transform::from_scale(dpi as f32, dpi as f32).pre_translate(0.0, slide as f32);
        let (pl, pt, pwid, phgt) = (self.plate_left as f32, plate_top as f32, self.plate_w as f32, (plate_bottom - plate_top) as f32);
        let plate = gfx::rrect(pl, pt, pwid, phgt, RADIUS as f32);
        // colours are (b, g, r, a): the surface is BGRA
        let white = |a: u8| Color::from_rgba8(255, 255, 255, a);

        let mut has_glass = false;
        if let Some(plate) = &plate {
            // soft drop shadow: stacked, growing, faint pills (cheap stand-in for a blur, redrawn every frame).
            // Only the ring outside the plate is filled: the glass covers the rest.
            for i in 1..=6 {
                let g = i as f32 * 1.0;
                let ring = gfx::rrect(pl - g, pt - g + 1.5, pwid + g * 2.0, phgt + g * 2.0, RADIUS as f32 + g).map(|outer| {
                    let mut pb = PathBuilder::new();
                    pb.push_path(&outer);
                    pb.push_path(plate);
                    pb.finish()
                });
                if let Some(p) = ring.flatten() {
                    let mut paint = Paint::default();
                    paint.anti_alias = true;
                    paint.set_color(Color::from_rgba8(0, 0, 0, 0x03));
                    pm.fill_path(&p, &paint, FillRule::EvenOdd, base, None);
                }
            }
            // what is behind the plate, in screen px: the wallpaper, then every app window there, live
            let region = windows::Win32::Foundation::RECT {
                left: (win_x * dpi).floor() as i32,
                top: ((win_y + plate_top) * dpi).floor() as i32,
                right: ((win_x + self.win_w) * dpi).ceil() as i32,
                bottom: ((win_y + plate_bottom) * dpi).ceil() as i32,
            };
            let glass_id = self.glass.as_ref().map_or(0, |g| g.data().as_ptr() as usize);
            let cached = self.behind.as_ref().is_some_and(|c| c.0 == region && c.1 == self.backdrop_gen && c.2 == glass_id)
                && self.backdrop.as_ref().is_none_or(|b| b.region() == region);
            if !cached {
                self.behind = None;
            }
            let fresh = if cached { None } else { self.glass.as_ref().and_then(|g| {
                if self.wall_strip.as_ref().is_none_or(|w| w.0 != region || w.1 != glass_id) {
                    let mut strip = Pixmap::new((region.right - region.left).max(1) as u32, (region.bottom - region.top).max(1) as u32)?;
                    let (sx, sy) = ((self.screen_w * dpi) as f32 / g.width() as f32, (self.screen_h * dpi) as f32 / g.height() as f32);
                    strip.draw_pixmap(
                        0, 0, g.as_ref(), &PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() },
                        Transform::from_scale(sx, sy).post_translate(-region.left as f32, -region.top as f32), None,
                    );
                    self.wall_strip = Some((region, glass_id, strip));
                }
                let mut comp = self.wall_strip.as_ref()?.2.clone();
                if let Some(b) = self.backdrop.as_mut() {
                    if self.hidden {
                        b.clear();
                    } else {
                        if region != b.region() {
                            b.update(region);
                        }
                        b.draw(region, &mut comp);
                    }
                }
                gfx::soften(&mut comp);
                Some(comp)
            }) };
            if let Some(c) = fresh {
                self.behind = Some((region, self.backdrop_gen, glass_id, c));
            }
            let d = dpi as f32;
            let gkey = [
                (pl * d * 16.0) as u32, ((pt + slide as f32) * d * 16.0) as u32, (pwid * d * 16.0) as u32, (phgt * d * 16.0) as u32,
                ((win_x * dpi * 16.0) as u32) ^ ((win_y * dpi * 16.0) as u32).rotate_left(16), (self.backdrop_gen as u32) ^ (glass_id as u32),
            ];
            if !fresh_used(&self.glass_layer, &gkey) || self.glass_layer.is_none() {
                self.glass_layer = self.behind.as_ref().map(|c| &c.3).and_then(|g| {
                    gfx::liquid_glass(
                        g, pl * d, (pt + slide as f32) * d, pwid * d, phgt * d, RADIUS as f32 * d, 16.0 * d,
                        ((win_x * dpi) as f32, (win_y * dpi) as f32), (region.left as f32, region.top as f32),
                    )
                }).map(|(mut p, x, y)| {
                    // the plate's white tint, baked into the cached layer instead of a gradient fill per frame
                    gfx::white_wash(&mut p, (pt + slide as f32) * d - y as f32, phgt * d, 0x14, 0x06);
                    (gkey, p, x, y)
                });
            }
            let glass = self.glass_layer.as_ref();
            if let Some((_, layer, gx, gy)) = glass {
                gfx::blit_over(&mut pm, layer, *gx, *gy);
            } else if let Some(fr) = &self.frost {
                let (fx, fy) = ((self.screen_w / fr.width() as f64) as f32, (self.screen_h / fr.height() as f64) as f32);
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.shader = Pattern::new(
                    fr.as_ref(), SpreadMode::Pad, FilterQuality::Bilinear, 1.0,
                    Transform::from_scale(fx, fy).post_translate(-(win_x as f32), -((win_y + slide) as f32)),
                );
                pm.fill_path(plate, &paint, FillRule::Winding, base, None);
            }
            has_glass = glass.is_some();
        }
        // everything over the glass goes on its own layer, kept for frames where only the backdrop changed
        let mut over = self.over.take().filter(|o| (o.width(), o.height()) == (pw, ph)).or_else(|| Pixmap::new(pw.max(1), ph.max(1)));
        if let (false, Some(over)) = (only_backdrop, over.as_mut()) {
        over.fill(Color::TRANSPARENT);
        let mut pm = over.as_mut();
        if let Some(plate) = &plate {
            let mut paint = Paint::default();
            paint.anti_alias = true;
            let grad = |a: u8, b: u8| LinearGradient::new(
                Point::from_xy(0.0, pt), Point::from_xy(0.0, pt + phgt),
                vec![GradientStop::new(0.0, white(a)), GradientStop::new(1.0, white(b))],
                SpreadMode::Pad, Transform::identity(),
            );
            if let (false, Some(g)) = (has_glass, grad(0x24, 0x10)) {
                paint.shader = g;
                pm.fill_path(plate, &paint, FillRule::Winding, base, None);
            }
            // macOS edge: a faint dark outline for separation, and a thin light rim just inside it,
            // brightest along the top like light catching glass
            let hair = Stroke { width: 1.0 / dpi as f32, ..Default::default() };
            if let Some(edge) = gfx::rrect(pl, pt, pwid, phgt, RADIUS as f32) {
                let mut dark = Paint::default();
                dark.anti_alias = true;
                dark.set_color(Color::from_rgba8(0, 0, 0, 0x38));
                pm.stroke_path(&edge, &dark, &hair, base, None);
            }
            let inset = 1.0 / dpi as f32;
            if let (Some(g), Some(rim)) = (grad(0x66, 0x14), gfx::rrect(pl + inset, pt + inset, pwid - inset * 2.0, phgt - inset * 2.0, RADIUS as f32 - inset)) {
                paint.shader = g;
                pm.stroke_path(&rim, &paint, &hair, base, None);
            }
        }
        for &sx in &self.seps {
            if let Some(r) = Rect::from_xywh(sx as f32, pt + 11.0, 1.0, phgt - 22.0) {
                let mut paint = Paint::default();
                paint.set_color(white(0x70));
                pm.fill_rect(r, &paint, base, None);
            }
        }

        // the dragged icon is drawn last so it floats over its neighbours
        let order: Vec<usize> = {
            let mut v: Vec<usize> = (0..self.items.len()).collect();
            if let Some(k) = &pressed_key {
                if let Some(p) = v.iter().position(|&i| &self.items[i].key == k) {
                    let x = v.remove(p);
                    v.push(x);
                }
            }
            v
        };
        let mut badges_to_draw = Vec::new();
        for &idx in &order {
            let it = &self.items[idx];
            if it.size < 0.5 {
                continue;
            }
            let is_pressed = pressed_key.as_deref() == Some(it.key.as_str());
            let bounce = {
                // same as bounce_of, inlined: self is partly borrowed
                if it.attention_bounce {
                    let cur = it.bounce_t % ATTENTION_CYCLE;
                    if cur < ATTENTION_HOP { b * 0.9 * (std::f64::consts::PI * cur / ATTENTION_HOP).sin() } else { 0.0 }
                } else if it.bounce_t > 0.0 {
                    b * 0.55 * (std::f64::consts::PI * (it.bounce_t % LAUNCH_PERIOD) / LAUNCH_PERIOD).sin().abs()
                } else {
                    0.0
                }
            };
            let ix = if is_pressed && self.dragging { self.drag_x - it.size / 2.0 } else { it.x };
            let iy = plate_bottom - PAD - it.size - bounce;
            let opacity = if is_pressed {
                if self.dragging { if self.drag_out { 0.45 } else { 0.9 } } else { 0.6 }
            } else {
                (it.presence * 1.3).min(1.0)
            };
            if let Some(ic) = &it.icon {
                let s = (it.size / ic.width() as f64) as f32;
                let tf = base.pre_translate(ix as f32, iy as f32).pre_scale(s, s);
                let pp = PixmapPaint { opacity: opacity as f32, quality: FilterQuality::Bilinear, ..Default::default() };
                pm.draw_pixmap(0, 0, ic.as_ref().as_ref(), &pp, tf, None);
            }
            let a = it.dot * it.presence;
            if a > 0.01 {
                // macOS: a small light dot on dark glass, a dark one on light glass
                if let Some(c) = PathBuilder::from_circle((ix + it.size / 2.0) as f32, (plate_bottom - 3.5) as f32, 2.2) {
                    let mut paint = Paint::default();
                    paint.anti_alias = true;
                    if self.light_glass {
                        paint.set_color(Color::from_rgba8(0x22, 0x1C, 0x1C, (0xC0 as f64 * a) as u8));
                    } else {
                        paint.set_color(Color::from_rgba8(0xF2, 0xF2, 0xF2, (0xE0 as f64 * a) as u8));
                    }
                    pm.fill_path(&c, &paint, FillRule::Winding, base, None);
                }
            }
            if it.badge > 0 {
                badges_to_draw.push((idx, ix, iy, opacity));
            }
        }

        // badges over every icon (a neighbour's corner overlaps them), under the label
        for (idx, ix, iy, op) in badges_to_draw {
            let it = &self.items[idx];
            let Some((_, txt)) = &it.badge_text else { continue };
            let s = it.size / b; // scaled with the icon
            let h = badge_h * s;
            let text_scale = s / self.cfg.magnify; // text was rasterized for the fully magnified size
            let (tw, th) = (txt.width() as f64 / dpi * text_scale, txt.height() as f64 / dpi * text_scale);
            let bw = (tw + 2.0 * 0.26 * h).max(h);
            let bx = ix + it.size * 0.94 - bw * 0.72;
            let by = iy + it.size * 0.05 - h * 0.32;
            if let Some(pill) = gfx::rrect(bx as f32, by as f32, bw as f32, h as f32, (h / 2.0) as f32) {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                // soft shadow: a slightly bigger, faint dark pill one pixel down
                if let Some(sh) = gfx::rrect((bx - 0.5) as f32, (by + 0.5) as f32, (bw + 1.0) as f32, (h + 1.0) as f32, (h / 2.0 + 0.5) as f32) {
                    paint.set_color(Color::from_rgba8(0, 0, 0, (0x30 as f64 * op) as u8));
                    pm.fill_path(&sh, &paint, FillRule::Winding, base, None);
                }
                paint.set_color(Color::from_rgba8(0x30, 0x3B, 0xFF, (255.0 * op) as u8)); // macOS system red
                pm.fill_path(&pill, &paint, FillRule::Winding, base, None);
            }
            let tx = bx + (bw - tw) / 2.0;
            let ty = by + (h - th) / 2.0 - h * 0.02;
            let k = (text_scale / dpi) as f32; // pixmap pixels → DIPs, then to the badge's scale
            let tf = base.pre_translate(tx as f32, ty as f32).pre_scale(k, k);
            let pp = PixmapPaint { opacity: op as f32, quality: FilterQuality::Bilinear, ..Default::default() };
            pm.draw_pixmap(0, 0, txt.as_ref(), &pp, tf, None);
        }

        // tooltip label above the hovered icon; fades out where it was
        if let (Some(i), Some((_, txt))) = (self.hovered, &self.label) {
            let it = &self.items[i];
            let (lw, lh) = (txt.width() as f64 / dpi + 22.0, txt.height() as f64 / dpi + 9.0);
            let lx = (it.x + it.size / 2.0 - lw / 2.0).clamp(4.0, (self.win_w - lw - 4.0).max(4.0));
            self.label_at = (lx, plate_bottom - PAD - it.size - lh - 10.0, lw, lh);
        }
        if self.label_alpha > 0.01 {
            if let Some((_, txt)) = &self.label {
                let (lx, ly, lw, lh) = self.label_at;
                let la = self.label_alpha;
                if let Some(bg) = gfx::rrect(lx as f32, ly as f32, lw as f32, lh as f32, 7.0) {
                    let mut paint = Paint::default();
                    paint.anti_alias = true;
                    paint.set_color(Color::from_rgba8(0x2A, 0x26, 0x26, (0xD8 as f64 * la) as u8));
                    pm.fill_path(&bg, &paint, FillRule::Winding, base, None);
                    paint.set_color(white((0x40 as f64 * la) as u8));
                    pm.stroke_path(&bg, &paint, &Stroke { width: 0.8, ..Default::default() }, base, None);
                }
                let tf = Transform::from_translate(((lx + 11.0) * dpi).round() as f32, ((ly + 4.0 + slide) * dpi).round() as f32);
                pm.draw_pixmap(0, 0, txt.as_ref(), &PixmapPaint { opacity: la as f32, ..Default::default() }, tf, None);
            }
        }

        }
        if let Some(o) = &over {
            gfx::over_sparse(pm.data_mut(), o.data());
        }
        self.over = over;
        surface.present(self.hwnd, (win_x * dpi).round() as i32, (win_y * dpi).round() as i32);
    }

    // -------------------------------------------------------------- events

    pub fn on_foreground(&mut self, h: HWND) {
        if !h.0.is_null() {
            self.last_app = h;
            self.update_badges();
        }
    }

    pub fn handle(&mut self, e: DockEv) -> Option<Act> {
        match e {
            DockEv::Wake => self.wake(),
            DockEv::Down => self.on_down(),
            DockEv::Up => return self.on_up(),
            DockEv::RightUp => return self.context_menu(),
            DockEv::Refresh => {
                if self.cfg.hide_taskbar {
                    taskbar::keep_hidden();
                }
                if !self.hidden {
                    crate::ensure_no_activate(self.hwnd);
                    self.refresh(false);
                    // safety net for window changes no event told us about
                    ev(DockEv::BackdropRecheck);
                }
            }
            DockEv::Toasts => self.poll_toasts(),
            DockEv::Edge => {
                if !self.hidden && ((self.hide > 0.0 && self.at_bottom_edge()) || (self.hide < 1.0 && !self.awake)) {
                    self.wake();
                }
            }
            DockEv::Wallpaper => self.load_frost(),
            DockEv::Backdrop(layout) => {
                if layout {
                    self.bd_due.1 = true;
                } else {
                    self.bd_due.0 = true;
                }
                let wait = self.bd_last + 1.0 / BACKDROP_FPS - self.clock.elapsed().as_secs_f64();
                if wait <= 0.0 {
                    self.backdrop_tick();
                } else if !self.bd_timer {
                    // until the timer fires, the capture thread posts nothing more (its pending flag stays set)
                    self.bd_timer = true;
                    unsafe { SetTimer(self.hwnd, TIMER_BACKDROP, (wait * 1000.0).ceil() as u32, None) };
                }
            }
            DockEv::BackdropRecheck => {
                if let Some(b) = self.backdrop.as_mut() {
                    if !self.hidden && b.update(b.region()) {
                        self.backdrop_gen += 1;
                        self.wake();
                    }
                }
            }
            DockEv::BackdropTick => {
                self.bd_timer = false;
                self.backdrop_tick();
            }
            DockEv::Flash(h) => self.wants_attention(HWND(h as *mut _)),
            // Windows sends a burst while monitors come and go: settle first
            DockEv::DisplayChanged => unsafe {
                SetTimer(self.hwnd, TIMER_DISPLAY, 400, None);
            },
            DockEv::DisplaySettled => {
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_DISPLAY);
                }
                self.apply_display();
            }
            DockEv::AppBar(ABN_POSCHANGED, _) => {
                if !self.cfg.auto_hide {
                    self.reserve();
                }
            }
            DockEv::AppBar(ABN_FULLSCREENAPP, on) => return Some(Act::Fullscreen(on != 0)),
            DockEv::AppBar(..) => {}
        }
        None
    }
}

/// The user's Downloads folder (FOLDERID_Downloads, so a moved folder is found too).
fn downloads_dir() -> Option<String> {
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), None) {
            let s = p.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const std::ffi::c_void));
            if let Some(s) = s.filter(|s| std::path::Path::new(s).is_dir()) {
                return Some(s);
            }
        }
        let s = format!("{}\\Downloads", std::env::var("USERPROFILE").ok()?);
        std::path::Path::new(&s).is_dir().then_some(s)
    }
}
