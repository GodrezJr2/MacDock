//! Battery state and where the laptop's power goes (port of Battery.cs + PowerMeter.cs).
//!
//! The battery driver only measures the battery side: on the battery its discharge rate is the whole system's
//! draw, but on the adapter nothing measures the input. So on AC the total is estimated as CPU package (Windows'
//! energy meter, RAPL) + GPU board power (NVML) + "the rest" (screen, RAM, SSD, fans), with the rest learned
//! while on the battery and saved for next time.

use std::ffi::c_void;
use std::time::Instant;

use windows::core::{GUID, PCSTR, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::IO::DeviceIoControl;

#[derive(Clone, Copy, Default)]
pub struct Reading {
    pub present: bool,
    pub percent: i32,
    pub on_ac: bool,
    pub charging: bool,
    pub discharging: bool,
    /// + into the battery, - out of it
    pub watts: f64,
    pub capacity_mwh: i32,
    pub full_mwh: i32,
    pub design_mwh: i32,
    pub cycles: i32,
    pub seconds_left: i32,
}

impl Reading {
    pub fn draw_w(&self) -> Option<f64> {
        self.discharging.then_some(-self.watts)
    }
    pub fn wear(&self) -> i32 {
        if self.full_mwh > 0 && self.design_mwh > 0 {
            (100.0 * (self.design_mwh - self.full_mwh) as f64 / self.design_mwh as f64).round() as i32
        } else {
            -1
        }
    }
}

const BATTERY_GUID: GUID = GUID::from_u128(0x72631e54_78a4_11d0_bcf7_00aa00b7b32a);
const EMI_GUID: GUID = GUID::from_u128(0x45BD8344_7ED6_49cf_A440_C276C933B053);
const IOCTL_QUERY_TAG: u32 = 0x294040;
const IOCTL_QUERY_INFO: u32 = 0x294044;
const IOCTL_QUERY_STATUS: u32 = 0x29404C;

/// Device interface paths of a class.
fn device_paths(guid: &GUID) -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(Some(guid), PCWSTR::null(), HWND::default(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) else {
            return out;
        };
        for i in 0.. {
            let mut did = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInterfaces(set, None, guid, i, &mut did).is_err() {
                break;
            }
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &did, None, 0, Some(&mut needed), None);
            if needed < 8 {
                continue;
            }
            let mut buf = vec![0u8; needed as usize + 2];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, Some(detail), needed, None, None).is_ok() {
                let p = buf.as_ptr().add(4) as *const u16;
                let mut v = Vec::new();
                let mut k = 0;
                while *p.add(k) != 0 {
                    v.push(*p.add(k));
                    k += 1;
                }
                v.push(0);
                out.push(v);
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn open(path: &[u16], access: u32) -> Option<HANDLE> {
    unsafe {
        CreateFileW(PCWSTR(path.as_ptr()), access, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), HANDLE::default()).ok()
    }
}

fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> bool {
    unsafe {
        let mut n = 0u32;
        DeviceIoControl(
            h, code,
            if input.is_empty() { None } else { Some(input.as_ptr() as *const c_void) }, input.len() as u32,
            Some(output.as_mut_ptr() as *mut c_void), output.len() as u32, Some(&mut n), None,
        )
        .is_ok()
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

struct BatteryDev {
    path: Option<Vec<u16>>,
}

impl BatteryDev {
    /// (state, rate mW, capacity mWh, full mWh, design mWh, cycles)
    fn read(&mut self) -> Option<(u32, i32, i32, i32, i32, i32)> {
        if self.path.is_none() {
            self.path = device_paths(&BATTERY_GUID).into_iter().next();
        }
        let path = self.path.as_ref()?;
        let Some(h) = open(path, 0xC000_0000) else {
            self.path = None; // device went away; look again next time
            return None;
        };
        let r = (|| {
            let mut tag = [0u8; 4];
            if !ioctl(h, IOCTL_QUERY_TAG, &0u32.to_le_bytes(), &mut tag) {
                return None;
            }
            // BATTERY_QUERY_INFORMATION { Tag, InformationLevel = 0 (BatteryInformation), AtRate }
            let mut qi = [0u8; 12];
            qi[..4].copy_from_slice(&tag);
            let mut info = [0u8; 36];
            if !ioctl(h, IOCTL_QUERY_INFO, &qi, &mut info) {
                return None;
            }
            // BATTERY_INFORMATION: Capabilities, Technology + 3 reserved, Chemistry[4], Designed, FullCharged, ..., CycleCount
            let (design, full, cycles) = (u32_at(&info, 12) as i32, u32_at(&info, 16) as i32, u32_at(&info, 32) as i32);
            let mut ws = [0u8; 20];
            ws[..4].copy_from_slice(&tag);
            let mut st = [0u8; 16];
            if !ioctl(h, IOCTL_QUERY_STATUS, &ws, &mut st) {
                return None;
            }
            Some((u32_at(&st, 0), u32_at(&st, 12) as i32, u32_at(&st, 4) as i32, full, design, cycles))
        })();
        unsafe {
            let _ = CloseHandle(h);
        }
        r
    }
}

fn read_battery(dev: &mut BatteryDev) -> Reading {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe {
        let _ = GetSystemPowerStatus(&mut ps);
    }
    if ps.BatteryFlag == 128 || ps.BatteryFlag == 255 {
        return Reading { present: false, percent: 100, on_ac: true, seconds_left: -1, ..Default::default() };
    }
    let percent = (ps.BatteryLifePercent as i32).clamp(0, 100);
    let on_ac = ps.ACLineStatus == 1;
    let (mut state, mut rate, mut cap, mut full, mut design, mut cycles) = (0, 0, 0, 0, 0, 0);
    let device = match dev.read() {
        Some(t) => {
            (state, rate, cap, full, design, cycles) = t;
            true
        }
        None => false,
    };
    let mut charging = state & 4 != 0;
    let mut discharging = state & 2 != 0;
    if !charging && !discharging && rate != 0 {
        charging = on_ac;
        discharging = !on_ac;
    }
    // the EC signs the rate inconsistently; the state flags are the reliable part
    let w = (rate as f64 / 1000.0).abs();
    let watts = if charging { w } else if discharging { -w } else { 0.0 };
    if !device {
        (full, design, cap) = (0, 0, 0);
    } else if full > 0 && percent > 0 {
        cap = (full as f64 * percent as f64 / 100.0).round() as i32;
    }
    let secs = ps.BatteryLifeTime as i32;
    Reading {
        present: true, percent, on_ac, charging, discharging, watts, capacity_mwh: cap, full_mwh: full, design_mwh: design, cycles,
        seconds_left: if secs > 0 { secs } else { -1 },
    }
}

/// Windows' Energy Meter Interface (EMI v2) on the CPU's PPM device: RAPL counters readable without admin.
struct EnergyMeter {
    h: HANDLE,
    channels: usize,
    pkg: usize,
    last: (u64, u64),
}

impl EnergyMeter {
    fn open() -> Option<EnergyMeter> {
        for path in device_paths(&EMI_GUID) {
            let Some(h) = open(&path, 0x8000_0000) else { continue };
            let mut b = [0u8; 4];
            let ok = ioctl(h, 0x224000, &[], &mut b) && u16::from_le_bytes([b[0], b[1]]) >= 2 && ioctl(h, 0x224004, &[], &mut b);
            if ok {
                let mut md = vec![0u8; u32::from_le_bytes(b) as usize];
                if md.len() >= 68 && ioctl(h, 0x224008, &[], &mut md) {
                    // EMI_METADATA_V2: OEM[16 wchar], Model[16 wchar], Revision u16, ChannelCount u16, then
                    // EMI_CHANNEL_V2 { Unit u32, NameSize u16, Name[] }
                    let count = u16::from_le_bytes([md[66], md[67]]) as usize;
                    let (mut off, mut pkg) = (68usize, None);
                    for c in 0..count {
                        if off + 6 > md.len() {
                            break;
                        }
                        let size = u16::from_le_bytes([md[off + 4], md[off + 5]]) as usize;
                        let end = (off + 6 + size).min(md.len());
                        let name: Vec<u16> = md[off + 6..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        let name = String::from_utf16_lossy(&name).trim_end_matches('\0').to_uppercase();
                        if pkg.is_none() && name.ends_with("_PKG") {
                            pkg = Some(c);
                        }
                        off += 6 + size;
                    }
                    if let Some(pkg) = pkg {
                        return Some(EnergyMeter { h, channels: count, pkg, last: (0, 0) });
                    }
                }
            }
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        None
    }

    fn watts(&mut self) -> Option<f64> {
        let mut m = vec![0u8; 16 * self.channels];
        if !ioctl(self.h, 0x22400C, &[], &mut m) {
            return None;
        }
        let o = self.pkg * 16;
        let e = u64::from_le_bytes(m[o..o + 8].try_into().unwrap());
        let t = u64::from_le_bytes(m[o + 8..o + 16].try_into().unwrap());
        let (pe, pt) = self.last;
        self.last = (e, t);
        if pt == 0 || t <= pt || e < pe {
            return None;
        }
        Some((e - pe) as f64 * 3600.0 / 1e12 / ((t - pt) as f64 / 1e7))
    }
}

impl Drop for EnergyMeter {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.h);
        }
    }
}

/// NVIDIA's management library (ships with the driver): board power of GPU 0.
struct Nvml {
    dev: *mut c_void,
    power: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32,
}

unsafe impl Send for Nvml {}

impl Nvml {
    fn open() -> Option<Nvml> {
        unsafe {
            let lib: HMODULE = LoadLibraryW(windows::core::w!("nvml.dll"))
                .or_else(|_| LoadLibraryW(windows::core::w!("C:\\Windows\\System32\\nvml.dll")))
                .ok()?;
            let init = GetProcAddress(lib, PCSTR(b"nvmlInit_v2\0".as_ptr()))?;
            let handle = GetProcAddress(lib, PCSTR(b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr()))?;
            let power = GetProcAddress(lib, PCSTR(b"nvmlDeviceGetPowerUsage\0".as_ptr()))?;
            let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(init);
            let handle: unsafe extern "C" fn(u32, *mut *mut c_void) -> i32 = std::mem::transmute(handle);
            if init() != 0 {
                return None;
            }
            let mut dev = std::ptr::null_mut();
            if handle(0, &mut dev) != 0 {
                return None;
            }
            Some(Nvml { dev, power: std::mem::transmute(power) })
        }
    }

    fn watts(&self) -> Option<f64> {
        let mut mw = 0u32;
        (unsafe { (self.power)(self.dev, &mut mw) } == 0).then_some(mw as f64 / 1000.0)
    }
}

#[derive(Clone, Copy)]
pub struct Sample {
    pub battery: Reading,
    pub cpu_w: Option<f64>,
    pub gpu_w: Option<f64>,
    pub gpu_asleep: bool,
    pub rest_w: f64,
    pub rest_learned: bool,
    pub total_w: f64,
    pub total_measured: bool,
    pub dc_in_w: f64,
    pub flow_w: f64,
}

const DEFAULT_REST_W: f64 = 14.0;
const GPU_SLOW_MS: u128 = 150;

pub struct PowerMeter {
    dev: BatteryDev,
    cpu: Option<EnergyMeter>,
    gpu: Option<Nvml>,
    opened: bool,
    next_gpu: Option<Instant>,
    gpu_asleep: bool,
    rest_w: f64,
    rest_learned: bool,
    learned: u32,
    cpu_avg: Option<f64>,
    gpu_avg: Option<f64>,
    pub last: Option<(Sample, Instant)>,
}

unsafe impl Send for PowerMeter {}

fn store_path() -> String {
    format!("{}\\MacDock\\power.json", std::env::var("LOCALAPPDATA").unwrap_or_default())
}

fn smooth(avg: Option<f64>, now: Option<f64>) -> Option<f64> {
    match (avg, now) {
        (_, None) => avg,
        (Some(a), Some(n)) => Some(a + (n - a) * 0.35),
        (None, n) => n,
    }
}

impl PowerMeter {
    pub fn new() -> PowerMeter {
        let saved = std::fs::read_to_string(store_path())
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("RestW").and_then(|x| x.as_f64()));
        PowerMeter {
            dev: BatteryDev { path: None }, cpu: None, gpu: None, opened: false, next_gpu: None, gpu_asleep: false,
            rest_w: saved.unwrap_or(DEFAULT_REST_W), rest_learned: saved.is_some(), learned: 0, cpu_avg: None, gpu_avg: None, last: None,
        }
    }

    /// A sample under 5 s old, so a reopened flyout shows numbers at once.
    pub fn recent(&self) -> Option<Sample> {
        self.last.filter(|(_, t)| t.elapsed().as_millis() < 5000).map(|(s, _)| s)
    }

    /// Blocking (battery IOCTLs, NVML wake-up): call it off the UI thread.
    pub fn read(&mut self) -> Sample {
        if !self.opened {
            self.opened = true;
            self.cpu = EnergyMeter::open();
            self.gpu = Nvml::open();
        }
        let b = read_battery(&mut self.dev);
        let cpu_now = self.cpu.as_mut().and_then(|c| c.watts());
        self.cpu_avg = smooth(self.cpu_avg, cpu_now);
        self.read_gpu();

        let parts = self.cpu_avg.unwrap_or(0.0) + self.gpu_avg.unwrap_or(0.0);
        let measured = b.present && !b.on_ac && b.discharging;
        match (measured, self.cpu_avg, b.draw_w()) {
            (true, Some(_), Some(draw)) => {
                let rest = (draw - parts).clamp(2.0, 60.0);
                self.learned += 1;
                // the first seconds after unplugging still carry AC-era averages; let them settle first
                if self.learned > 5 {
                    self.rest_w = if self.rest_learned { self.rest_w + (rest - self.rest_w) * 0.05 } else { rest };
                    self.rest_learned = true;
                    if self.learned % 30 == 0 {
                        self.save();
                    }
                }
            }
            _ => self.learned = 0,
        }
        let total = match (measured, b.draw_w()) {
            (true, Some(d)) => d,
            _ => parts + self.rest_w,
        };
        let flow = if b.present { b.watts } else { 0.0 };
        let dc_in = if b.on_ac || !b.present { (total + flow).max(0.0) } else { 0.0 };
        let s = Sample {
            battery: b, cpu_w: self.cpu_avg, gpu_w: self.gpu_avg, gpu_asleep: self.gpu_asleep, rest_w: self.rest_w,
            rest_learned: self.rest_learned, total_w: total, total_measured: measured, dc_in_w: dc_in, flow_w: flow,
        };
        self.last = Some((s, Instant::now()));
        s
    }

    fn read_gpu(&mut self) {
        let Some(g) = self.gpu.as_ref() else { return };
        if self.next_gpu.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let t0 = Instant::now();
        let w = g.watts();
        let slept = t0.elapsed().as_millis() > GPU_SLOW_MS;
        if slept {
            // a slow query means the Optimus dGPU was asleep: the value is the wake-up spike, not its draw
            self.gpu_asleep = true;
            self.gpu_avg = Some(0.0);
        } else {
            self.gpu_asleep = false;
            self.gpu_avg = smooth(self.gpu_avg, w);
        }
        self.next_gpu = Some(Instant::now() + std::time::Duration::from_millis(if slept { 30000 } else { 1000 }));
    }

    fn save(&self) {
        let p = store_path();
        if let Some(d) = std::path::Path::new(&p).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, format!("{{\"RestW\":{}}}", (self.rest_w * 100.0).round() / 100.0));
    }

    /// Flyout closed: keep what was learned.
    pub fn flush(&mut self) {
        if self.learned > 5 {
            self.save();
        }
    }
}
