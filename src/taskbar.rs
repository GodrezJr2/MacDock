//! Hides the Windows taskbar while the dock runs and puts it back exactly as it was (port of Taskbar.cs).

use std::cell::Cell;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetClassNameW, GetShellWindow, GetWindowThreadProcessId, IsWindowVisible, ShowWindow, ShowWindowAsync, SW_HIDE,
    SW_SHOW,
};

const ABS_AUTOHIDE: isize = 1;

thread_local! {
    static HIDDEN: Cell<bool> = const { Cell::new(false) };
    static ORIGINAL: Cell<isize> = const { Cell::new(0) };
    /// the show-event hook on Explorer's process and that process id (Explorer restarts get a new one)
    static WATCH: Cell<(isize, u32)> = const { Cell::new((0, 0)) };
}

const EVENT_OBJECT_SHOW: u32 = 0x8002;
const WINEVENT_OUTOFCONTEXT: u32 = 0;

fn is_bar(h: HWND) -> bool {
    let mut buf = [0u16; 32];
    let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
    let c = String::from_utf16_lossy(&buf[..n]);
    c == "Shell_TrayWnd" || c == "Shell_SecondaryTrayWnd"
}

/// Explorer shows the taskbar again on its own (leaving a fullscreen app, minimizing one, display changes):
/// hide it the moment that happens instead of waiting for the next refresh tick, so it never flashes.
unsafe extern "system" fn on_show(_: HWINEVENTHOOK, _: u32, h: HWND, obj: i32, _: i32, _: u32, _: u32) {
    if obj == 0 && !h.0.is_null() && HIDDEN.get() && is_bar(h) {
        let _ = ShowWindowAsync(h, SW_HIDE);
    }
}

fn watch(main: HWND) {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(main, Some(&mut pid)) };
    let (hook, cur) = WATCH.get();
    if pid == 0 || (pid == cur && hook != 0) {
        return;
    }
    unwatch();
    let h = unsafe { SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_show), pid, 0, WINEVENT_OUTOFCONTEXT) };
    WATCH.set((h.0 as isize, pid));
}

fn unwatch() {
    let (hook, _) = WATCH.get();
    if hook != 0 {
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut _));
        }
    }
    WATCH.set((0, 0));
}

/// Explorer's taskbar must be owned by the process owning the OS shell window; class names alone are spoofable.
fn shell_identity() -> Option<(HWND, u32)> {
    unsafe {
        let shell = GetShellWindow();
        if shell.0.is_null() {
            return None;
        }
        let mut shell_pid = 0u32;
        if GetWindowThreadProcessId(shell, Some(&mut shell_pid)) == 0 || shell_pid == 0 {
            return None;
        }
        Some((shell, shell_pid))
    }
}

fn main_bar() -> Option<(HWND, u32)> {
    let (_, shell_pid) = shell_identity()?;
    unsafe {
        let mut h = HWND::default();
        loop {
            h = FindWindowExW(HWND::default(), h, w!("Shell_TrayWnd"), None).ok()?;
            if h.0.is_null() {
                return None;
            }
            let mut pid = 0u32;
            if GetWindowThreadProcessId(h, Some(&mut pid)) != 0 && pid == shell_pid {
                return Some((h, shell_pid));
            }
        }
    }
}

/// The validated Explorer taskbar and its shell-owner PID.
pub fn explorer_taskbar() -> Option<(HWND, u32)> {
    main_bar()
}

/// The real Explorer taskbar owner, excluding our temporary notification-area host.
pub fn explorer_pid() -> Option<u32> {
    explorer_taskbar().map(|(_, pid)| pid)
}

/// SHAppBarMessage discovers its own receiver, independently of APPBARDATA.hWnd.
/// Only call it if the first tray window is Explorer's or our validated forwarding host.
pub unsafe fn appbar_message(msg: u32, data: &mut APPBARDATA) -> usize {
    let Ok(receiver) = FindWindowExW(HWND::default(), HWND::default(), w!("Shell_TrayWnd"), None) else { return 0 };
    let Some((_, pid)) = shell_identity() else { return 0 };
    let mut owner = 0;
    if GetWindowThreadProcessId(receiver, Some(&mut owner)) == 0 { return 0; }
    if owner != pid && !(owner == std::process::id() && crate::tray::owns_window(receiver)) { return 0; }
    SHAppBarMessage(msg, data)
}

fn bars() -> Vec<HWND> {
    let Some((main, shell_pid)) = explorer_taskbar() else { return Vec::new() };
    let mut v = vec![main];
    let mut h = HWND::default();
    while let Ok(n) = unsafe { FindWindowExW(HWND::default(), h, w!("Shell_SecondaryTrayWnd"), None) } {
        if n.0.is_null() {
            break;
        }
        let mut pid = 0u32;
        let tid = unsafe { GetWindowThreadProcessId(n, Some(&mut pid)) };
        if tid != 0 && pid == shell_pid {
            v.push(n);
        }
        h = n;
    }
    v
}

fn data(h: HWND, state: isize) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: h, lParam: LPARAM(state), ..Default::default() }
}

pub fn hide() {
    let Some((main, _)) = main_bar() else { return };
    unsafe {
        let mut d = data(main, 0);
        let state = appbar_message(ABM_GETSTATE, &mut d) as isize;
        if !HIDDEN.get() {
            ORIGINAL.set(state);
        }
        // autohide so the taskbar stops reserving space, then hide it outright. Only send the state when it
        // differs: every ABM_SETSTATE makes Explorer re-show the bar a moment later.
        if state & ABS_AUTOHIDE == 0 {
            let mut d = data(main, state | ABS_AUTOHIDE);
            appbar_message(ABM_SETSTATE, &mut d);
        }
        for b in bars() {
            let _ = ShowWindow(b, SW_HIDE);
        }
    }
    HIDDEN.set(true);
    watch(main);
}

/// Explorer re-shows the taskbar after restarts or display changes.
pub fn keep_hidden() {
    if HIDDEN.get() && bars().into_iter().any(|b| unsafe { IsWindowVisible(b) }.as_bool()) {
        hide();
    }
}

/// `force` (the --restore escape hatch) puts the taskbar back even if this process never hid it.
pub fn restore(force: bool) {
    let hidden = HIDDEN.get();
    if !hidden && !force {
        return;
    }
    unwatch();
    let Some((main, _)) = main_bar() else { return };
    unsafe {
        let mut d = data(main, if hidden { ORIGINAL.get() } else { 0 });
        appbar_message(ABM_SETSTATE, &mut d);
        for b in bars() {
            let _ = ShowWindow(b, SW_SHOW);
        }
    }
    HIDDEN.set(false);
}
