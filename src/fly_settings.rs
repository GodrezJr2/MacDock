//! MacDock Settings: every setting in one floating window, applied as it changes.
//!
//! The window keeps its own copy of the config for drawing and sends each change to the app as
//! `Ev::Settings`; the app writes it into the dock's config (the one that gets saved) and redoes what
//! depends on it. Menu bar, desktop widgets, the Alt+Tab switcher and the Claude login option are set up at
//! start, so changing them lights up "Restart MacDock". Close / minimize / full screen are macOS traffic
//! lights; the title row drags the window.

use crate::config::{self, Config};
use crate::flyout::{white, Cmd, Content, Ui, ACCENT};
use crate::i18n::t;
use crate::{push, Ev};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    IconSize,
    Magnify,
    AutoHide,
    Recents,
    WindowFan,
    Downloads,
    Tiles,
    Badges,
    HideTaskbar,
    MenuBar,
    Widgets,
    Switcher,
    WinSpotlight,
    BatteryPct,
    SecondaryDocks,
    AiUsage,
    ClaudeOAuth,
    TrayIcons,
    Genie,
    GenieMs,
    MagnifySpeed,
    HideDelay,
    HideSpeed,
    ReduceMotion,
    /// 0 auto, 1 English, 2 Indonesian
    Language,
    /// 0 the Apps window, 1 the classic full-screen Launchpad
    LaunchpadStyle,
}

pub enum Change {
    /// a switch (value 0/1), a slider value or a choice; `done` is false while a slider is still being dragged
    Set(Key, f64, bool),
    ResetAnimations,
    /// every setting in this window back to its default (pinned apps, icons and widget layout stay)
    ResetAll,
    Restart,
    OpenConfig,
    Open,
}

const LANGS: [&str; 3] = ["auto", "en", "id"];
const LANG_NAMES: [&str; 3] = ["Auto", "English", "Indonesia"];
const STYLES: [&str; 2] = ["apps", "classic"];
const STYLE_NAMES: [&str; 2] = ["Apps window", "Full screen"];
/// the choice rows, in the order their option ids are numbered (ID_CHOICE + 10 * row + option)
const CHOICES: [Key; 2] = [Key::Language, Key::LaunchpadStyle];

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
        Key::Language => {
            let l = LANGS[(v.round() as usize).min(LANGS.len() - 1)];
            let changed = c.language != l;
            c.language = l.into();
            return changed;
        }
        Key::LaunchpadStyle => {
            let l = STYLES[(v.round() as usize).min(STYLES.len() - 1)];
            let changed = c.launchpad_style != l;
            c.launchpad_style = l.into();
            return changed;
        }
        Key::AutoHide => Some(&mut c.auto_hide),
        Key::Recents => Some(&mut c.show_recents),
        Key::WindowFan => Some(&mut c.window_fan),
        Key::Downloads => Some(&mut c.downloads_stack),
        Key::Tiles => Some(&mut c.icon_tiles),
        Key::Badges => Some(&mut c.badges),
        Key::HideTaskbar => Some(&mut c.hide_taskbar),
        Key::MenuBar => Some(&mut c.menu_bar),
        Key::Widgets => Some(&mut c.desktop_widgets),
        Key::Switcher => Some(&mut c.app_switcher),
        Key::WinSpotlight => Some(&mut c.win_spotlight),
        Key::BatteryPct => Some(&mut c.battery_percent),
        Key::SecondaryDocks => Some(&mut c.secondary_docks),
        Key::AiUsage => Some(&mut c.ai_usage),
        Key::ClaudeOAuth => Some(&mut c.claude_usage_oauth),
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

/// The settings in this window whose value differs from the default, with the default value.
pub fn defaults_differing(c: &Config) -> Vec<(Key, f64)> {
    let d = Config::default();
    ALL.iter().filter_map(|&k| {
        let v = value_of(&d, k);
        (value_of(c, k) != v).then_some((k, v))
    }).collect()
}

/// Set up once at start: a change only takes effect after a restart.
fn needs_restart(k: Key) -> bool {
    matches!(k, Key::MenuBar | Key::Widgets | Key::Switcher | Key::ClaudeOAuth)
}

/// The restart-only settings, in the order `Settings::started` keeps them.
fn restart_state(c: &Config) -> [bool; 4] {
    [c.menu_bar, c.desktop_widgets, c.app_switcher, c.claude_usage_oauth]
}

enum Row {
    Switch(Key, &'static str),
    Slider(Key, &'static str, f64, f64),
    /// a segmented control: one of these options
    Choice(Key, &'static str, &'static [&'static str]),
    /// grey explanation under the rows above
    Note(&'static str),
}

const ID_RESTART: u32 = 100;
const ID_CONFIG: u32 = 101;
const ID_RESET: u32 = 102;
const ID_RESET_ALL: u32 = 103;
const ID_CLOSE: u32 = 110;
const ID_MIN: u32 = 111;
const ID_ZOOM: u32 = 112;
const ID_CHOICE: u32 = 200; // 200 + option index

const W: f32 = 700.0;
const COL_GAP: f32 = 22.0;
const SWITCH_H: f32 = 36.0;
const SLIDER_H: f32 = 56.0;
const CHOICE_H: f32 = 44.0;
const GROUP_R: f32 = 10.0;
const INSET: f32 = 12.0;
/// the title row, which drags the window
const TITLE_H: f32 = 22.0;

fn dock_rows() -> Vec<Row> {
    vec![
        Row::Slider(Key::IconSize, "Size", 32.0, 96.0),
        Row::Slider(Key::Magnify, "Magnification", 1.0, 3.0),
        Row::Switch(Key::AutoHide, "Automatically hide the dock"),
        Row::Switch(Key::Recents, "Show recent apps"),
        Row::Switch(Key::WindowFan, "Fan out windows on hover"),
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
        Row::Switch(Key::TrayIcons, "Tray icons in the menu bar"),
        Row::Switch(Key::AiUsage, "AI usage in the menu bar"),
        Row::Switch(Key::ClaudeOAuth, "Claude limits from your Claude login"),
        Row::Switch(Key::Widgets, "Desktop widgets"),
        Row::Switch(Key::Switcher, "Mac-style Alt+Tab"),
        Row::Switch(Key::WinSpotlight, "Windows key opens Spotlight"),
        Row::Switch(Key::SecondaryDocks, "Docks on secondary monitors"),
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

fn general_rows() -> Vec<Row> {
    vec![Row::Choice(Key::Language, "Language", &LANG_NAMES), Row::Choice(Key::LaunchpadStyle, "All apps", &STYLE_NAMES)]
}

pub struct Settings {
    cfg: Config,
    /// the restart-only settings as this process started with them
    started: [bool; 4],
    /// "Reset to Defaults" was clicked once: the next click on it resets
    confirm_reset: bool,
}

impl Settings {
    pub fn new(cfg: &Config) -> Settings {
        Settings { cfg: cfg.clone(), started: restart_state(cfg), confirm_reset: false }
    }

    /// The restart-only settings this process is actually running with.
    pub fn set_started(&mut self, started: [bool; 4]) {
        self.started = started;
    }

    fn restart_due(&self) -> bool {
        restart_state(&self.cfg) != self.started
    }

    fn value(&self, k: Key) -> f64 {
        value_of(&self.cfg, k)
    }

    fn label(&self, k: Key) -> String {
        let v = self.value(k);
        match k {
            Key::IconSize => format!("{v:.0} px"),
            Key::Magnify => {
                if v <= 1.001 {
                    t("Off").into()
                } else {
                    format!("{v:.2}×")
                }
            }
            Key::GenieMs | Key::HideDelay => format!("{:.2} s", v / 1000.0),
            Key::MagnifySpeed | Key::HideSpeed => format!("{v:.2}×"),
            _ => String::new(),
        }
    }

    /// A slider's min and max as drawn; None for switches and choices.
    fn range(k: Key) -> Option<(f64, f64)> {
        dock_rows().into_iter().chain(anim_rows()).find_map(|r| match r {
            Row::Slider(key, _, lo, hi) if key == k => Some((lo, hi)),
            _ => None,
        })
    }

    fn key_of(id: u32) -> Option<Key> {
        ALL.get(id.checked_sub(1)? as usize).copied()
    }

    fn slider_of(id: u32) -> Option<(Key, f64, f64)> {
        let k = Self::key_of(id)?;
        Self::range(k).map(|(lo, hi)| (k, lo, hi))
    }

    fn set(&mut self, k: Key, v: f64, done: bool) {
        if apply(&mut self.cfg, k, v) || done {
            if k == Key::Language {
                crate::i18n::set(&self.cfg.language); // this window redraws before the app sees the change
            }
            push(Ev::Settings(Change::Set(k, v, done)));
        }
    }

    fn slide(&mut self, id: u32, fx: f32, done: bool) {
        let Some((k, lo, hi)) = Self::slider_of(id) else { return };
        self.set(k, lo + (hi - lo) * fx.clamp(0.0, 1.0) as f64, done);
    }

    fn group(&mut self, ui: &mut Ui, x: f32, y: f32, w: f32, title: &str, rows: &[Row]) -> f32 {
        ui.text(x + 4.0, y, t(title), 12.0, 600, white(0x99), 0.0);
        let top = y + 22.0;
        let h: f32 = rows
            .iter()
            .map(|r| match r {
                Row::Switch(..) => SWITCH_H,
                Row::Slider(..) => SLIDER_H,
                Row::Choice(..) => CHOICE_H,
                Row::Note(s) => note_h(ui, t(s), w),
            })
            .sum();
        ui.fill(x, top, w, h, GROUP_R, white(0x0F));
        let mut ry = top;
        let n = rows.len();
        for (i, r) in rows.iter().enumerate() {
            match *r {
                Row::Switch(k, label) => {
                    let label = t(label);
                    ui.text_mid(x + INSET, ry + SWITCH_H / 2.0, label, 13.0, 400, white(0xEB), w - 90.0);
                    if needs_restart(k) {
                        let lw = ui.text_w(label, 13.0, 400);
                        ui.text_mid(x + INSET + lw + 6.0, ry + SWITCH_H / 2.0 + 1.0, t("restart"), 10.5, 400, white(0x66), 0.0);
                    }
                    ui.switch(id_of(k), x + w - INSET - 36.0, ry + (SWITCH_H - 20.0) / 2.0, self.value(k) >= 0.5);
                    ry += SWITCH_H;
                }
                Row::Slider(k, label, lo, hi) => {
                    ui.text_mid(x + INSET, ry + 17.0, t(label), 13.0, 400, white(0xEB), w - 110.0);
                    let val = self.label(k);
                    let vw = ui.text_w(&val, 12.0, 400);
                    ui.text_mid(x + w - INSET - vw, ry + 17.0, &val, 12.0, 400, white(0x8C), 0.0);
                    let f = ((self.value(k) - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
                    slider(ui, id_of(k), x + INSET, ry + 30.0, w - 2.0 * INSET, f);
                    ry += SLIDER_H;
                }
                Row::Choice(k, label, opts) => {
                    ui.text_mid(x + INSET, ry + CHOICE_H / 2.0, t(label), 13.0, 400, white(0xEB), 0.0);
                    let cur = self.value(k).round() as usize;
                    let seg_w: Vec<f32> = opts.iter().map(|o| ui.text_w(t(o), 12.0, 500) + 22.0).collect();
                    let total: f32 = seg_w.iter().sum();
                    let (sx, sy, sh) = (x + w - INSET - total, ry + (CHOICE_H - 26.0) / 2.0, 26.0);
                    ui.fill(sx, sy, total, sh, 7.0, white(0x1A));
                    let mut cx = sx;
                    for (j, o) in opts.iter().enumerate() {
                        let sw = seg_w[j];
                        let row = CHOICES.iter().position(|&x| x == k).unwrap_or(0) as u32;
                        let hov = ui.hit(ID_CHOICE + 10 * row + j as u32, cx, sy, sw, sh);
                        if j == cur {
                            ui.fill(cx + 2.0, sy + 2.0, sw - 4.0, sh - 4.0, 5.5, white(0x5C));
                        } else if hov {
                            ui.fill(cx + 2.0, sy + 2.0, sw - 4.0, sh - 4.0, 5.5, white(0x24));
                        }
                        ui.text_mid(cx + 11.0, sy + sh / 2.0, t(o), 12.0, 500, white(if j == cur { 0xFF } else { 0xB3 }), 0.0);
                        cx += sw;
                    }
                    ry += CHOICE_H;
                }
                Row::Note(s) => {
                    let s = t(s);
                    let nh = note_h(ui, s, w);
                    ui.text_wrap(x + INSET, ry + 8.0, s, 11.5, 400, white(0x80), w - 2.0 * INSET);
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

/// A setting's value as the window shows it: a switch as 0/1, a choice as its index.
fn value_of(c: &Config, k: Key) -> f64 {
    let b = |x: bool| if x { 1.0 } else { 0.0 };
    match k {
        Key::IconSize => c.icon_size,
        Key::Magnify => c.magnify,
        Key::GenieMs => c.genie_ms,
        Key::MagnifySpeed => c.magnify_speed,
        Key::HideDelay => c.hide_delay_ms,
        Key::HideSpeed => c.hide_speed,
        Key::Language => LANGS.iter().position(|l| *l == c.language).unwrap_or(0) as f64,
        Key::LaunchpadStyle => STYLES.iter().position(|l| *l == c.launchpad_style).unwrap_or(0) as f64,
        Key::AutoHide => b(c.auto_hide),
        Key::Recents => b(c.show_recents),
        Key::WindowFan => b(c.window_fan),
        Key::Downloads => b(c.downloads_stack),
        Key::Tiles => b(c.icon_tiles),
        Key::Badges => b(c.badges),
        Key::HideTaskbar => b(c.hide_taskbar),
        Key::MenuBar => b(c.menu_bar),
        Key::Widgets => b(c.desktop_widgets),
        Key::Switcher => b(c.app_switcher),
        Key::WinSpotlight => b(c.win_spotlight),
        Key::BatteryPct => b(c.battery_percent),
        Key::SecondaryDocks => b(c.secondary_docks),
        Key::AiUsage => b(c.ai_usage),
        Key::ClaudeOAuth => b(c.claude_usage_oauth),
        Key::TrayIcons => b(c.tray_icons),
        Key::Genie => b(c.genie),
        Key::ReduceMotion => b(c.reduce_motion),
    }
}

fn note_h(ui: &Ui, s: &str, w: f32) -> f32 {
    // same wrapping as text_wrap, measured only
    let max_w = w - 2.0 * INSET;
    let mut lines = 1;
    let mut cur = String::new();
    for word in s.split_whitespace() {
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

const ALL: [Key; 26] = [
    Key::IconSize, Key::Magnify, Key::AutoHide, Key::Recents, Key::WindowFan, Key::Downloads, Key::Tiles, Key::Badges,
    Key::HideTaskbar, Key::MenuBar, Key::Widgets, Key::Switcher, Key::WinSpotlight, Key::BatteryPct, Key::AiUsage, Key::ClaudeOAuth,
    Key::TrayIcons, Key::SecondaryDocks, Key::Genie, Key::GenieMs, Key::MagnifySpeed, Key::HideDelay, Key::HideSpeed, Key::ReduceMotion,
    Key::Language, Key::LaunchpadStyle,
];

fn id_of(k: Key) -> u32 {
    ALL.iter().position(|&x| x == k).unwrap_or(0) as u32 + 1
}

const KNOB: f32 = 9.0;

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

/// A button for a step that cannot be undone, asking to be clicked again.
fn button_red(ui: &mut Ui, id: u32, x: f32, y: f32, w: f32, text: &str) {
    let hov = ui.hit(id, x, y, w, 30.0);
    ui.fill(x, y, w, 30.0, 8.0, if hov { (0xFF, 0x4D, 0x45, 255) } else { (0xE5, 0x3E, 0x36, 255) });
    let tw = ui.text_w(text, 13.0, 500);
    ui.text_mid(x + (w - tw) / 2.0, y + 15.0, text, 13.0, 500, white(0xFF), 0.0);
}

/// Close, minimize, zoom: macOS window buttons at the left of the title row, their marks shown on hover.
fn traffic_lights(ui: &mut Ui, cy: f32) {
    const R: f32 = 6.0;
    const STEP: f32 = 20.0;
    let x0 = 2.0 + R;
    // one hover for the group, like macOS: all three marks show together
    let over: Vec<bool> = (0..3).map(|i| ui.hit(ID_CLOSE + i, x0 + i as f32 * STEP - R - 2.0, cy - R - 3.0, 2.0 * R + 4.0, 2.0 * R + 6.0)).collect();
    let hov = over.iter().any(|&h| h);
    let colors = [(0xFF, 0x5F, 0x57, 255), (0xFE, 0xBC, 0x2E, 255), (0x28, 0xC8, 0x40, 255)];
    let mark = (0x4D, 0x00, 0x00, 0xCC);
    for (i, c) in colors.iter().enumerate() {
        let cx = x0 + i as f32 * STEP;
        ui.circle(cx, cy, R, *c);
        if !hov {
            continue;
        }
        let d = 2.6;
        match i {
            0 => {
                ui.line(cx - d, cy - d, cx + d, cy + d, 1.2, mark);
                ui.line(cx - d, cy + d, cx + d, cy - d, 1.2, mark);
            }
            1 => ui.line(cx - 3.2, cy, cx + 3.2, cy, 1.2, mark),
            _ => {
                ui.line(cx - 3.2, cy, cx + 3.2, cy, 1.2, mark);
                ui.line(cx, cy - 3.2, cx, cy + 3.2, 1.2, mark);
            }
        }
    }
}

impl Content for Settings {
    fn width(&self) -> f32 {
        W
    }

    fn padding(&self) -> (f32, f32) {
        (20.0, 14.0)
    }

    fn drag_area(&self, _x: f32, y: f32) -> bool {
        y < TITLE_H + 4.0
    }

    fn redraw_on_move(&self) -> bool {
        false // rows light up per spot; a slider being dragged redraws through the press
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let cw = ui.w;
        let col = (cw - COL_GAP) / 2.0;
        traffic_lights(ui, TITLE_H / 2.0);
        let title = t("MacDock Settings");
        let tw = ui.text_w(title, 13.0, 600);
        ui.text_mid((cw - tw) / 2.0, TITLE_H / 2.0, title, 13.0, 600, white(0xD9), 0.0);
        let ver = format!("v{}", crate::update::CURRENT);
        let vw = ui.text_w(&ver, 11.5, 400);
        ui.text_mid(cw - vw, TITLE_H / 2.0, &ver, 11.5, 400, white(0x66), 0.0);
        let top = TITLE_H + 16.0;

        // left: dock, then menu bar & desktop
        let mut ly = self.group(ui, 0.0, top, col, "Dock", &dock_rows());
        ly = self.group(ui, 0.0, ly + 18.0, col, "Menu Bar & Desktop", &desktop_rows());

        // right: animation, general, then actions
        let x = col + COL_GAP;
        let mut ry = self.group(ui, x, top, col, "Animation", &anim_rows());
        ry = self.group(ui, x, ry + 18.0, col, "General", &general_rows());
        ry += 16.0;
        let due = self.restart_due();
        let msg = t(if due { "Restart MacDock to apply the menu bar, widgets and Alt+Tab changes." } else { "Changes apply right away and are saved to config.json." });
        ry += ui.text_wrap(x + 4.0, ry, msg, 11.5, 400, if due { white(0xD9) } else { white(0x80) }, col - 8.0) + 10.0;
        let bw = (col - 10.0) / 2.0;
        button(ui, ID_RESTART, x, ry, bw, t("Restart MacDock"), due);
        if self.confirm_reset {
            button_red(ui, ID_RESET_ALL, x + bw + 10.0, ry, bw, t("Click again to reset"));
        } else {
            button(ui, ID_RESET_ALL, x + bw + 10.0, ry, bw, t("Reset to Defaults"), false);
        }
        ry += 40.0;
        button(ui, ID_RESET, x, ry, bw, t("Reset Animations"), false);
        button(ui, ID_CONFIG, x + bw + 10.0, ry, bw, t("Open config.json"), false);
        ry += 30.0;
        ly.max(ry)
    }

    fn press(&mut self, id: u32, fx: f32) {
        self.slide(id, fx, false);
    }

    fn drag(&mut self, id: u32, fx: f32) {
        self.slide(id, fx, false);
    }

    fn release(&mut self, id: u32, fx: f32) {
        self.slide(id, fx, true);
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        // a second click on Reset to Defaults confirms it; anything else in between cancels
        let confirming = std::mem::take(&mut self.confirm_reset);
        match id {
            ID_RESET_ALL if !confirming => self.confirm_reset = true,
            ID_RESET_ALL => {
                for (k, v) in defaults_differing(&self.cfg) {
                    apply(&mut self.cfg, k, v);
                }
                crate::i18n::set(&self.cfg.language);
                push(Ev::Settings(Change::ResetAll));
            }
            ID_CLOSE => return Cmd::Close,
            ID_MIN => return Cmd::Minimize,
            ID_ZOOM => return Cmd::Zoom,
            ID_RESTART => push(Ev::Settings(Change::Restart)),
            ID_CONFIG => push(Ev::Settings(Change::OpenConfig)),
            ID_RESET => {
                reset_animations(&mut self.cfg);
                push(Ev::Settings(Change::ResetAnimations));
            }
            _ if (ID_CHOICE..ID_CHOICE + 10 * CHOICES.len() as u32).contains(&id) => {
                let (row, opt) = ((id - ID_CHOICE) / 10, (id - ID_CHOICE) % 10);
                self.set(CHOICES[row as usize], opt as f64, true)
            }
            _ => {
                if let Some(k) = Self::key_of(id).filter(|&k| Self::range(k).is_none()) {
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
        assert!(apply(&mut c, Key::SecondaryDocks, 0.0));
        assert!(!c.secondary_docks);
        assert!(apply(&mut c, Key::SecondaryDocks, 1.0));
        assert!(c.secondary_docks);
        assert!(apply(&mut c, Key::AutoHide, 1.0));
        assert!(c.auto_hide);
        assert!(!apply(&mut c, Key::AutoHide, 1.0));
        apply(&mut c, Key::GenieMs, 99999.0);
        assert_eq!(c.genie_ms, config::GENIE_MS.1);
        apply(&mut c, Key::IconSize, 47.6);
        assert_eq!(c.icon_size, 48.0);
        assert!(apply(&mut c, Key::Language, 2.0));
        assert_eq!(c.language, "id");
        c.reduce_motion = true;
        reset_animations(&mut c);
        assert!(!c.reduce_motion);
        assert_eq!(c.genie_ms, 560.0);
    }

    #[test]
    fn reset_all_brings_every_setting_back() {
        let mut c = Config::default();
        assert!(defaults_differing(&c).is_empty());
        c.pinned = vec!["x.exe".into()];
        apply(&mut c, Key::IconSize, 80.0);
        apply(&mut c, Key::WindowFan, 0.0);
        apply(&mut c, Key::Language, 2.0);
        apply(&mut c, Key::ReduceMotion, 1.0);
        let diff = defaults_differing(&c);
        assert_eq!(diff.len(), 4);
        for (k, v) in diff {
            apply(&mut c, k, v);
        }
        assert!(defaults_differing(&c).is_empty());
        assert_eq!(c.pinned, vec!["x.exe".to_string()]); // not a setting of this window
    }

    #[test]
    fn every_key_has_an_id() {
        for (i, k) in ALL.iter().enumerate() {
            assert_eq!(id_of(*k), i as u32 + 1);
            assert_eq!(Settings::key_of(i as u32 + 1), Some(*k));
        }
        assert!(ALL.len() < ID_RESTART as usize);
    }

    /// Render the window to target/settings.png and settings_id.png (cargo test snapshot -- --ignored).
    #[test]
    #[ignore]
    fn snapshot() {
        for (lang, file) in [("en", "settings"), ("id", "settings_id")] {
            crate::i18n::set(lang);
            let mut s = Settings::new(&Config::default());
            s.cfg.menu_bar = false; // show the restart state
            s.cfg.language = lang.into();
            let (w, h, px) = crate::flyout::snapshot(&mut s, 1.25);
            let path = format!(r"{}\target\{file}.png", env!("CARGO_MANIFEST_DIR"));
            assert!(crate::gfx::save_png(&path, w, h, &px));
        }
        crate::i18n::set("en");
    }
}
