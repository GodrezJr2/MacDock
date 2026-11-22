//! MacDock Settings: every config.json switch and slider in one panel, applied as it changes.
//!
//! The panel keeps its own copy of the config for drawing and sends each change to the app as
//! `Ev::Settings`; the app writes it into the dock's config (the one that gets saved) and redoes what
//! depends on it. Menu bar, desktop widgets and the Alt+Tab switcher are built at start, so changing them
//! lights up "Restart MacDock".

use crate::config::{self, Config};
use crate::flyout::{white, Cmd, Content, Ui, ACCENT};
use crate::{push, Ev};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    IconSize,
    Magnify,
    AutoHide,
    Recents,
    Downloads,
    Tiles,
    Badges,
    HideTaskbar,
    MenuBar,
    Widgets,
    Switcher,
    BatteryPct,
    AiUsage,
    TrayIcons,
    Genie,
    GenieMs,
    MagnifySpeed,
    HideDelay,
    HideSpeed,
    ReduceMotion,
}

pub enum Change {
    /// a switch (value 0/1) or a slider value; `done` is false while a slider is still being dragged
    Set(Key, f64, bool),
    ResetAnimations,
    Restart,
    OpenConfig,
}

/// Write a change into a config. Returns false if nothing changed.
pub fn apply(c: &mut Config, k: Key, v: f64) -> bool {
    let on = v >= 0.5;
    let before = (c.icon_size, c.magnify, c.genie_ms, c.magnify_speed, c.hide_delay_ms, c.hide_speed);
    let flag = match k {
        Key::IconSize => {
            c.icon_size = v.round().clamp(config::ICON_SIZE.0, config::ICON_SIZE.1);
            None
        }
        Key::Magnify => {
            c.magnify = ((v * 20.0).round() / 20.0).clamp(config::MAGNIFY.0, config::MAGNIFY.1);
            None
        }
        Key::GenieMs => {
            c.genie_ms = ((v / 10.0).round() * 10.0).clamp(config::GENIE_MS.0, config::GENIE_MS.1);
            None
        }
        Key::MagnifySpeed => {
            c.magnify_speed = ((v * 20.0).round() / 20.0).clamp(config::SPEED.0, config::SPEED.1);
            None
        }
        Key::HideDelay => {
            c.hide_delay_ms = ((v / 10.0).round() * 10.0).clamp(config::HIDE_DELAY_MS.0, config::HIDE_DELAY_MS.1);
            None
        }
        Key::HideSpeed => {
            c.hide_speed = ((v * 20.0).round() / 20.0).clamp(config::SPEED.0, config::SPEED.1);
            None
        }
        Key::AutoHide => Some(&mut c.auto_hide),
        Key::Recents => Some(&mut c.show_recents),
        Key::Downloads => Some(&mut c.downloads_stack),
        Key::Tiles => Some(&mut c.icon_tiles),
        Key::Badges => Some(&mut c.badges),
        Key::HideTaskbar => Some(&mut c.hide_taskbar),
        Key::MenuBar => Some(&mut c.menu_bar),
        Key::Widgets => Some(&mut c.desktop_widgets),
        Key::Switcher => Some(&mut c.app_switcher),
        Key::BatteryPct => Some(&mut c.battery_percent),
        Key::AiUsage => Some(&mut c.ai_usage),
        Key::TrayIcons => Some(&mut c.tray_icons),
        Key::Genie => Some(&mut c.genie),
        Key::ReduceMotion => Some(&mut c.reduce_motion),
    };
    match flag {
        Some(f) => {
            let changed = *f != on;
            *f = on;
            changed
        }
        None => before != (c.icon_size, c.magnify, c.genie_ms, c.magnify_speed, c.hide_delay_ms, c.hide_speed),
    }
}

pub fn reset_animations(c: &mut Config) {
    let d = Config::default();
    c.genie_ms = d.genie_ms;
    c.magnify_speed = d.magnify_speed;
    c.hide_delay_ms = d.hide_delay_ms;
    c.hide_speed = d.hide_speed;
    c.reduce_motion = d.reduce_motion;
}

/// Built once at start: a change only takes effect after a restart.
fn needs_restart(k: Key) -> bool {
    matches!(k, Key::MenuBar | Key::Widgets | Key::Switcher)
}

enum Row {
    Switch(Key, &'static str),
    Slider(Key, &'static str, f64, f64),
    /// grey explanation under the rows above
    Note(&'static str),
}

const ID_RESTART: u32 = 100;
const ID_CONFIG: u32 = 101;
const ID_RESET: u32 = 102;

const W: f32 = 700.0;
const COL_GAP: f32 = 22.0;
const SWITCH_H: f32 = 36.0;
const SLIDER_H: f32 = 56.0;
const GROUP_R: f32 = 10.0;
const INSET: f32 = 12.0;

fn dock_rows() -> Vec<Row> {
    vec![
        Row::Slider(Key::IconSize, "Size", 32.0, 96.0),
        Row::Slider(Key::Magnify, "Magnification", 1.0, 3.0),
        Row::Switch(Key::AutoHide, "Automatically hide the dock"),
        Row::Switch(Key::Recents, "Show recent apps"),
        Row::Switch(Key::Downloads, "Show Downloads stack"),
        Row::Switch(Key::Tiles, "Icons on rounded tiles"),
        Row::Switch(Key::Badges, "Notification badges"),
    ]
}

fn desktop_rows() -> Vec<Row> {
    vec![
        Row::Switch(Key::HideTaskbar, "Hide the Windows taskbar"),
        Row::Switch(Key::MenuBar, "Menu bar"),
        Row::Switch(Key::BatteryPct, "Battery percentage"),
        Row::Switch(Key::AiUsage, "AI usage in the menu bar"),
        Row::Switch(Key::TrayIcons, "Tray icons in the menu bar"),
        Row::Switch(Key::Widgets, "Desktop widgets"),
        Row::Switch(Key::Switcher, "Mac-style Alt+Tab"),
    ]
}

fn anim_rows() -> Vec<Row> {
    vec![
        Row::Switch(Key::Genie, "Genie effect when minimizing"),
        Row::Slider(Key::GenieMs, "Minimize duration", config::GENIE_MS.0, 1200.0),
        Row::Slider(Key::MagnifySpeed, "Magnification speed", config::SPEED.0, 3.0),
        Row::Slider(Key::HideSpeed, "Dock show / hide speed", config::SPEED.0, 3.0),
        Row::Slider(Key::HideDelay, "Hide delay", 0.0, 1500.0),
        Row::Switch(Key::ReduceMotion, "Reduce motion"),
        Row::Note("Reduce motion snaps the dock instead of animating it. Turn the Genie effect off to minimize with the Windows animation."),
    ]
}

pub struct Settings {
    cfg: Config,
    /// values at start of the ones that need a restart
    started: (bool, bool, bool),
}

impl Settings {
    pub fn new(cfg: &Config) -> Settings {
        Settings { cfg: cfg.clone(), started: (cfg.menu_bar, cfg.desktop_widgets, cfg.app_switcher) }
    }

    /// Mark the restart-only settings as already running with these values (the process started with them).
    pub fn set_started(&mut self, menu_bar: bool, widgets: bool, switcher: bool) {
        self.started = (menu_bar, widgets, switcher);
    }

    fn restart_due(&self) -> bool {
        (self.cfg.menu_bar, self.cfg.desktop_widgets, self.cfg.app_switcher) != self.started
    }

    fn value(&self, k: Key) -> f64 {
        let c = &self.cfg;
        let b = |x: bool| if x { 1.0 } else { 0.0 };
        match k {
            Key::IconSize => c.icon_size,
            Key::Magnify => c.magnify,
            Key::GenieMs => c.genie_ms,
            Key::MagnifySpeed => c.magnify_speed,
            Key::HideDelay => c.hide_delay_ms,
            Key::HideSpeed => c.hide_speed,
            Key::AutoHide => b(c.auto_hide),
            Key::Recents => b(c.show_recents),
            Key::Downloads => b(c.downloads_stack),
            Key::Tiles => b(c.icon_tiles),
            Key::Badges => b(c.badges),
            Key::HideTaskbar => b(c.hide_taskbar),
            Key::MenuBar => b(c.menu_bar),
            Key::Widgets => b(c.desktop_widgets),
            Key::Switcher => b(c.app_switcher),
            Key::BatteryPct => b(c.battery_percent),
            Key::AiUsage => b(c.ai_usage),
            Key::TrayIcons => b(c.tray_icons),
            Key::Genie => b(c.genie),
            Key::ReduceMotion => b(c.reduce_motion),
        }
    }

    fn label(&self, k: Key) -> String {
        let v = self.value(k);
        match k {
            Key::IconSize => format!("{v:.0} px"),
            Key::Magnify => {
                if v <= 1.001 {
                    "Off".into()
                } else {
                    format!("{v:.2}×")
                }
            }
            Key::GenieMs => format!("{:.2} s", v / 1000.0),
            Key::HideDelay => format!("{:.2} s", v / 1000.0),
            Key::MagnifySpeed | Key::HideSpeed => format!("{v:.2}×"),
            _ => String::new(),
        }
    }

    /// A slider's min and max as drawn.
    fn range(k: Key) -> (f64, f64) {
        for r in dock_rows().into_iter().chain(anim_rows()) {
            if let Row::Slider(key, _, lo, hi) = r {
                if key == k {
                    return (lo, hi);
                }
            }
        }
        (0.0, 1.0)
    }

    fn key_of(id: u32) -> Option<Key> {
        ALL.get(id.checked_sub(1)? as usize).copied()
    }

    fn set(&mut self, k: Key, v: f64, done: bool) {
        if apply(&mut self.cfg, k, v) || done {
            push(Ev::Settings(Change::Set(k, v, done)));
        }
    }

    fn slide(&mut self, id: u32, fx: f32, done: bool) {
        let Some(k) = Self::key_of(id) else { return };
        let (lo, hi) = Self::range(k);
        let v = lo + (hi - lo) * slider_frac(fx) as f64;
        self.set(k, v, done);
    }

    fn group(&mut self, ui: &mut Ui, x: f32, y: f32, w: f32, title: &str, rows: &[Row]) -> f32 {
        ui.text(x + 4.0, y, title, 12.0, 600, white(0x99), 0.0);
        let top = y + 22.0;
        let h: f32 = rows
            .iter()
            .map(|r| match r {
                Row::Switch(..) => SWITCH_H,
                Row::Slider(..) => SLIDER_H,
                Row::Note(t) => note_h(ui, t, w),
            })
            .sum();
        ui.fill(x, top, w, h, GROUP_R, white(0x0F));
        let mut ry = top;
        let n = rows.len();
        for (i, r) in rows.iter().enumerate() {
            match *r {
                Row::Switch(k, label) => {
                    let restart = needs_restart(k);
                    ui.text_mid(x + INSET, ry + SWITCH_H / 2.0, label, 13.0, 400, white(0xEB), w - 90.0);
                    if restart {
                        let lw = ui.text_w(label, 13.0, 400);
                        ui.text_mid(x + INSET + lw + 6.0, ry + SWITCH_H / 2.0 + 1.0, "restart", 10.5, 400, white(0x66), 0.0);
                    }
                    ui.switch(id_of(k), x + w - INSET - 36.0, ry + (SWITCH_H - 20.0) / 2.0, self.value(k) >= 0.5);
                    ry += SWITCH_H;
                }
                Row::Slider(k, label, lo, hi) => {
                    ui.text_mid(x + INSET, ry + 17.0, label, 13.0, 400, white(0xEB), w - 110.0);
                    let val = self.label(k);
                    let vw = ui.text_w(&val, 12.0, 400);
                    ui.text_mid(x + w - INSET - vw, ry + 17.0, &val, 12.0, 400, white(0x8C), 0.0);
                    let f = ((self.value(k) - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
                    slider(ui, id_of(k), x + INSET, ry + 30.0, w - 2.0 * INSET, f);
                    ry += SLIDER_H;
                }
                Row::Note(t) => {
                    let nh = note_h(ui, t, w);
                    ui.text_wrap(x + INSET, ry + 8.0, t, 11.5, 400, white(0x80), w - 2.0 * INSET);
                    ry += nh;
                }
            }
            if i + 1 < n && !matches!(rows[i + 1], Row::Note(_)) {
                ui.fill(x + INSET, ry - 0.5, w - 2.0 * INSET, 1.0, 0.0, white(0x14));
            }
        }
        ry
    }
}

fn note_h(ui: &Ui, t: &str, w: f32) -> f32 {
    // same wrapping as text_wrap, measured only
    let max_w = w - 2.0 * INSET;
    let mut lines = 1;
    let mut cur = String::new();
    for word in t.split_whitespace() {
        let next = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if !cur.is_empty() && ui.text_w(&next, 11.5, 400) > max_w {
            lines += 1;
            cur = word.to_string();
        } else {
            cur = next;
        }
    }
    lines as f32 * ui.line_h(11.5, 400) + 16.0
}

const ALL: [Key; 20] = [
    Key::IconSize, Key::Magnify, Key::AutoHide, Key::Recents, Key::Downloads, Key::Tiles, Key::Badges,
    Key::HideTaskbar, Key::MenuBar, Key::Widgets, Key::Switcher, Key::BatteryPct, Key::AiUsage, Key::TrayIcons,
    Key::Genie, Key::GenieMs, Key::MagnifySpeed, Key::HideDelay, Key::HideSpeed, Key::ReduceMotion,
];

fn id_of(k: Key) -> u32 {
    ALL.iter().position(|&x| x == k).unwrap_or(0) as u32 + 1
}

const KNOB: f32 = 9.0;

/// Where on the track a press landed, 0..1 (the knob's centre never leaves the track).
fn slider_frac(fx: f32) -> f32 {
    fx.clamp(0.0, 1.0)
}

/// macOS settings slider: thin track, accent fill up to a white knob.
fn slider(ui: &mut Ui, id: u32, x: f32, y: f32, w: f32, f: f32) {
    // the spot spans the track exactly, so the press fraction is the value
    ui.hit(id, x, y - 6.0, w, 2.0 * KNOB + 12.0);
    let cy = y + KNOB;
    ui.fill(x, cy - 2.0, w, 4.0, 2.0, white(0x33));
    let kx = x + w * f;
    ui.fill(x, cy - 2.0, (kx - x).max(4.0), 4.0, 2.0, (ACCENT.0, ACCENT.1, ACCENT.2, 255));
    let kx = kx.clamp(x + KNOB, x + w - KNOB);
    ui.circle(kx, cy + 1.0, KNOB, (0, 0, 0, 0x40));
    ui.circle(kx, cy, KNOB, white(255));
}

fn button(ui: &mut Ui, id: u32, x: f32, y: f32, w: f32, text: &str, primary: bool) {
    let hov = ui.hit(id, x, y, w, 30.0);
    let fill = match (primary, hov) {
        (true, false) => (ACCENT.0, ACCENT.1, ACCENT.2, 255),
        (true, true) => (0x30, 0x98, 0xFF, 255),
        (false, false) => white(0x1C),
        (false, true) => white(0x2A),
    };
    ui.fill(x, y, w, 30.0, 8.0, fill);
    let tw = ui.text_w(text, 13.0, 500);
    ui.text_mid(x + (w - tw) / 2.0, y + 15.0, text, 13.0, 500, white(0xF2), 0.0);
}

impl Content for Settings {
    fn width(&self) -> f32 {
        W
    }

    fn padding(&self) -> (f32, f32) {
        (20.0, 18.0)
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let cw = ui.w;
        let col = (cw - COL_GAP) / 2.0;
        ui.text(0.0, 0.0, "MacDock Settings", 17.0, 700, white(0xF2), 0.0);
        let ver = format!("v{}", crate::update::CURRENT);
        let vw = ui.text_w(&ver, 12.0, 400);
        ui.text(cw - vw, 4.0, &ver, 12.0, 400, white(0x73), 0.0);
        let top = 40.0;

        // left: dock, then menu bar & desktop
        let mut ly = self.group(ui, 0.0, top, col, "Dock", &dock_rows());
        ly = self.group(ui, 0.0, ly + 18.0, col, "Menu Bar & Desktop", &desktop_rows());

        // right: animation, then actions
        let x = col + COL_GAP;
        let mut ry = self.group(ui, x, top, col, "Animation", &anim_rows());
        ry += 18.0;
        let due = self.restart_due();
        let msg = if due { "Restart MacDock to apply the menu bar, widgets and Alt+Tab changes." } else { "Changes apply right away and are saved to config.json." };
        ry += ui.text_wrap(x + 4.0, ry, msg, 11.5, 400, if due { white(0xD9) } else { white(0x80) }, col - 8.0) + 10.0;
        let bw = (col - 10.0) / 2.0;
        button(ui, ID_RESTART, x, ry, col, "Restart MacDock", due);
        ry += 40.0;
        button(ui, ID_RESET, x, ry, bw, "Reset Animations", false);
        button(ui, ID_CONFIG, x + bw + 10.0, ry, bw, "Open config.json", false);
        ry += 30.0;
        ly.max(ry)
    }

    fn press(&mut self, id: u32, fx: f32) {
        if Self::key_of(id).is_some_and(|k| Self::range(k) != (0.0, 1.0)) {
            self.slide(id, fx, false);
        }
    }

    fn drag(&mut self, id: u32, fx: f32) {
        if Self::key_of(id).is_some_and(|k| Self::range(k) != (0.0, 1.0)) {
            self.slide(id, fx, false);
        }
    }

    fn release(&mut self, id: u32, fx: f32) {
        if Self::key_of(id).is_some_and(|k| Self::range(k) != (0.0, 1.0)) {
            self.slide(id, fx, true);
        }
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_RESTART => push(Ev::Settings(Change::Restart)),
            ID_CONFIG => push(Ev::Settings(Change::OpenConfig)),
            ID_RESET => {
                reset_animations(&mut self.cfg);
                push(Ev::Settings(Change::ResetAnimations));
            }
            _ => {
                if let Some(k) = Self::key_of(id).filter(|&k| Self::range(k) == (0.0, 1.0)) {
                    let v = if self.value(k) >= 0.5 { 0.0 } else { 1.0 };
                    self.set(k, v, true);
                }
            }
        }
        Cmd::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_toggle_and_sliders_clamp() {
        let mut c = Config::default();
        assert!(apply(&mut c, Key::AutoHide, 1.0));
        assert!(c.auto_hide);
        assert!(!apply(&mut c, Key::AutoHide, 1.0));
        apply(&mut c, Key::GenieMs, 99999.0);
        assert_eq!(c.genie_ms, config::GENIE_MS.1);
        apply(&mut c, Key::IconSize, 47.6);
        assert_eq!(c.icon_size, 48.0);
        c.reduce_motion = true;
        reset_animations(&mut c);
        assert!(!c.reduce_motion);
        assert_eq!(c.genie_ms, 560.0);
    }

    #[test]
    fn every_key_has_an_id() {
        for (i, k) in ALL.iter().enumerate() {
            assert_eq!(id_of(*k), i as u32 + 1);
            assert_eq!(Settings::key_of(i as u32 + 1), Some(*k));
        }
    }

    /// Render the panel to target/settings.png for a look (cargo test snapshot -- --ignored).
    #[test]
    #[ignore]
    fn snapshot() {
        let mut s = Settings::new(&Config::default());
        s.cfg.menu_bar = false; // show the restart state
        let (w, h, px) = crate::flyout::snapshot(&mut s, 1.25);
        let path = format!(r"{}\target\settings.png", env!("CARGO_MANIFEST_DIR"));
        assert!(crate::gfx::save_png(&path, w, h, &px));
    }
}
