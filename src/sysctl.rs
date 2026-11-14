//! System controls behind Control Center (port of SystemControls.cs): master volume, panel brightness,
//! Bluetooth radio, dark mode and Windows' power mode. The slow ones (WMI, radios) are called from workers.

use windows::core::{w, BSTR, GUID, PCSTR, PCWSTR, VARIANT};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoSetProxyBlanket, CLSCTX_ALL, CLSCTX_INPROC_SERVER, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};

// ---- volume (Core Audio)

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

/// 0..1 (0 when muted), or None without an output device.
pub fn volume() -> Option<f32> {
    let ep = endpoint()?;
    unsafe {
        let v = ep.GetMasterVolumeLevelScalar().ok()?;
        let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        Some(if muted { 0.0 } else { v })
    }
}

pub fn set_volume(v: f32) {
    if let Some(ep) = endpoint() {
        unsafe {
            let _ = ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
            let _ = ep.SetMute(v <= 0.001, std::ptr::null());
        }
    }
}

// ---- brightness (WMI root\wmi; the built-in panel only)

fn wmi() -> Option<IWbemServices> {
    unsafe {
        let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        let svc = loc.ConnectServer(&BSTR::from("root\\wmi"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None).ok()?;
        let _ = CoSetProxyBlanket(&svc, 10, 0, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE);
        Some(svc)
    }
}

fn first(svc: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
    unsafe {
        let en = svc
            .ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)
            .ok()?;
        let mut objs = [None];
        let mut n = 0u32;
        let _ = en.Next(WBEM_INFINITE, &mut objs, &mut n);
        objs[0].take()
    }
}

/// 0-100, or None (no WMI brightness: desktop monitor).
pub fn brightness() -> Option<u8> {
    let svc = wmi()?;
    let o = first(&svc, "SELECT CurrentBrightness FROM WmiMonitorBrightness")?;
    unsafe {
        let mut v = VARIANT::default();
        o.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
        i32::try_from(&v).ok().or_else(|| u32::try_from(&v).ok().map(|x| x as i32)).map(|x| x.clamp(0, 100) as u8)
    }
}

/// Sets the built-in panel through the video port's \\.\LCD device. WMI's WmiSetBrightness answers
/// WBEM_E_ACCESS_DENIED from a plain (non-.NET) COM client even with an impersonate blanket, so it is
/// only the fallback.
fn set_brightness_ioctl(pct: u8) -> bool {
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::IO::DeviceIoControl;
    const IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS: u32 = 0x0023_049C;
    // DISPLAY_BRIGHTNESS: ucDisplayPolicy (AC | DC), ucACBrightness, ucDCBrightness
    let p = pct.min(100);
    let buf = [3u8, p, p];
    unsafe {
        let Ok(h) = CreateFileW(
            w!("\\\\.\\LCD"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return false;
        };
        let ok = DeviceIoControl(h, IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS, Some(buf.as_ptr().cast()), buf.len() as u32, None, 0, None, None).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

pub fn set_brightness(pct: u8) {
    if set_brightness_ioctl(pct) {
        return;
    }
    let run = || -> Option<()> {
        let svc = wmi()?;
        let inst = first(&svc, "SELECT * FROM WmiMonitorBrightnessMethods")?;
        unsafe {
            let mut path = VARIANT::default();
            inst.Get(w!("__PATH"), 0, &mut path, None, None).ok()?;
            let path = BSTR::try_from(&path).ok()?;
            let mut class = None;
            svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None).ok()?;
            let class = class?;
            let mut sig = None;
            class.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, std::ptr::null_mut()).ok()?;
            let params = sig?.SpawnInstance(0).ok()?;
            // WMI carries CIM uint32 as VT_I4; a VT_UI4 fails Put with WBEM_E_TYPE_MISMATCH
            params.Put(w!("Timeout"), 0, &VARIANT::from(1i32), 0).ok()?;
            params.Put(w!("Brightness"), 0, &VARIANT::from(pct.min(100)), 0).ok()?;
            svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None).ok()?;
        }
        Some(())
    };
    let _ = run();
}

// ---- Bluetooth radio

/// Blocking. None = no Bluetooth radio.
pub fn bluetooth() -> Option<Radio> {
    let _ = Radio::RequestAccessAsync().and_then(|o| o.get());
    Radio::GetRadiosAsync()
        .and_then(|o| o.get())
        .ok()?
        .into_iter()
        .find(|r| r.Kind().is_ok_and(|k| k == RadioKind::Bluetooth))
}

pub fn set_radio(r: &Radio, on: bool) {
    let _ = r.SetStateAsync(if on { RadioState::On } else { RadioState::Off }).and_then(|o| o.get());
}

// ---- dark mode

const THEME_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

pub fn dark() -> bool {
    let mut v = 1u32;
    let mut n = 4u32;
    unsafe {
        let r = RegGetValueW(HKEY_CURRENT_USER, THEME_KEY, w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut n));
        r.is_ok() && v == 0
    }
}

pub fn set_dark(dark: bool) {
    let v: u32 = if dark { 0 } else { 1 };
    let bytes = v.to_le_bytes();
    unsafe {
        let mut k = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, THEME_KEY, 0, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut k, None).is_ok() {
            let _ = RegSetValueExW(k, w!("AppsUseLightTheme"), 0, REG_DWORD, Some(&bytes));
            let _ = RegSetValueExW(k, w!("SystemUsesLightTheme"), 0, REG_DWORD, Some(&bytes));
            let _ = RegCloseKey(k);
        }
        // tell running apps (Explorer, Settings, WinUI apps) to re-read it
        let s = crate::shell::wide("ImmersiveColorSet");
        let _ = SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, WPARAM(0), LPARAM(s.as_ptr() as isize), SMTO_ABORTIFHUNG, 200, None);
    }
}

// ---- power mode (Settings › Power: efficiency / balanced / performance overlay)

pub const POWER_MODES: [(&str, GUID); 3] = [
    ("Efficiency", GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a)),
    ("Balanced", GUID::zeroed()),
    ("Performance", GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238)),
];

type GetOverlay = unsafe extern "system" fn(*mut GUID) -> u32;
type SetOverlay = unsafe extern "system" fn(*const GUID) -> u32;

fn powrprof(name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe {
        let lib = LoadLibraryW(w!("powrprof.dll")).ok()?;
        GetProcAddress(lib, PCSTR(name.as_ptr()))
    }
}

fn overlay(name: &[u8]) -> Option<usize> {
    let f: GetOverlay = unsafe { std::mem::transmute(powrprof(name)?) };
    let mut g = GUID::zeroed();
    if unsafe { f(&mut g) } != 0 {
        return None;
    }
    POWER_MODES.iter().position(|m| m.1 == g)
}

/// The mode the user picked (Settings › Power mode). Not the effective one: Energy Saver forces the effective
/// overlay to Efficiency, which made the tile show Efficiency and a click look like it did nothing.
pub fn power_mode() -> usize {
    overlay(b"PowerGetActualOverlayScheme\0").unwrap_or(1)
}

/// Energy Saver (or battery saver) is holding the system at Efficiency over the user's pick.
pub fn power_saver() -> bool {
    overlay(b"PowerGetEffectiveOverlayScheme\0") == Some(0) && power_mode() != 0
}

pub fn set_power_mode(i: usize) {
    let Some(f) = powrprof(b"PowerSetActiveOverlayScheme\0") else { return };
    // the GUID goes by value; on x64 a 16-byte struct is passed by pointer, same as *const GUID
    let f: SetOverlay = unsafe { std::mem::transmute(f) };
    let g = POWER_MODES[i.min(2)].1;
    unsafe {
        f(&g);
    }
}
