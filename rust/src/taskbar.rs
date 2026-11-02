//! Hides the Windows taskbar while the dock runs and puts it back exactly as it was (port of Taskbar.cs).

use std::cell::Cell;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, IsWindowVisible, ShowWindow, SW_HIDE, SW_SHOW,
};

const ABS_AUTOHIDE: isize = 1;

thread_local! {
    static HIDDEN: Cell<bool> = const { Cell::new(false) };
    static ORIGINAL: Cell<isize> = const { Cell::new(0) };
}

/// Explorer's own taskbar, skipping any Shell_TrayWnd this process registers (a future tray host).
fn main_bar() -> Option<HWND> {
    let me = std::process::id();
    let mut h = HWND::default();
    loop {
        h = unsafe { FindWindowExW(HWND::default(), h, w!("Shell_TrayWnd"), None) }.ok()?;
        if h.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if pid != me {
            return Some(h);
        }
    }
}

fn bars() -> Vec<HWND> {
    let mut v: Vec<HWND> = main_bar().into_iter().collect();
    let mut h = HWND::default();
    while let Ok(n) = unsafe { FindWindowExW(HWND::default(), h, w!("Shell_SecondaryTrayWnd"), None) } {
        if n.0.is_null() {
            break;
        }
        v.push(n);
        h = n;
    }
    v
}

fn data(h: HWND, state: isize) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: h, lParam: LPARAM(state), ..Default::default() }
}

pub fn hide() {
    let Some(main) = main_bar() else { return };
    unsafe {
        let mut d = data(main, 0);
        let state = SHAppBarMessage(ABM_GETSTATE, &mut d) as isize;
        if !HIDDEN.get() {
            ORIGINAL.set(state);
        }
        // autohide so the taskbar stops reserving space, then hide it outright. Only send the state when it
        // differs: every ABM_SETSTATE makes Explorer re-show the bar a moment later.
        if state & ABS_AUTOHIDE == 0 {
            let mut d = data(main, state | ABS_AUTOHIDE);
            SHAppBarMessage(ABM_SETSTATE, &mut d);
        }
        // only the main screen's bar: MacDock lives there. The other screens keep their taskbar (autohide is one
        // setting for all of them, so it slides up from the bottom edge there).
        let _ = ShowWindow(main, SW_HIDE);
        for b in bars().into_iter().filter(|&b| b != main) {
            if !IsWindowVisible(b).as_bool() {
                let _ = ShowWindow(b, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNA);
            }
        }
    }
    HIDDEN.set(true);
}

/// Explorer re-shows the taskbar after restarts or display changes.
pub fn keep_hidden() {
    if HIDDEN.get() && main_bar().is_some_and(|b| unsafe { IsWindowVisible(b) }.as_bool()) {
        hide();
    }
}

/// `force` (the --restore escape hatch) puts the taskbar back even if this process never hid it.
pub fn restore(force: bool) {
    let hidden = HIDDEN.get();
    if !hidden && !force {
        return;
    }
    let Some(main) = main_bar() else { return };
    unsafe {
        let mut d = data(main, if hidden { ORIGINAL.get() } else { 0 });
        SHAppBarMessage(ABM_SETSTATE, &mut d);
        for b in bars() {
            let _ = ShowWindow(b, SW_SHOW);
        }
    }
    HIDDEN.set(false);
}
