//! %APPDATA%\MacDock\config.json, shared with the C# build (same keys, same defaults as Config.cs).

use serde_json::{json, Map, Value};

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
    /// Keys we don't know (written by a newer build) survive a save.
    extra: Map<String, Value>,
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
        c.claude_limit_5h = v.remove("ClaudeLimit5h").and_then(|x| x.as_u64()).filter(|x| *x > 0);
        if let Some(Value::Array(p)) = v.remove("Pinned") {
            c.pinned = p.iter().filter_map(|s| s.as_str().map(String::from)).collect();
        }
        if let Some(x) = v.remove("IconSize").and_then(|x| x.as_f64()) {
            c.icon_size = x.clamp(16.0, 128.0);
        }
        if let Some(x) = v.remove("Magnify").and_then(|x| x.as_f64()) {
            c.magnify = x.clamp(1.0, 3.0);
        }
        c.extra = v;
        c
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
