//! The battery extra (port of BatteryPopup.cs): charge, then the power flow (adapter input, total draw and the
//! battery's share) with a CPU / GPU / rest breakdown. See power.rs for what is measured and what is estimated.

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::flyout::{white, Content, Ui, WM_FLY_DATA};
use crate::power::{PowerMeter, Sample};

const GREEN: (u8, u8, u8) = (0x34, 0xC7, 0x59);
const AMBER: (u8, u8, u8) = (0xFF, 0x9F, 0x0A);
const GREY: (u8, u8, u8) = (0x8E, 0x8E, 0x93);
const CPU: (u8, u8, u8, u8) = (0x0A, 0x84, 0xFF, 255);
const GPU: (u8, u8, u8, u8) = (0x30, 0xD1, 0x58, 255);
const REST: (u8, u8, u8, u8) = (255, 255, 255, 0x70);

/// One meter for the app's life: NVML takes ~0.6 s to open, and "rest" learning carries across openings.
static METER: Mutex<Option<PowerMeter>> = Mutex::new(None);

pub struct BatteryFly {
    hwnd: isize,
    sample: Option<Sample>,
    slot: Arc<Mutex<Option<Sample>>>,
    reading: Arc<Mutex<bool>>,
}

impl BatteryFly {
    pub fn new() -> BatteryFly {
        let recent = METER.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.recent()));
        BatteryFly { hwnd: 0, sample: recent, slot: Arc::new(Mutex::new(None)), reading: Arc::new(Mutex::new(false)) }
    }

    fn refresh(&self) {
        {
            let mut r = self.reading.lock().unwrap();
            if *r {
                return;
            }
            *r = true;
        }
        let (slot, reading, hwnd) = (self.slot.clone(), self.reading.clone(), self.hwnd);
        std::thread::spawn(move || {
            let s = {
                let mut m = METER.lock().unwrap();
                m.get_or_insert_with(PowerMeter::new).read()
            };
            *slot.lock().unwrap() = Some(s);
            *reading.lock().unwrap() = false;
            unsafe {
                let _ = PostMessageW(HWND(hwnd as *mut _), WM_FLY_DATA, WPARAM(0), LPARAM(0));
            }
        });
    }
}

fn watts(w: f64) -> String {
    if w < 9.95 {
        format!("{w:.1} W")
    } else {
        format!("{w:.0} W")
    }
}

fn duration(hours: f64) -> String {
    let total = (hours * 60.0).round() as i64;
    let (h, m) = (total / 60, total % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else {
        format!("{m} min")
    }
}

fn time_line(s: &Sample) -> String {
    let r = &s.battery;
    if !r.present {
        return String::new();
    }
    if r.charging && r.watts > 0.5 && r.full_mwh > r.capacity_mwh {
        return duration((r.full_mwh - r.capacity_mwh) as f64 / 1000.0 / r.watts) + " until full";
    }
    if !r.on_ac && r.seconds_left > 0 {
        return duration(r.seconds_left as f64 / 3600.0) + " remaining";
    }
    if r.charging {
        "Charging…".into()
    } else if r.on_ac {
        if r.percent >= 95 { "Fully charged" } else { "Holding charge" }.into()
    } else {
        "Estimating…".into()
    }
}

impl Content for BatteryFly {
    fn width(&self) -> f32 {
        330.0
    }
    fn padding(&self) -> (f32, f32) {
        (16.0, 14.0)
    }
    fn opened(&mut self, hwnd: isize) {
        self.hwnd = hwnd;
        self.refresh();
    }
    fn period_ms(&self) -> u32 {
        1000
    }
    fn tick(&mut self) {
        self.refresh();
    }
    fn data(&mut self) {
        if let Some(s) = self.slot.lock().unwrap().take() {
            self.sample = Some(s);
        }
    }
    fn closing(&mut self) {
        if let Ok(mut m) = METER.try_lock() {
            if let Some(m) = m.as_mut() {
                m.flush();
            }
        }
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let s = self.sample;
        let r = s.map(|s| s.battery);

        // header + status chip
        let (status, tint) = match r {
            None => ("", GREY),
            Some(r) if !r.present => ("No battery", (255, 255, 255)),
            Some(r) if r.charging => ("Charging", GREEN),
            Some(r) if r.on_ac => ("Plugged in", GREEN),
            Some(r) if r.percent <= 20 => ("Low", AMBER),
            _ => ("On battery", GREY),
        };
        ui.text_mid(0.0, 10.0, "Battery", 13.5, 600, white(0xF2), 0.0);
        if !status.is_empty() {
            let tw = ui.text_w(status, 11.0, 600);
            let (cw, ch) = (tw + 16.0, 19.0);
            ui.fill(w - cw, 10.0 - ch / 2.0, cw, ch, 9.0, (tint.0, tint.1, tint.2, 0x26));
            ui.text_mid(w - cw + 8.0, 9.5, status, 11.0, 600, (tint.0, tint.1, tint.2, 255), 0.0);
        }
        let mut y = 20.0 + 10.0;

        // hero: glyph, big percentage, time line
        let big_h = ui.line_h(34.0, 600);
        let time_h = ui.line_h(12.0, 400);
        let hero_h = big_h + time_h - 3.0;
        if let Some(r) = r {
            ui.battery(2.0, y + (hero_h - 22.0) / 2.0, 46.0, 22.0, r.percent as u8, r.on_ac, r.charging);
        }
        let tx = 2.0 + 46.0 + 14.0;
        let pct = r.filter(|r| r.present).map(|r| r.percent.to_string()).unwrap_or_else(|| "—".into());
        let pw = ui.text(tx, y, &pct, 34.0, 600, white(255), 0.0);
        let small_h = ui.line_h(16.0, 500);
        ui.text(tx + pw + 2.0, y + big_h - 7.0 - small_h, "%", 16.0, 500, white(0x8C), 0.0);
        if let Some(s) = s.as_ref() {
            ui.text(tx, y + big_h - 3.0, &time_line(s), 12.0, 400, white(0x99), 0.0);
        }
        y += hero_h + 14.0;

        // power flow tiles
        let tile_w = (w - 12.0) / 3.0;
        let (lab_h, val_h, sub_h) = (ui.line_h(11.0, 500), ui.line_h(19.0, 600), ui.line_h(10.5, 400));
        let tile_h = 8.0 + lab_h + 3.0 + val_h + sub_h + 9.0;
        let est = if s.is_some_and(|s| !s.total_measured) { "≈" } else { "" };
        let tiles: [(String, String, String, (u8, u8, u8, u8)); 3] = match s {
            None => std::array::from_fn(|i| {
                (["DC in", "Total draw", "From battery"][i].to_string(), "—".to_string(), String::new(), white(0xF2))
            }),
            Some(s) => {
                let b = s.battery;
                let dc = if b.on_ac || !b.present {
                    (format!("{est}{}", watts(s.dc_in_w)), "adapter".to_string())
                } else {
                    ("0 W".to_string(), "unplugged".to_string())
                };
                let batt = if b.charging {
                    ("Into battery", watts(s.flow_w), "charging", (GREEN.0, GREEN.1, GREEN.2, 255))
                } else {
                    let sub = if b.discharging { "discharging" } else if b.on_ac { "idle" } else { "—" };
                    ("From battery", watts((-s.flow_w).max(0.0)), sub, white(0xF2))
                };
                [
                    ("DC in".into(), dc.0, dc.1, white(0xF2)),
                    ("Total draw".into(), format!("{est}{}", watts(s.total_w)), if s.total_measured { "measured" } else { "estimated" }.into(), white(0xF2)),
                    (batt.0.into(), batt.1, batt.2.into(), batt.3),
                ]
            }
        };
        for (i, (label, value, sub, vc)) in tiles.iter().enumerate() {
            let x = i as f32 * (tile_w + 6.0);
            ui.fill(x, y, tile_w, tile_h, 10.0, white(0x10));
            let mut ty = y + 8.0;
            ui.text(x + 10.0, ty, label, 11.0, 500, white(0x99), tile_w - 20.0);
            ty += lab_h + 3.0;
            ui.text(x + 10.0, ty, value, 19.0, 600, *vc, tile_w - 16.0);
            ty += val_h;
            ui.text(x + 10.0, ty, sub, 10.5, 400, white(0x80), tile_w - 20.0);
        }
        y += tile_h + 14.0;

        // breakdown bar + legend
        let (cpu, gpu, rest) = match s {
            Some(s) => {
                let (c, g) = (s.cpu_w.unwrap_or(0.0), s.gpu_w.unwrap_or(0.0));
                let rest = if s.total_measured { s.total_w - c - g } else { s.rest_w }.max(0.0);
                (c, g, rest)
            }
            None => (0.0, 0.0, 0.0),
        };
        let sum = (cpu + gpu + rest).max(0.1);
        ui.fill(0.0, y, w, 6.0, 3.0, white(0x10));
        let mut x = 0.0;
        let segs = [(cpu, CPU), (gpu, GPU), (rest, REST)];
        let last = segs.iter().rposition(|(v, _)| *v > 0.0);
        for (i, (v, c)) in segs.iter().enumerate() {
            let sw = (w * (*v / sum) as f32).max(0.0);
            if sw > 0.0 {
                let gap = if Some(i) == last { 0.0 } else { 1.5 };
                // round only the bar's outer ends
                ui.fill(x, y, (sw - gap).max(1.0), 6.0, if i == 0 || Some(i) == last { 3.0 } else { 0.0 }, *c);
            }
            x += sw;
        }
        y += 6.0 + 7.0;
        let legend = match s {
            Some(s) => [
                if s.cpu_w.is_none() { "CPU —".to_string() } else { format!("CPU {cpu:.0} W") },
                if s.gpu_asleep { "GPU off".into() } else if s.gpu_w.is_none() { "GPU —".into() } else { format!("GPU {gpu:.0} W") },
                format!("Rest {}{rest:.0} W", if s.total_measured { "" } else { "~" }),
            ],
            None => ["CPU —".into(), "GPU —".into(), "Rest —".into()],
        };
        let lh = ui.line_h(10.5, 400);
        let mut lx = 0.0;
        for (i, t) in legend.iter().enumerate() {
            ui.circle(lx + 3.5, y + lh / 2.0 + 0.5, 3.5, [CPU, GPU, REST][i]);
            lx += 12.0;
            lx += ui.text(lx, y, t, 10.5, 400, white(0x80), 0.0) + 14.0;
        }
        y += lh + 12.0;
        ui.divider(y);
        y += 1.0 + 10.0;

        // capacity + note
        if let Some(s) = s {
            let b = s.battery;
            if b.full_mwh > 0 {
                let mut cap = format!("{:.1} / {:.1} Wh", b.capacity_mwh as f64 / 1000.0, b.full_mwh as f64 / 1000.0);
                if b.wear() >= 0 {
                    cap += &format!("  ·  {}% wear", b.wear());
                }
                if b.cycles > 0 {
                    cap += &format!("  ·  {} cycles", b.cycles);
                }
                ui.text(0.0, y, &cap, 11.5, 400, white(0x80), w);
                y += ui.line_h(11.5, 400);
            }
            let note = if s.total_measured {
                "Total draw measured at the battery. Rest = everything besides CPU and GPU (screen, RAM, SSD, fans)."
            } else if s.rest_learned {
                "On the adapter nothing measures the input, so it is CPU + GPU sensors plus the rest, learned while on battery."
            } else {
                "Estimated: CPU + GPU sensors plus a default for the rest. Run on battery with this open for a minute to calibrate."
            };
            y += 6.0;
            y += ui.text_wrap(0.0, y, note, 10.5, 400, white(0x66), w);
        } else {
            y += ui.text(0.0, y, "Reading sensors…", 11.5, 400, white(0x80), 0.0).min(0.0) + ui.line_h(11.5, 400);
        }
        y
    }
}
