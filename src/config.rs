//! %APPDATA%\MacDock\config.json, shared with the C# build (same keys, same defaults as Config.cs).

use serde_json::{json, Map, Value};

#[derive(Clone)]
pub struct Config {
    pub pinned: Vec<String>,
    pub icon_size: f64,
    pub magnify: f64,
    pub hide_taskbar: bool,
    pub menu_bar: bool,
    pub genie: bool,
    /// Put every app icon on a macOS-style squircle tile.
    pub icon_tiles: bool,
    /// Slide the dock away until the pointer touches the bottom edge (no space reserved for it).
    pub auto_hide: bool,
    /// Red unread badges from window titles and Action Center toasts.
    pub badges: bool,
    /// Host the notification-area icons behind a ^ item in the menu bar.
    pub tray_icons: bool,
    /// Claude Code / Codex token burn and limits in the menu bar, read from their local logs.
    pub ai_usage: bool,
    /// Tokens per 5-hour Claude block that count as 100% (Claude Code logs no limit itself). None = no %.
    pub claude_limit_5h: Option<u64>,
    /// Ask Anthropic's usage endpoint (with Claude Code's own login) for Claude's real 5-hour / weekly limits.
    pub claude_usage_oauth: bool,
    /// Dock item key → image or exe the user picked as its icon (right-click → Change Icon…).
    pub icon_overrides: Map<String, Value>,
    /// Up to 3 apps that ran unpinned and quit, newest first, shown after a divider like macOS
    /// ({Key, Launch, Exe, Aumid, Name}).
    pub recent: Vec<Value>,
    pub show_recents: bool,
    /// A Downloads folder item before the Trash; click lists the newest files.
    pub downloads_stack: bool,
    /// The battery percentage next to the battery icon (off like macOS; still shown at 20% or less).
    pub battery_percent: bool,
    /// macOS-style widgets on the desktop (clock, date, battery, system).
    pub desktop_widgets: bool,
    /// macOS-style Alt+Tab: one icon per app in a frosted panel (replaces the Windows switcher).
    pub app_switcher: bool,
    /// How long the Genie minimize takes, in ms (restoring takes a little less).
    pub genie_ms: f64,
    /// Multiplier on how fast icons grow and shrink under the pointer.
    pub magnify_speed: f64,
    /// How long a hidden dock lingers after the pointer leaves, in ms.
    pub hide_delay_ms: f64,
    /// Multiplier on how fast an auto-hiding dock slides in and out.
    pub hide_speed: f64,
    /// Snap instead of animating: magnification, auto-hide and a very short minimize.
    pub reduce_motion: bool,
    /// Keys we don't know (written by a newer build) survive a save.
    extra: Map<String, Value>,
}

/// Allowed ranges (min, max) of the numeric settings.
pub const ICON_SIZE: (f64, f64) = (16.0, 128.0);
pub const MAGNIFY: (f64, f64) = (1.0, 3.0);
pub const GENIE_MS: (f64, f64) = (150.0, 1500.0);
pub const HIDE_DELAY_MS: (f64, f64) = (0.0, 2000.0);
pub const SPEED: (f64, f64) = (0.25, 4.0);

impl Config {
    /// Per-frame smoothing factor for a rate (1/s) scaled by a speed setting; 1 = jump there now.
    pub fn ease(&self, dt: f64, rate: f64, speed: f64) -> f64 {
        if self.reduce_motion {
            1.0
        } else {
            1.0 - (-dt * rate * speed).exp()
        }
    }

    /// Genie length in seconds for a minimize (false) or a restore (true).
    pub fn genie_secs(&self, restore: bool) -> f64 {
        let s = if self.reduce_motion { 0.15 } else { self.genie_ms / 1000.0 };
        if restore { s * 0.5 / 0.56 } else { s }
    }
}

pub fn dir() -> String {
    format!("{}\\MacDock", std::env::var("APPDATA").unwrap_or_default())
}

pub fn path() -> String {
    format!("{}\\config.json", dir())
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pinned: [
                r"shell:AppsFolder\Microsoft.Windows.Explorer",
                r"shell:AppsFolder\Chrome",
                r"shell:AppsFolder\Microsoft.OutlookForWindows_8wekyb3d8bbwe!Microsoft.OutlookforWindows",
                r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
                r"shell:AppsFolder\Microsoft.VisualStudioCode",
                r"shell:AppsFolder\com.squirrel.Discord.Discord",
                r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            icon_size: 52.0,
            magnify: 1.75,
            hide_taskbar: true,
            menu_bar: true,
            genie: true,
            icon_tiles: true,
            auto_hide: false,
            badges: true,
            tray_icons: true,
            ai_usage: true,
            claude_limit_5h: None,
            claude_usage_oauth: false,
            icon_overrides: Map::new(),
            recent: Vec::new(),
            show_recents: true,
            downloads_stack: true,
            desktop_widgets: true,
            app_switcher: true,
            battery_percent: false,
            genie_ms: 560.0,
            magnify_speed: 1.0,
            hide_delay_ms: 350.0,
            hide_speed: 1.0,
            reduce_motion: false,
            extra: Map::new(),
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let Some(Value::Object(mut v)) = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')).ok()) else {
            c.save();
            return c;
        };
        let mut b = |k: &str, d: &mut bool| {
            if let Some(x) = v.remove(k).and_then(|x| x.as_bool()) {
                *d = x;
            }
        };
        b("HideTaskbar", &mut c.hide_taskbar);
        b("MenuBar", &mut c.menu_bar);
        b("Genie", &mut c.genie);
        b("IconTiles", &mut c.icon_tiles);
        b("AutoHide", &mut c.auto_hide);
        b("Badges", &mut c.badges);
        b("TrayIcons", &mut c.tray_icons);
        b("AiUsage", &mut c.ai_usage);
        b("ClaudeUsageOAuth", &mut c.claude_usage_oauth);
        b("ShowRecents", &mut c.show_recents);
        b("DownloadsStack", &mut c.downloads_stack);
        b("BatteryPercent", &mut c.battery_percent);
        b("DesktopWidgets", &mut c.desktop_widgets);
        b("AppSwitcher", &mut c.app_switcher);
        b("ReduceMotion", &mut c.reduce_motion);
        let mut f = |k: &str, d: &mut f64, lo: f64, hi: f64| {
            if let Some(x) = v.remove(k).and_then(|x| x.as_f64()) {
                *d = x.clamp(lo, hi);
            }
        };
        f("GenieDuration", &mut c.genie_ms, GENIE_MS.0, GENIE_MS.1);
        f("MagnifySpeed", &mut c.magnify_speed, SPEED.0, SPEED.1);
        f("AutoHideDelay", &mut c.hide_delay_ms, HIDE_DELAY_MS.0, HIDE_DELAY_MS.1);
        f("AutoHideSpeed", &mut c.hide_speed, SPEED.0, SPEED.1);
        if let Some(Value::Array(r)) = v.remove("Recent") {
            c.recent = r.into_iter().filter(|x| x.get("Key").and_then(|k| k.as_str()).is_some()).collect();
        }
        c.claude_limit_5h = v.remove("ClaudeLimit5h").and_then(|x| x.as_u64()).filter(|x| *x > 0);
        if let Some(Value::Object(o)) = v.remove("IconOverrides") {
            c.icon_overrides = o;
        }
        if let Some(Value::Array(p)) = v.remove("Pinned") {
            c.pinned = p.iter().filter_map(|s| s.as_str().map(String::from)).collect();
        }
        if let Some(x) = v.remove("IconSize").and_then(|x| x.as_f64()) {
            c.icon_size = x.clamp(ICON_SIZE.0, ICON_SIZE.1);
        }
        if let Some(x) = v.remove("Magnify").and_then(|x| x.as_f64()) {
            c.magnify = x.clamp(MAGNIFY.0, MAGNIFY.1);
        }
        c.extra = v;
        c
    }

    /// A string setting this build has no field for (read-only, kept as it is on save).
    pub fn extra_str(&self, key: &str) -> Option<String> {
        self.extra.get(key).and_then(|v| v.as_str()).map(String::from)
    }

    pub fn save(&self) {
        let mut m = self.extra.clone();
        let fields = json!({
            "Pinned": self.pinned,
            "IconSize": self.icon_size,
            "Magnify": self.magnify,
            "HideTaskbar": self.hide_taskbar,
            "MenuBar": self.menu_bar,
            "Genie": self.genie,
            "IconTiles": self.icon_tiles,
            "AutoHide": self.auto_hide,
            "Badges": self.badges,
            "TrayIcons": self.tray_icons,
            "AiUsage": self.ai_usage,
            "ClaudeLimit5h": self.claude_limit_5h,
            "ClaudeUsageOAuth": self.claude_usage_oauth,
            "IconOverrides": self.icon_overrides,
            "Recent": self.recent,
            "ShowRecents": self.show_recents,
            "DownloadsStack": self.downloads_stack,
            "BatteryPercent": self.battery_percent,
            "DesktopWidgets": self.desktop_widgets,
            "AppSwitcher": self.app_switcher,
            "GenieDuration": self.genie_ms,
            "MagnifySpeed": self.magnify_speed,
            "AutoHideDelay": self.hide_delay_ms,
            "AutoHideSpeed": self.hide_speed,
            "ReduceMotion": self.reduce_motion,
        });
        if let Value::Object(f) = fields {
            m.extend(f);
        }
        let _ = std::fs::create_dir_all(dir());
        if let Ok(s) = serde_json::to_string_pretty(&Value::Object(m)) {
            let _ = std::fs::write(path(), s);
        }
    }
}
