//! The notification area host (port of Tray.cs, which used ManagedShell). With the taskbar hidden the tray icons
//! (Discord, Steam, OneDrive…) would be unreachable, so while the ^ flyout is open we register our own
//! Shell_TrayWnd on top of Explorer's and broadcast TaskbarCreated: every app re-adds its icon to us through
//! Shell_NotifyIcon (WM_COPYDATA). AppBar messages that land on our window are passed on to Explorer.
//!
//! Only while the flyout is open: a foreign Shell_TrayWnd makes Windows App SDK report the shell as unsupported
//! (AppWindow.IsShownInSwitchers throws E_NOTIMPL) and WinUI apps that set it at startup crash. Stopping destroys
//! the window and broadcasts TaskbarCreated again, so every icon goes back to Explorer.

use std::cell::RefCell;

use tiny_skia::{IntSize, Pixmap};
use windows::core::{w, GUID};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Icon {
    pub hwnd: u32,
    pub uid: u32,
    pub guid: Option<GUID>,
    pub callback: u32,
    pub version: u32,
    pub hicon: u32,
    pub tip: String,
    /// the owning app's friendly name, for icons without a tooltip
    pub name: String,
    pub hidden: bool,
    pub image: Option<Pixmap>,
    /// screen rect of its cell, for apps that anchor popups to the icon
    pub place: RECT,
}

struct Host {
    hwnd: HWND,
    explorer: HWND,
    icons: Vec<Icon>,
    notify: isize, // the flyout to wake on changes
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

const TIMER_TOP: usize = 1;

fn taskbar_created() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Ask every app to (re-)add its icons to whichever Shell_TrayWnd is on top now.
fn broadcast() {
    unsafe {
        let _ = SendNotifyMessageW(HWND_BROADCAST, taskbar_created(), WPARAM(0), LPARAM(0));
    }
}

pub fn running() -> bool {
    HOST.with(|h| h.try_borrow().map(|h| h.is_some()).unwrap_or(true))
}

/// Start hosting; `notify` gets WM_FLY_DATA whenever the icons change.
pub fn start(notify: isize) -> bool {
    if running() {
        HOST.with(|h| {
            if let Ok(mut h) = h.try_borrow_mut() {
                if let Some(h) = h.as_mut() {
                    h.notify = notify;
                }
            }
        });
        return true;
    }
    unsafe {
        let explorer = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: w!("Shell_TrayWnd"), ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW, w!("Shell_TrayWnd"), w!(""), WS_POPUP, 0, 0, 0, 0, HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return false;
        };
        HOST.with(|h| *h.borrow_mut() = Some(Host { hwnd, explorer, icons: Vec::new(), notify }));
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetTimer(hwnd, TIMER_TOP, 500, None);
        broadcast();
    }
    true
}

pub fn stop() {
    let host = HOST.with(|h| h.borrow_mut().take());
    if let Some(h) = host {
        unsafe {
            let _ = DestroyWindow(h.hwnd);
        }
        broadcast(); // everything goes back to Explorer
    }
}

/// Run `f` over the visible icons (with images).
pub fn with_icons<R>(f: impl FnOnce(Option<&mut Vec<Icon>>) -> R) -> R {
    HOST.with(|h| match h.try_borrow_mut() {
        Ok(mut h) => f(h.as_mut().map(|h| &mut h.icons)),
        Err(_) => f(None),
    })
}

fn wstr(b: &[u8], off: usize, chars: usize) -> String {
    let end = (off + chars * 2).min(b.len());
    let v: Vec<u16> = b[off.min(end)..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&v)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).unwrap_or(0)
}

fn guid_at(b: &[u8], o: usize) -> Option<GUID> {
    let s = b.get(o..o + 16)?;
    let g = GUID::from_values(
        u32::from_le_bytes(s[0..4].try_into().unwrap()),
        u16::from_le_bytes(s[4..6].try_into().unwrap()),
        u16::from_le_bytes(s[6..8].try_into().unwrap()),
        s[8..16].try_into().unwrap(),
    );
    (g != GUID::zeroed()).then_some(g)
}

/// An HICON as premultiplied BGRA: drawn on black and on white, alpha from the difference (works for 32-bit
/// icons and for old masked ones alike).
pub fn icon_pixmap(hicon: u32, px: u32) -> Option<Pixmap> {
    if hicon == 0 {
        return None;
    }
    let h = HICON(hicon as usize as *mut _);
    let draw = |bg: u8| -> Option<Vec<u8>> {
        unsafe {
            let dc = CreateCompatibleDC(HDC::default());
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: px as i32, biHeight: -(px as i32), biPlanes: 1,
                    biBitCount: 32, biCompression: BI_RGB.0, ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp);
            std::ptr::write_bytes(bits as *mut u8, bg, (px * px * 4) as usize);
            let ok = DrawIconEx(dc, 0, 0, h, px as i32, px as i32, 0, HBRUSH::default(), DI_NORMAL).is_ok();
            let _ = GdiFlush();
            let v = std::slice::from_raw_parts(bits as *const u8, (px * px * 4) as usize).to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(dc);
            ok.then_some(v)
        }
    };
    let (b, wh) = (draw(0)?, draw(255)?);
    let mut out = vec![0u8; b.len()];
    for i in (0..b.len()).step_by(4) {
        // on black: c·a ; on white: c·a + (1 - a)·255  →  a = 255 - (white - black)
        let d = (0..3).map(|k| wh[i + k] as i32 - b[i + k] as i32).max().unwrap_or(0);
        let a = (255 - d).clamp(0, 255) as u8;
        for k in 0..3 {
            out[i + k] = b[i + k].min(a);
        }
        out[i + 3] = a;
    }
    Pixmap::from_vec(out, IntSize::from_wh(px, px)?)
}

fn app_name(hwnd: u32) -> String {
    let pid = crate::shell::window_pid(HWND(hwnd as usize as *mut _));
    let exe = crate::shell::Shell::default().process_info(pid).0;
    exe.map(|e| crate::shell::friendly_name(Some(&e), None, "")).unwrap_or_default()
}

/// Shell_NotifyIcon arriving as WM_COPYDATA (dwData 1): SHELLTRAYDATA { signature, message, NOTIFYICONDATA32 }.
fn on_notify(host: &mut Host, b: &[u8]) -> bool {
    if b.len() < 8 + 24 {
        return false;
    }
    let msg = u32_at(b, 4);
    let n = &b[8..];
    let (hwnd, uid, flags) = (u32_at(n, 4), u32_at(n, 8), u32_at(n, 12));
    let guid = if flags & 0x20 != 0 { guid_at(n, 936) } else { None };
    let pos = host.icons.iter().position(|i| match (guid, i.guid) {
        (Some(g), Some(ig)) => g == ig,
        _ => i.hwnd == hwnd && i.uid == uid,
    });
    match msg {
        0 | 1 => {
            // NIM_ADD / NIM_MODIFY
            let i = match pos {
                Some(p) => p,
                None if msg == 0 || msg == 1 => {
                    host.icons.push(Icon {
                        hwnd, uid, guid, callback: 0, version: 0, hicon: 0, tip: String::new(), name: app_name(hwnd), hidden: false, image: None, place: RECT::default(),
                    });
                    host.icons.len() - 1
                }
                None => return false,
            };
            let ic = &mut host.icons[i];
            if flags & 1 != 0 {
                ic.callback = u32_at(n, 16);
            }
            if flags & 2 != 0 {
                let hicon = u32_at(n, 20);
                if hicon != ic.hicon || ic.image.is_none() {
                    ic.hicon = hicon;
                    ic.image = icon_pixmap(hicon, 32);
                }
            }
            if flags & 4 != 0 {
                ic.tip = wstr(n, 24, 128);
            }
            if flags & 8 != 0 {
                let (state, mask) = (u32_at(n, 280), u32_at(n, 284));
                if mask & 1 != 0 {
                    ic.hidden = state & 1 != 0;
                }
            }
            true
        }
        2 => {
            // NIM_DELETE
            if let Some(p) = pos {
                host.icons.remove(p);
                true
            } else {
                false
            }
        }
        4 => {
            // NIM_SETVERSION
            if let Some(p) = pos {
                host.icons[p].version = u32_at(n, 800);
                true
            } else {
                false
            }
        }
        _ => pos.is_some(),
    }
}

/// Shell_NotifyIconGetRect (dwData 3): WINNOTIFYICONIDENTIFIER { magic, message, cbSize, pad, hWnd, uID, guid }.
fn on_identify(host: &Host, b: &[u8]) -> isize {
    let (msg, hwnd, uid, guid) = (u32_at(b, 4), u32_at(b, 16), u32_at(b, 20), guid_at(b, 24));
    let Some(ic) = host.icons.iter().find(|i| match (guid, i.guid) {
        (Some(g), Some(ig)) => g == ig,
        _ => i.hwnd == hwnd && i.uid == uid,
    }) else {
        return 0;
    };
    let r = ic.place;
    let mk = |x: i32, y: i32| ((x & 0xFFFF) | ((y & 0xFFFF) << 16)) as isize;
    match msg {
        1 => mk(r.left, r.top),
        2 => mk(r.right, r.bottom),
        _ => 0,
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            let bytes = if cds.lpData.is_null() { &[][..] } else { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            let r = HOST.with(|host| {
                let Ok(mut g) = host.try_borrow_mut() else { return None };
                let host = g.as_mut()?;
                match cds.dwData {
                    1 => {
                        let ok = on_notify(host, bytes);
                        if ok {
                            let _ = PostMessageW(HWND(host.notify as *mut _), crate::flyout::WM_FLY_DATA, WPARAM(0), LPARAM(0));
                        }
                        Some(LRESULT(ok as isize))
                    }
                    3 => Some(LRESULT(on_identify(host, bytes))),
                    _ => {
                        // AppBar and anything else: Explorer's business
                        let ex = host.explorer;
                        drop(g);
                        if ex.0.is_null() {
                            return Some(LRESULT(0));
                        }
                        Some(SendMessageW(ex, WM_COPYDATA, wp, lp))
                    }
                }
            });
            return r.unwrap_or(LRESULT(0));
        }
        WM_TIMER => {
            // stay above Explorer's Shell_TrayWnd so Shell_NotifyIcon keeps finding us first
            let _ = SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Mouse input for an icon, the way Explorer delivers it (NOTIFYICON_VERSION_4 packs the cursor into wParam).
pub fn send_mouse(ic: &Icon, mouse_msg: u32) {
    if ic.callback == 0 {
        return;
    }
    let mut p = windows::Win32::Foundation::POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
        let target = HWND(ic.hwnd as usize as *mut _);
        let mut pid = 0u32;
        GetWindowThreadProcessId(target, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        let (wp, lp) = if ic.version >= 4 {
            (((p.x & 0xFFFF) | ((p.y & 0xFFFF) << 16)) as usize, ((mouse_msg & 0xFFFF) | ((ic.uid & 0xFFFF) << 16)) as isize)
        } else {
            (ic.uid as usize, mouse_msg as isize)
        };
        let _ = SendNotifyMessageW(target, ic.callback, WPARAM(wp), LPARAM(lp));
    }
}
