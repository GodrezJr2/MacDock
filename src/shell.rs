//! Shell glue: app windows, process identity, display names, icons, launching and activation.
//! A port of Shell.cs, same rules for which windows count as apps.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;

use tiny_skia::{IntSize, Pixmap};
use windows::core::{Interface, BSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, SIZE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP};
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_Link_TargetParsingPath};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, AttachThreadInput,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, SHGetPropertyStoreForWindow, SHGetPropertyStoreFromParsingName, GPS_DEFAULT, PROPERTYKEY,
};
use windows::Win32::UI::Shell::{
    IExtractIconW, IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, ShellExecuteW, BHID_SFUIObject,
    SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn eq_ic(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ------------------------------------------------------------------ property stores

fn read_string(ps: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let pv = ps.GetValue(key).ok()?;
        let s = BSTR::try_from(&pv).ok()?.to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn store_string(parsing: &str, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let w = wide(parsing);
        let ps: IPropertyStore = SHGetPropertyStoreFromParsingName(PCWSTR(w.as_ptr()), None, GPS_DEFAULT).ok()?;
        read_string(&ps, key)
    }
}

pub fn link_target(parsing: &str) -> Option<String> {
    store_string(parsing, &PKEY_Link_TargetParsingPath)
}
pub fn item_aumid(parsing: &str) -> Option<String> {
    store_string(parsing, &PKEY_AppUserModel_ID)
}

pub fn window_aumid(h: HWND) -> Option<String> {
    unsafe {
        let ps: IPropertyStore = SHGetPropertyStoreForWindow(h).ok()?;
        read_string(&ps, &PKEY_AppUserModel_ID)
    }
}

pub fn display_name(parsing: &str) -> Option<String> {
    unsafe {
        let w = wide(parsing);
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let p = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.filter(|s| !s.trim().is_empty())
    }
}

// ------------------------------------------------------------------ icons

/// The shell's own icon for anything parseable (shell:AppsFolder\…, .lnk, .exe), as premultiplied BGRA.
/// Shrunk to `max_px` so the cache holds what is drawn, not 256 px originals.
pub fn icon(parsing: &str, max_px: u32) -> Option<Pixmap> {
    unsafe {
        let w = wide(parsing);
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        // Packaged apps: the factory's ICONONLY path hands back a generic icon in this process, so read the
        // package's own logo PNG (the best size/scale variant) straight from its folder.
        if let Some(pm) = package_logo(&item, max_px) {
            return Some(pm);
        }
        let f: IShellItemImageFactory = item.cast().ok()?;
        let ask = if max_px > 128 { 256 } else { 128 };
        let hbm = f.GetImage(SIZE { cx: ask, cy: ask }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let pm = hbitmap_pixels(hbm);
        let _ = DeleteObject(hbm);
        let pm = pm?;
        if pm.width() <= max_px {
            return Some(pm);
        }
        let mut small = Pixmap::new(max_px, max_px)?;
        let s = max_px as f32 / pm.width() as f32;
        small.draw_pixmap(
            0,
            0,
            pm.as_ref(),
            &tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() },
            tiny_skia::Transform::from_scale(s, s),
            None,
        );
        Some(small)
    }
}

/// For packaged apps the shell's icon location is a logo PNG, e.g. `...\Images\Square44x44Logo.png-100`.
unsafe fn package_logo(item: &IShellItem, max_px: u32) -> Option<Pixmap> {
    let x: IExtractIconW = item.BindToHandler(None, &BHID_SFUIObject).ok()?;
    let mut buf = [0u16; 1024];
    let (mut idx, mut flags) = (0i32, 0u32);
    x.GetIconLocation(0, &mut buf, &mut idx, &mut flags).ok()?;
    let loc = from_wide(&buf);
    let cut = loc.to_ascii_lowercase().find(".png")?;
    best_logo(&loc[..cut + 4], max_px).and_then(|p| crate::gfx::image_file(&p, max_px))
}

/// Pick the best MRT variant of `stem.png`: transparent ("unplated") target sizes first, then scales.
/// Prefers the smallest image at least `want` px wide, else the largest one there is.
fn best_logo(png: &str, want: u32) -> Option<String> {
    let path = std::path::Path::new(png);
    let dir = path.parent()?;
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    // nominal size from names like Square44x44Logo; plain logos are taken as 44
    let base: u32 = stem
        .find('x')
        .and_then(|i| {
            let d: String = stem[..i].chars().rev().take_while(|c| c.is_ascii_digit()).collect();
            d.chars().rev().collect::<String>().parse().ok()
        })
        .unwrap_or(44);
    let mut best: Option<(bool, bool, u32, String)> = None; // (big enough, unplated, size, path)
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let n = name.to_ascii_lowercase();
        if !n.ends_with(".png") || !(n.starts_with(&format!("{stem}.")) ) || n.contains("contrast") || n.contains("lightunplated") {
            continue;
        }
        let quals = n.get(stem.len() + 1..n.len() - 4).unwrap_or("");
        let num = |key: &str| -> Option<u32> {
            let i = quals.find(key)? + key.len();
            quals[i..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
        };
        let size = num("targetsize-").or_else(|| num("scale-").map(|s| base * s / 100)).unwrap_or(base);
        let unplated = quals.contains("unplated");
        let cand = (size >= want, unplated, size, e.path().to_string_lossy().to_string());
        let better = match &best {
            None => true,
            Some(b) => match (cand.0, b.0) {
                (true, false) => true,
                (false, true) => false,
                (true, true) => (cand.2, !cand.1) < (b.2, !b.1),
                (false, false) => (cand.2, cand.1) > (b.2, b.1),
            },
        };
        if better {
            best = Some(cand);
        }
    }
    best.map(|b| b.3)
}

/// The image factory hands back a premultiplied 32 bpp DIB, usually bottom-up.
unsafe fn hbitmap_pixels(hbm: HBITMAP) -> Option<Pixmap> {
    let mut bm = BITMAP::default();
    if GetObjectW(hbm, size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0
        || bm.bmBits.is_null()
        || bm.bmBitsPixel != 32
    {
        return None;
    }
    let (w, h, stride) = (bm.bmWidth as usize, bm.bmHeight.unsigned_abs() as usize, bm.bmWidthBytes as usize);
    let mut v = vec![0u8; w * h * 4];
    let src = bm.bmBits as *const u8;
    for y in 0..h {
        let sy = if bm.bmHeight > 0 { h - 1 - y } else { y };
        std::ptr::copy_nonoverlapping(src.add(sy * stride), v.as_mut_ptr().add(y * w * 4), w * 4);
    }
    // some icons come back with colour above alpha; clamp so the premultiplied invariant holds
    for p in v.chunks_exact_mut(4) {
        let a = p[3];
        p[0] = p[0].min(a);
        p[1] = p[1].min(a);
        p[2] = p[2].min(a);
    }
    Pixmap::from_vec(v, IntSize::from_wh(w as u32, h as u32)?)
}

// ------------------------------------------------------------------ windows

#[derive(Clone)]
pub struct AppWindow {
    pub hwnd: HWND,
    pub pid: u32,
    pub exe: Option<String>,
    pub aumid: Option<String>,
    pub title: String,
}

#[derive(Default)]
pub struct Shell {
    procs: HashMap<u32, (Option<String>, Option<String>)>,
    win_aumids: HashMap<isize, Option<String>>,
}

impl Shell {
    pub fn process_info(&mut self, pid: u32) -> (Option<String>, Option<String>) {
        if let Some(c) = self.procs.get(&pid) {
            return c.clone();
        }
        let mut r = (None, None);
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                let mut buf = [0u16; 1024];
                let mut n = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok() {
                    r.0 = Some(from_wide(&buf[..n as usize]));
                }
                let mut ab = [0u16; 512];
                let mut len = ab.len() as u32;
                if GetApplicationUserModelId(h, &mut len, PWSTR(ab.as_mut_ptr())).is_ok() {
                    r.1 = Some(from_wide(&ab));
                }
                let _ = CloseHandle(h);
            }
        }
        self.procs.insert(pid, r.clone());
        r
    }

    /// Top-level windows that would get a taskbar button, in z-order (front first).
    pub fn app_windows(&mut self) -> Vec<AppWindow> {
        if self.win_aumids.len() > 400 {
            self.win_aumids.clear();
        }
        if self.procs.len() > 400 {
            self.procs.clear();
        }
        let mut all: Vec<HWND> = Vec::new();
        unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> windows::Win32::Foundation::BOOL {
            (*(l.0 as *mut Vec<HWND>)).push(h);
            true.into()
        }
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
        }
        let me = unsafe { GetCurrentProcessId() };
        let mut out = Vec::new();
        let mut cls = [0u16; 256];
        for h in all {
            unsafe {
                if !IsWindowVisible(h).as_bool() {
                    continue;
                }
                let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
                let app_win = ex & WS_EX_APPWINDOW.0 != 0;
                let owned = GetWindow(h, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false);
                if !app_win && (ex & WS_EX_TOOLWINDOW.0 != 0 || owned) {
                    continue;
                }
                let mut cloaked = 0u32;
                if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4).is_ok() && cloaked != 0 {
                    continue;
                }
                let mut tb = [0u16; 512];
                let tn = GetWindowTextW(h, &mut tb);
                if tn <= 0 {
                    continue;
                }
                let title = String::from_utf16_lossy(&tb[..tn as usize]);
                let n = GetClassNameW(h, &mut cls) as usize;
                let class = String::from_utf16_lossy(&cls[..n]);
                if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Windows.UI.Core.CoreWindow") {
                    continue;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                if pid == me {
                    continue;
                }
                let (mut exe, paumid) = self.process_info(pid);
                let frame = class == "ApplicationFrameWindow";
                let waumid = match self.win_aumids.get(&(h.0 as isize)) {
                    Some(a) => a.clone(),
                    None => {
                        let a = window_aumid(h);
                        // UWP frames get their AUMID a moment after they appear: don't remember a miss for them
                        if a.is_some() || !frame {
                            self.win_aumids.insert(h.0 as isize, a.clone());
                        }
                        a
                    }
                };
                let mut aumid = waumid.or(paumid);
                if frame {
                    // the real app lives in a child CoreWindow owned by another process
                    let mut inner = (pid, 0u32);
                    unsafe extern "system" fn child(c: HWND, l: LPARAM) -> windows::Win32::Foundation::BOOL {
                        let st = &mut *(l.0 as *mut (u32, u32));
                        let mut cp = 0u32;
                        GetWindowThreadProcessId(c, Some(&mut cp));
                        if cp != st.0 {
                            st.1 = cp;
                            return false.into();
                        }
                        true.into()
                    }
                    let _ = EnumChildWindows(h, Some(child), LPARAM(&mut inner as *mut _ as isize));
                    if inner.1 != 0 {
                        let ip = self.process_info(inner.1);
                        exe = ip.0;
                        aumid = aumid.or(ip.1);
                    }
                    if aumid.is_none() {
                        continue;
                    }
                }
                out.push(AppWindow { hwnd: h, pid, exe, aumid, title });
            }
        }
        out
    }
}

pub fn friendly_name(exe: Option<&str>, aumid: Option<&str>, fallback: &str) -> String {
    if let Some(a) = aumid {
        if let Some(n) = display_name(&format!("shell:AppsFolder\\{a}")) {
            return n;
        }
    }
    if let Some(e) = exe {
        if let Some(n) = display_name(e) {
            return n.trim_end_matches(".exe").to_string();
        }
        return std::path::Path::new(e).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    fallback.to_string()
}

// ------------------------------------------------------------------ actions

pub fn launch(target: &str) {
    unsafe {
        let open = wide("open");
        if target.len() > 6 && eq_ic(&target[..6], "shell:") {
            let (exe, arg) = (wide("explorer.exe"), wide(target));
            ShellExecuteW(HWND::default(), PCWSTR(open.as_ptr()), PCWSTR(exe.as_ptr()), PCWSTR(arg.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL);
        } else {
            let t = wide(target);
            ShellExecuteW(HWND::default(), PCWSTR(open.as_ptr()), PCWSTR(t.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
}

pub fn minimize(h: HWND) {
    unsafe {
        let _ = PostMessageW(h, WM_SYSCOMMAND, windows::Win32::Foundation::WPARAM(SC_MINIMIZE as usize), LPARAM(0));
    }
}

pub fn activate(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if SetForegroundWindow(h).as_bool() {
            let _ = BringWindowToTop(h);
            return;
        }
        // an empty input makes us the last input again, which is what SetForegroundWindow checks
        SendInput(&[INPUT::default()], size_of::<INPUT>() as i32);
        if SetForegroundWindow(h).as_bool() {
            let _ = BringWindowToTop(h);
            return;
        }
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let _ = AttachThreadInput(me, fg_thread, true);
        let _ = SetForegroundWindow(h);
        let _ = BringWindowToTop(h);
        let _ = AttachThreadInput(me, fg_thread, false);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn packaged_icons() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        for (i, spec) in [
            r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
            r"shell:AppsFolder\Microsoft.OutlookForWindows_8wekyb3d8bbwe!Microsoft.OutlookforWindows",
        ]
        .iter()
        .enumerate()
        {
            let pm = super::icon(spec, 128);
            match pm {
                Some(p) => {
                    let mut c = p.clone();
                    for px in c.data_mut().chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                    std::fs::write(format!("target/icon{i}_{}x{}.rgba", c.width(), c.height()), c.data()).unwrap();
                    println!("{spec}: {}x{}", p.width(), p.height());
                }
                None => println!("{spec}: none"),
            }
        }
    }
}

pub fn close(h: HWND) {
    unsafe {
        let _ = PostMessageW(h, WM_CLOSE, windows::Win32::Foundation::WPARAM(0), LPARAM(0));
    }
}

/// Open Explorer with the file selected.
pub fn show_in_explorer(path: &str) {
    unsafe {
        let (open, exe, arg) = (wide("open"), wide("explorer.exe"), wide(&format!("/select,\"{path}\"")));
        ShellExecuteW(HWND::default(), PCWSTR(open.as_ptr()), PCWSTR(exe.as_ptr()), PCWSTR(arg.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Run a program with arguments, no window (shutdown /r and the like).
pub fn run_hidden(exe: &str, args: &str) {
    unsafe {
        let (open, e, a) = (wide("open"), wide(exe), wide(args));
        ShellExecuteW(HWND::default(), PCWSTR(open.as_ptr()), PCWSTR(e.as_ptr()), PCWSTR(a.as_ptr()), PCWSTR::null(), SW_HIDE);
    }
}

/// File name of a process's exe ("SnippingTool.exe"), uncached.
pub fn exe_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(h);
        let path = from_wide(&buf[..n as usize]);
        ok.then(|| path.rsplit('\\').next().unwrap_or(&path).to_string())
    }
}

pub fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}
