//! The Wi-Fi extra (port of WifiFlyout.cs): switch, the joined network, nearby networks, join with a password.
//! Every WinRT call blocks, so each runs on a worker thread that posts WM_FLY_DATA when its result is in.

use std::sync::{Arc, Mutex};

use windows::core::HSTRING;
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Devices::WiFi::{WiFiAccessStatus, WiFiAdapter, WiFiAvailableNetwork, WiFiConnectionStatus, WiFiReconnectionKind};
use windows::Networking::Connectivity::NetworkAuthenticationType;
use windows::Security::Credentials::PasswordCredential;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::flyout::{white, Cmd, Content, Ui, ACCENT4, WM_FLY_DATA};
use crate::menubar::net;

// ---- shared Wi-Fi state (also used by Control Center)

static ADAPTER: Mutex<Option<WiFiAdapter>> = Mutex::new(None);
static ASKED: Mutex<bool> = Mutex::new(false);
static RADIO: Mutex<Option<Radio>> = Mutex::new(None);

fn mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Blocking.
pub fn adapter() -> Option<WiFiAdapter> {
    let mut a = ADAPTER.lock().unwrap();
    if a.is_none() {
        let mut asked = ASKED.lock().unwrap();
        if !*asked {
            *asked = true;
            let ok = WiFiAdapter::RequestAccessAsync().and_then(|o| o.get()).is_ok_and(|s| s == WiFiAccessStatus::Allowed);
            if !ok {
                return None;
            }
        }
        *a = WiFiAdapter::FindAllAdaptersAsync().and_then(|o| o.get()).ok().and_then(|v| v.GetAt(0).ok());
    }
    a.clone()
}

/// Blocking.
pub fn radio() -> Option<Radio> {
    let mut r = RADIO.lock().unwrap();
    if r.is_none() {
        let _ = Radio::RequestAccessAsync().and_then(|o| o.get());
        if let Ok(all) = Radio::GetRadiosAsync().and_then(|o| o.get()) {
            *r = all.into_iter().find(|x| x.Kind().is_ok_and(|k| k == RadioKind::WiFi));
        }
    }
    r.clone()
}

/// Blocking.
pub fn is_on() -> bool {
    radio().map_or(true, |r| r.State().map_or(true, |s| s != RadioState::Off))
}

/// Blocking.
pub fn set_on(on: bool) {
    if let Some(r) = radio() {
        let _ = r.SetStateAsync(if on { RadioState::On } else { RadioState::Off }).and_then(|o| o.get());
    }
}

/// Run `f` on an MTA worker and wake the flyout when it is done.
pub fn spawn(hwnd: isize, f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        mta();
        f();
        unsafe {
            let _ = PostMessageW(HWND(hwnd as *mut _), WM_FLY_DATA, WPARAM(0), LPARAM(0));
        }
    });
}

// ---- the flyout

#[derive(Clone)]
struct NetInfo {
    ssid: String,
    bars: u8,
    secured: bool,
}

#[derive(Default)]
struct State {
    loaded: bool,
    on: bool,
    current: Option<(String, u8)>,
    nets: Vec<NetInfo>,
    status: String,
    /// the network whose password row is open
    expanded: Option<String>,
    busy: bool,
}

const ID_SWITCH: u32 = 1;
const ID_CURRENT: u32 = 2;
const ID_SETTINGS: u32 = 3;
const ID_FIELD: u32 = 4;
const ID_JOIN: u32 = 5;
const ID_NET: u32 = 100;
const ROW_H: f32 = 36.0;
const LIST_MAX: f32 = 330.0;

pub struct WifiFly {
    hwnd: isize,
    st: Arc<Mutex<State>>,
    scroll: usize,
    password: String,
}

fn status_name(s: WiFiConnectionStatus) -> String {
    match s {
        WiFiConnectionStatus::AccessRevoked => "AccessRevoked".into(),
        WiFiConnectionStatus::InvalidCredential => "InvalidCredential".into(),
        WiFiConnectionStatus::NetworkNotAvailable => "NetworkNotAvailable".into(),
        WiFiConnectionStatus::Timeout => "Timeout".into(),
        WiFiConnectionStatus::UnsupportedAuthenticationProtocol => "UnsupportedAuthenticationProtocol".into(),
        _ => "UnspecifiedFailure".into(),
    }
}

/// The adapter's report entry for an SSID (strongest one).
fn find_net(ad: &WiFiAdapter, ssid: &str) -> Option<WiFiAvailableNetwork> {
    let list = ad.NetworkReport().ok()?.AvailableNetworks().ok()?;
    list.into_iter()
        .filter(|n| n.Ssid().is_ok_and(|s| s.to_string() == ssid))
        .max_by(|a, b| {
            let (x, y) = (a.NetworkRssiInDecibelMilliwatts().unwrap_or(-100.0), b.NetworkRssiInDecibelMilliwatts().unwrap_or(-100.0));
            x.total_cmp(&y)
        })
}

/// Blocking: refill the state (scan first if asked).
fn reload(st: &Mutex<State>, scan: bool) {
    let on = is_on();
    {
        let mut s = st.lock().unwrap();
        s.on = on;
        s.loaded = true;
        if !on {
            s.current = None;
            s.nets.clear();
            s.status = "Wi-Fi is off".into();
            return;
        }
        if s.status == "Wi-Fi is off" {
            s.status.clear();
        }
    }
    let cur = net::current();
    let wired = cur.is_none() && net::wired();
    {
        let mut s = st.lock().unwrap();
        s.current = cur.clone();
        if wired && s.status.is_empty() {
            s.status = "Not connected · using Ethernet".into();
        }
    }
    let Some(ad) = adapter() else {
        st.lock().unwrap().status = "No access to Wi-Fi. Allow location for desktop apps in Settings › Privacy › Location.".into();
        return;
    };
    if scan {
        {
            let mut s = st.lock().unwrap();
            if s.current.is_none() && s.status.is_empty() {
                s.status = "Searching…".into();
            }
        }
        let _ = ad.ScanAsync().and_then(|o| o.get());
    }
    let mut nets: Vec<(NetInfo, f64)> = Vec::new();
    if let Ok(list) = ad.NetworkReport().and_then(|r| r.AvailableNetworks()) {
        for n in list {
            let ssid = n.Ssid().map(|s| s.to_string()).unwrap_or_default();
            if ssid.is_empty() || cur.as_ref().is_some_and(|c| c.0 == ssid) {
                continue;
            }
            let rssi = n.NetworkRssiInDecibelMilliwatts().unwrap_or(-100.0);
            let secured = n
                .SecuritySettings()
                .and_then(|s| s.NetworkAuthenticationType())
                .is_ok_and(|t| t != NetworkAuthenticationType::Open80211 && t != NetworkAuthenticationType::None);
            let info = NetInfo { ssid, bars: net::bars(n.SignalBars().unwrap_or(0)), secured };
            match nets.iter_mut().find(|(x, _)| x.ssid == info.ssid) {
                Some(e) if e.1 < rssi => *e = (info, rssi),
                Some(_) => {}
                None => nets.push((info, rssi)),
            }
        }
    }
    let mut nets: Vec<NetInfo> = nets.into_iter().map(|(n, _)| n).collect();
    nets.sort_by(|a, b| b.bars.cmp(&a.bars).then_with(|| a.ssid.to_lowercase().cmp(&b.ssid.to_lowercase())));
    let mut s = st.lock().unwrap();
    if s.status == "Searching…" {
        s.status.clear();
    }
    if s.current.is_none() && nets.is_empty() && s.status.is_empty() {
        s.status = "No networks found".into();
    }
    s.nets = nets;
}

impl WifiFly {
    pub fn new() -> WifiFly {
        // show the connected network at once; the scan fills in the rest
        let st = State { on: true, current: net::current(), ..Default::default() };
        WifiFly { hwnd: 0, st: Arc::new(Mutex::new(st)), scroll: 0, password: String::new() }
    }

    fn run(&self, f: impl FnOnce(&Mutex<State>) + Send + 'static) {
        let st = self.st.clone();
        {
            let mut s = st.lock().unwrap();
            if s.busy {
                return;
            }
            s.busy = true;
        }
        spawn(self.hwnd, move || {
            f(&st);
            st.lock().unwrap().busy = false;
        });
    }

    fn join(&mut self, ssid: String, secured: bool, password: Option<String>) {
        self.st.lock().unwrap().status = format!("Joining {ssid}…");
        self.run(move |st| {
            let Some(ad) = adapter() else { return };
            let Some(n) = find_net(&ad, &ssid) else {
                st.lock().unwrap().status = format!("{ssid} is out of range.");
                return;
            };
            let r = match &password {
                Some(p) => PasswordCredential::new().and_then(|c| {
                    c.SetPassword(&HSTRING::from(p.as_str()))?;
                    ad.ConnectWithPasswordCredentialAsync(&n, WiFiReconnectionKind::Automatic, &c)?.get()
                }),
                None => ad.ConnectAsync(&n, WiFiReconnectionKind::Automatic).and_then(|o| o.get()),
            };
            let status = r.and_then(|r| r.ConnectionStatus()).unwrap_or(WiFiConnectionStatus::UnspecifiedFailure);
            if status == WiFiConnectionStatus::Success {
                {
                    let mut s = st.lock().unwrap();
                    s.expanded = None;
                    s.status.clear();
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                reload(st, false);
                return;
            }
            let mut s = st.lock().unwrap();
            if password.is_some() {
                s.status = if status == WiFiConnectionStatus::InvalidCredential { "Wrong password.".into() } else { format!("Couldn't join ({}).", status_name(status)) };
            } else if secured
                && [WiFiConnectionStatus::InvalidCredential, WiFiConnectionStatus::UnspecifiedFailure, WiFiConnectionStatus::AccessRevoked].contains(&status)
            {
                // no saved profile: ask for the password
                s.status.clear();
                s.expanded = Some(ssid.clone());
            } else {
                s.status = format!("Couldn't join {ssid} ({}).", status_name(status));
            }
        });
    }

    fn submit(&mut self) {
        let Some(ssid) = self.st.lock().unwrap().expanded.clone() else { return };
        if self.password.is_empty() {
            return;
        }
        let p = std::mem::take(&mut self.password);
        self.join(ssid, true, Some(p));
    }
}

impl Content for WifiFly {
    fn width(&self) -> f32 {
        310.0
    }
    fn padding(&self) -> (f32, f32) {
        (6.0, 9.0)
    }
    fn opened(&mut self, hwnd: isize) {
        self.hwnd = hwnd;
        self.run(|st| reload(st, true));
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_SWITCH => {
                let on = !self.st.lock().unwrap().on;
                {
                    let mut s = self.st.lock().unwrap();
                    s.on = on;
                    s.status = if on { "Turning on…".into() } else { String::new() };
                }
                self.run(move |st| {
                    set_on(on);
                    std::thread::sleep(std::time::Duration::from_millis(if on { 1500 } else { 200 }));
                    if on {
                        st.lock().unwrap().status.clear();
                    }
                    reload(st, on);
                });
            }
            ID_CURRENT => self.run(|st| {
                if let Some(ad) = adapter() {
                    let _ = ad.Disconnect();
                }
                std::thread::sleep(std::time::Duration::from_millis(800));
                reload(st, false);
            }),
            ID_SETTINGS => return Cmd::Launch("ms-settings:network-wifi".into()),
            ID_JOIN => self.submit(),
            id if id >= ID_NET => {
                let n = self.st.lock().unwrap().nets.get((id - ID_NET) as usize).cloned();
                if let Some(n) = n {
                    if self.st.lock().unwrap().expanded.as_deref() == Some(&n.ssid) {
                        return Cmd::None;
                    }
                    self.password.clear();
                    self.join(n.ssid, n.secured, None);
                }
            }
            _ => {}
        }
        Cmd::None
    }

    fn char(&mut self, c: char) -> Cmd {
        if self.st.lock().unwrap().expanded.is_none() {
            return Cmd::None;
        }
        match c {
            '\r' => self.submit(),
            '\u{8}' => {
                self.password.pop();
            }
            c if !c.is_control() => self.password.push(c),
            _ => {}
        }
        Cmd::None
    }

    fn wheel(&mut self, delta: i32) {
        let n = self.st.lock().unwrap().nets.len();
        let visible = (LIST_MAX / ROW_H) as usize;
        let max = n.saturating_sub(visible);
        self.scroll = if delta < 0 { (self.scroll + 1).min(max) } else { self.scroll.saturating_sub(1) };
    }

    fn data(&mut self) {}

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let st = self.st.lock().unwrap();
        // header
        ui.text_mid(8.0, 10.0, "Wi-Fi", 13.5, 600, white(0xF2), 0.0);
        ui.switch(ID_SWITCH, w - 4.0 - 36.0, 0.0, st.on);
        let mut y = 26.0;
        if !st.status.is_empty() {
            y += ui.text_wrap(8.0, y, &st.status, 11.5, 400, white(0x80), w - 16.0) + 4.0;
        }
        // the joined network
        if st.on {
            if let Some((ssid, bars)) = &st.current {
                ui.row(ID_CURRENT, 0.0, y, w, ROW_H + 4.0);
                net_row(ui, y + 2.0, w, ssid, *bars, true, true);
                y += ROW_H + 4.0;
            }
        }
        // other networks
        if st.on && !st.nets.is_empty() {
            y += 4.0;
            ui.text(8.0, y, "Other Networks", 11.5, 600, white(0x80), 0.0);
            y += ui.line_h(11.5, 600) + 2.0;
            let visible = (LIST_MAX / ROW_H) as usize;
            let mut shown = 0.0;
            for (i, n) in st.nets.iter().enumerate().skip(self.scroll).take(visible) {
                ui.row(ID_NET + i as u32, 0.0, y, w, ROW_H);
                net_row(ui, y, w, &n.ssid, n.bars, n.secured, false);
                y += ROW_H;
                shown += ROW_H;
                if st.expanded.as_deref() == Some(&n.ssid) {
                    // password field + Join
                    let (fx, fw) = (44.0, w - 44.0 - 8.0 - 56.0 - 6.0);
                    let fy = y + 2.0;
                    ui.hit(ID_FIELD, fx, fy, fw, 28.0);
                    ui.fill(fx, fy, fw, 28.0, 6.0, white(0x1A));
                    ui.outline(fx, fy, fw, 28.0, 6.0, 1.0, ACCENT4);
                    let dots: String = "•".repeat(self.password.chars().count());
                    let tw = if dots.is_empty() {
                        ui.text_mid(fx + 8.0, fy + 14.0, "Password", 13.0, 400, white(0x60), fw - 16.0);
                        0.0
                    } else {
                        ui.text_mid(fx + 8.0, fy + 14.0, &dots, 13.0, 400, white(0xF2), fw - 16.0)
                    };
                    ui.fill(fx + 8.0 + tw + 1.0, fy + 7.0, 1.0, 14.0, 0.0, white(0xE0)); // caret
                    let jx = w - 8.0 - 56.0;
                    let hov = ui.hit(ID_JOIN, jx, fy, 56.0, 28.0);
                    ui.fill(jx, fy, 56.0, 28.0, 6.0, if hov { (0x30, 0x98, 0xFF, 255) } else { ACCENT4 });
                    let jw = ui.text_w("Join", 13.0, 500);
                    ui.text_mid(jx + (56.0 - jw) / 2.0, fy + 14.0, "Join", 13.0, 500, white(255), 0.0);
                    y += 2.0 + 28.0 + 8.0;
                }
            }
            let _ = shown;
            if st.nets.len() > visible {
                // a thin scroll hint on the right
                let total = st.nets.len() as f32;
                let list_h = visible as f32 * ROW_H;
                let top = y - list_h;
                let th = (list_h * visible as f32 / total).max(20.0);
                let ty = top + (list_h - th) * self.scroll as f32 / (total - visible as f32);
                ui.fill(w - 3.0, ty, 2.0, th, 1.0, white(0x40));
            }
        }
        // settings
        y += 8.0;
        ui.fill(8.0, y, w - 16.0, 1.0, 0.0, white(0x1A));
        y += 1.0 + 8.0;
        ui.row(ID_SETTINGS, 0.0, y, w, 30.0);
        ui.text_mid(8.0, y + 15.0, "Wi-Fi Settings…", 13.0, 400, white(0xF2), 0.0);
        y + 30.0 - 3.0
    }
}

/// Bubble with bars, name (and "Connected"), lock on the right.
fn net_row(ui: &mut Ui, y: f32, w: f32, ssid: &str, bars: u8, secured: bool, connected: bool) {
    let cy = y + ROW_H / 2.0;
    ui.circle(8.0 + 13.0, cy, 13.0, if connected { ACCENT4 } else { white(0x26) });
    ui.wifi(8.0 + 13.0 - 7.5, cy - 6.0, 15.0, 11.0, bars, false);
    let tx = 8.0 + 26.0 + 10.0;
    let right = if secured { 24.0 } else { 8.0 };
    if connected {
        let (h1, h2) = (ui.line_h(13.0, 600), ui.line_h(11.0, 400));
        let top = cy - (h1 + h2) / 2.0;
        ui.text(tx, top, ssid, 13.0, 600, white(0xF2), w - tx - right);
        ui.text(tx, top + h1, "Connected", 11.0, 400, white(0x80), 0.0);
    } else {
        ui.text_mid(tx, cy, ssid, 13.0, 400, white(0xF2), w - tx - right);
    }
    if secured {
        ui.glyph(w - 8.0 - 6.0, cy, '\u{E72E}', 11.0, white(0x80));
    }
}
