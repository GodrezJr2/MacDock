//! Control Center (port of ControlCenter.cs): connectivity, appearance, power mode, display, sound and now playing.

use std::sync::{Arc, Mutex};

use tiny_skia::Pixmap;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Playback,
};
use windows::Storage::Streams::DataReader;

use crate::fly_wifi::{self, spawn};
use crate::flyout::{white, Cmd, Content, Ui, ACCENT4};
use crate::menubar::net;
use crate::sysctl;

const OFF: (u8, u8, u8, u8) = (255, 255, 255, 0x30);

const ID_WIFI_DOT: u32 = 1;
const ID_WIFI_TEXT: u32 = 2;
const ID_BT_DOT: u32 = 3;
const ID_BT_TEXT: u32 = 4;
const ID_DARK: u32 = 5;
const ID_POWER_DOT: u32 = 6;
const ID_POWER_TEXT: u32 = 7;
const ID_BRIGHT: u32 = 8;
const ID_VOL: u32 = 9;
const ID_PREV: u32 = 10;
const ID_PLAY: u32 = 11;
const ID_NEXT: u32 = 12;

static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);

fn session() -> Option<Session> {
    let mut m = MANAGER.lock().unwrap();
    if m.is_none() {
        *m = Manager::RequestAsync().and_then(|o| o.get()).ok();
    }
    m.as_ref()?.GetCurrentSession().ok()
}

struct Media {
    title: String,
    artist: String,
    playing: bool,
    art: Option<Pixmap>,
}

#[derive(Default)]
struct State {
    wifi_on: bool,
    wifi_sub: String,
    bt: Option<bool>, // None = no radio
    bt_looked: bool,
    brightness: Option<f32>,
    volume: Option<f32>,
    media: Option<Media>,
}

/// (pending value, worker running)
type Job<T> = Arc<Mutex<(Option<T>, bool)>>;

pub struct ControlCenter {
    hwnd: isize,
    st: Arc<Mutex<State>>,
    dark: bool,
    power: usize,
    /// slider values while dragging (the worker's reads must not fight the mouse)
    drag: Option<(u32, f32)>,
    bright_job: Job<u8>,
    vol_job: Job<f32>,
}

fn wifi_sub(on: bool) -> String {
    if !on {
        return "Off".into();
    }
    match net::current() {
        Some((ssid, _)) => ssid,
        None if net::wired() => "Not connected · Ethernet".into(),
        None => "Not connected".into(),
    }
}

fn read_media(st: &Mutex<State>) {
    let read = || -> Option<Media> {
        let s = session()?;
        let p = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
        let title = p.Title().ok()?.to_string();
        if title.is_empty() {
            return None;
        }
        let mut artist = p.Artist().map(|a| a.to_string()).unwrap_or_default();
        if artist.is_empty() {
            let id = s.SourceAppUserModelId().map(|a| a.to_string()).unwrap_or_default();
            artist = crate::shell::friendly_name(None, Some(&id), &id);
        }
        let playing = s.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()).is_ok_and(|x| x == Playback::Playing);
        // keep the old art while the song is the same
        let same = st.lock().unwrap().media.as_ref().is_some_and(|m| m.title == title);
        let art = if same {
            st.lock().unwrap().media.as_mut().and_then(|m| m.art.take())
        } else {
            (|| -> Option<Pixmap> {
                let stream = p.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
                let size = stream.Size().ok()? as u32;
                if size == 0 || size > 8 << 20 {
                    return None;
                }
                let r = DataReader::CreateDataReader(&stream).ok()?;
                r.LoadAsync(size).ok()?.get().ok()?;
                let mut buf = vec![0u8; size as usize];
                r.ReadBytes(&mut buf).ok()?;
                crate::gfx::image_bytes(&buf, 120)
            })()
        };
        Some(Media { title, artist, playing, art })
    };
    let m = read();
    st.lock().unwrap().media = m;
}

impl ControlCenter {
    pub fn new() -> ControlCenter {
        let st = State { wifi_on: true, wifi_sub: wifi_sub(true), bt: None, ..Default::default() };
        ControlCenter {
            hwnd: 0, st: Arc::new(Mutex::new(st)), dark: sysctl::dark(), power: sysctl::power_mode(), drag: None,
            bright_job: Arc::new(Mutex::new((None, false))), vol_job: Arc::new(Mutex::new((None, false))),
        }
    }

    fn work(&self, f: impl FnOnce(&Mutex<State>) + Send + 'static) {
        let st = self.st.clone();
        spawn(self.hwnd, move || f(&st));
    }

    /// Latest-value-wins setter: one worker drains the slot so a fast drag never queues a backlog.
    fn push_job<T: Send + 'static + Copy>(&self, slot: &Job<T>, v: T, apply: fn(T)) {
        let start = {
            let mut s = slot.lock().unwrap();
            s.0 = Some(v);
            !std::mem::replace(&mut s.1, true)
        };
        if start {
            let slot = slot.clone();
            std::thread::spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
                }
                loop {
                    let next = {
                        let mut s = slot.lock().unwrap();
                        let n = s.0.take();
                        if n.is_none() {
                            s.1 = false;
                        }
                        n
                    };
                    match next {
                        Some(v) => apply(v),
                        None => break,
                    }
                }
            });
        }
    }

    fn slider_value(&self, id: u32, fx: f32, w: f32) -> f32 {
        let _ = id;
        ((fx * w - 6.0) / (w - 12.0).max(1.0)).clamp(0.0, 1.0)
    }

    fn set_slider(&mut self, id: u32, v: f32) {
        self.drag = Some((id, v));
        if id == ID_VOL {
            self.st.lock().unwrap().volume = Some(v);
            self.push_job(&self.vol_job.clone(), v, sysctl::set_volume);
        } else {
            self.st.lock().unwrap().brightness = Some(v);
            self.push_job(&self.bright_job.clone(), (v * 100.0).round() as u8, sysctl::set_brightness);
        }
    }

    fn media_cmd(&self, id: u32) {
        self.work(move |st| {
            if let Some(s) = session() {
                let _ = match id {
                    ID_PREV => s.TrySkipPreviousAsync(),
                    ID_NEXT => s.TrySkipNextAsync(),
                    _ => s.TryTogglePlayPauseAsync(),
                }
                .and_then(|o| o.get());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            read_media(st);
        });
    }
}

const SLIDER_W: f32 = 330.0 - 26.0 - 20.0 - 20.0;

impl Content for ControlCenter {
    fn width(&self) -> f32 {
        330.0
    }
    fn padding(&self) -> (f32, f32) {
        (10.0, 10.0)
    }
    fn period_ms(&self) -> u32 {
        2000
    }
    fn opened(&mut self, hwnd: isize) {
        self.hwnd = hwnd;
        self.work(|st| {
            let on = fly_wifi::is_on();
            {
                let mut s = st.lock().unwrap();
                s.wifi_on = on;
                s.wifi_sub = wifi_sub(on);
                s.volume = sysctl::volume();
            }
            read_media(st);
            let bt = sysctl::bluetooth().map(|r| r.State().is_ok_and(|x| x == windows::Devices::Radios::RadioState::On));
            {
                let mut s = st.lock().unwrap();
                s.bt = bt;
                s.bt_looked = true;
            }
            let b = sysctl::brightness();
            st.lock().unwrap().brightness = b.map(|b| b as f32 / 100.0);
        });
    }
    fn tick(&mut self) {
        self.work(read_media);
    }

    fn press(&mut self, id: u32, fx: f32) {
        if id == ID_BRIGHT || id == ID_VOL {
            let v = self.slider_value(id, fx, SLIDER_W);
            self.set_slider(id, v);
        }
    }
    fn drag(&mut self, id: u32, fx: f32) {
        if id == ID_BRIGHT || id == ID_VOL {
            let v = self.slider_value(id, fx, SLIDER_W);
            self.set_slider(id, v);
        }
    }
    fn release(&mut self, _id: u32, _fx: f32) {
        self.drag = None;
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_WIFI_DOT => {
                let on = {
                    let mut s = self.st.lock().unwrap();
                    s.wifi_on = !s.wifi_on;
                    s.wifi_sub = if s.wifi_on { "Turning on…".into() } else { "Off".into() };
                    s.wifi_on
                };
                self.work(move |st| {
                    fly_wifi::set_on(on);
                    std::thread::sleep(std::time::Duration::from_millis(if on { 3000 } else { 100 }));
                    st.lock().unwrap().wifi_sub = wifi_sub(on);
                });
            }
            ID_WIFI_TEXT => return Cmd::OpenWifi,
            ID_BT_DOT => {
                let on = {
                    let mut s = self.st.lock().unwrap();
                    let Some(b) = s.bt else { return Cmd::None };
                    s.bt = Some(!b);
                    !b
                };
                self.work(move |_| {
                    if let Some(r) = sysctl::bluetooth() {
                        sysctl::set_radio(&r, on);
                    }
                });
            }
            ID_BT_TEXT => return Cmd::Launch("ms-settings:bluetooth".into()),
            ID_DARK => {
                self.dark = !self.dark;
                let d = self.dark;
                std::thread::spawn(move || sysctl::set_dark(d));
            }
            ID_POWER_DOT => {
                self.power = (sysctl::power_mode() + 1) % 3;
                sysctl::set_power_mode(self.power);
                self.power = sysctl::power_mode();
            }
            ID_POWER_TEXT => return Cmd::Launch("ms-settings:powersleep".into()),
            ID_PREV | ID_PLAY | ID_NEXT => self.media_cmd(id),
            _ => {}
        }
        Cmd::None
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let st = self.st.lock().unwrap();
        let col = (w - 8.0) / 2.0;
        let row_h = 32.0;

        // connectivity tile (two rows) and two square tiles beside it
        let right_h = 2.0 * (10.0 + row_h + 10.0) + 8.0;
        tile(ui, 0.0, 0.0, col, right_h);
        let wifi_sub = st.wifi_sub.clone();
        toggle(ui, 10.0, 10.0, col - 20.0, '\u{E701}', "Wi-Fi", &wifi_sub, st.wifi_on, ID_WIFI_DOT, ID_WIFI_TEXT);
        let bt_sub = match st.bt {
            None if st.bt_looked => "Unavailable",
            None => "",
            Some(true) => "On",
            Some(false) => "Off",
        };
        toggle(ui, 10.0, 10.0 + row_h + 10.0, col - 20.0, '\u{E702}', "Bluetooth", bt_sub, st.bt == Some(true), ID_BT_DOT, ID_BT_TEXT);

        let rx = col + 8.0;
        let th = 10.0 + row_h + 10.0;
        tile(ui, rx, 0.0, col, th);
        toggle(ui, rx + 10.0, 10.0, col - 20.0, '\u{E708}', "Dark Mode", if self.dark { "On" } else { "Off" }, self.dark, ID_DARK, ID_DARK);
        tile(ui, rx, th + 8.0, col, th);
        let pg = match self.power {
            0 => '\u{EC0A}',
            _ => '\u{E945}',
        };
        toggle(ui, rx + 10.0, th + 8.0 + 10.0, col - 20.0, pg, "Power Mode", sysctl::POWER_MODES[self.power].0, self.power != 1, ID_POWER_DOT, ID_POWER_TEXT);
        let mut y = right_h + 8.0;

        // display and sound
        let title_h = ui.line_h(12.5, 600);
        for (id, title, v) in [(ID_BRIGHT, "Display", st.brightness), (ID_VOL, "Sound", st.volume)] {
            if id == ID_BRIGHT && v.is_none() && st.bt_looked {
                continue; // no adjustable panel (desktop monitor)
            }
            let h = 10.0 + title_h + 8.0 + 26.0 + 10.0;
            tile(ui, 0.0, y, w, h);
            ui.text(12.0, y + 10.0, title, 12.5, 600, white(0xF2), 0.0);
            let sy = y + 10.0 + title_h + 8.0;
            let v = self.drag.filter(|d| d.0 == id).map(|d| d.1).or(v).unwrap_or(0.0);
            let g = if id == ID_BRIGHT {
                '\u{E706}'
            } else if v <= 0.001 {
                '\u{E74F}'
            } else if v < 0.34 {
                '\u{E993}'
            } else if v < 0.67 {
                '\u{E994}'
            } else {
                '\u{E995}'
            };
            slider(ui, id, 10.0, sy, SLIDER_W, v, g);
            y += h + 8.0;
        }

        // now playing
        if let Some(m) = &st.media {
            let h = 60.0;
            tile(ui, 0.0, y, w, h);
            let (ax, ay) = (10.0, y + 10.0);
            ui.fill(ax, ay, 40.0, 40.0, 7.0, OFF);
            if let Some(a) = &m.art {
                // square-crop the art into the rounded box
                let side = a.width().min(a.height()) as f32;
                let (sw, sh) = (40.0 * a.width() as f32 / side, 40.0 * a.height() as f32 / side);
                ui.image_rounded(a, ax - (sw - 40.0) / 2.0, ay - (sh - 40.0) / 2.0, sw, sh, (ax, ay, 40.0, 40.0), 7.0);
            }
            let ctrl_w = 3.0 * 28.0;
            let tx = ax + 40.0 + 10.0;
            let tw = w - tx - ctrl_w - 10.0 - 6.0;
            let (h1, h2) = (ui.line_h(13.0, 600), ui.line_h(11.5, 400));
            let ty = y + (h - h1 - h2) / 2.0;
            ui.text(tx, ty, &m.title, 13.0, 600, white(0xF2), tw);
            ui.text(tx, ty + h1, &m.artist, 11.5, 400, white(0x99), tw);
            let mut bx = w - 10.0 - ctrl_w;
            for (id, g, size) in [
                (ID_PREV, '\u{E892}', 13.0),
                (ID_PLAY, if m.playing { '\u{E769}' } else { '\u{E768}' }, 15.0),
                (ID_NEXT, '\u{E893}', 13.0),
            ] {
                let by = y + (h - 28.0) / 2.0;
                if ui.hit(id, bx, by, 28.0, 28.0) {
                    ui.fill(bx, by, 28.0, 28.0, 14.0, white(0x1C));
                }
                ui.glyph(bx + 14.0, by + 14.0, g, size, white(0xF2));
                bx += 28.0;
            }
            y += h + 8.0;
        }
        y - 8.0
    }
}

fn tile(ui: &mut Ui, x: f32, y: f32, w: f32, h: f32) {
    ui.fill(x, y, w, h, 12.0, white(0x14));
    ui.outline(x, y, w, h, 12.0, 1.0, white(0x14));
}

/// Round icon button (toggles) and title/subtitle beside it (opens more).
#[allow(clippy::too_many_arguments)]
fn toggle(ui: &mut Ui, x: f32, y: f32, w: f32, g: char, title: &str, sub: &str, on: bool, dot_id: u32, text_id: u32) {
    let row_h = 32.0;
    let cy = y + row_h / 2.0;
    ui.hit(text_id, x, y, w, row_h);
    ui.hit(dot_id, x, cy - 14.0, 28.0, 28.0);
    ui.circle(x + 14.0, cy, 14.0, if on { ACCENT4 } else { OFF });
    ui.glyph(x + 14.0, cy, g, 13.0, white(0xFF));
    let tx = x + 28.0 + 9.0;
    let (h1, h2) = (ui.line_h(12.5, 600), ui.line_h(11.0, 400));
    let ty = cy - (h1 + h2) / 2.0;
    ui.text(tx, ty, title, 12.5, 600, white(0xF2), w - (tx - x));
    ui.text(tx, ty + h1, sub, 11.0, 400, white(0x99), w - (tx - x));
}

/// macOS slider: thick rounded track, white fill that never shrinks below a circle, icon inside on the left.
fn slider(ui: &mut Ui, id: u32, x: f32, y: f32, w: f32, v: f32, g: char) {
    let h = 26.0;
    ui.hit(id, x, y, w, h);
    ui.fill(x, y, w, h, 13.0, OFF);
    let fw = h + (w - h) * v.clamp(0.0, 1.0);
    ui.fill(x, y, fw, h, 13.0, white(0xFF));
    ui.glyph(x + 8.0 + 6.0, y + h / 2.0, g, 12.0, (0x55, 0x55, 0x5A, 0xFF));
}
