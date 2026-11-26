//! The Windows key opens Spotlight (Settings › "Windows key opens Spotlight", off by default).
//!
//! A tap of Win on its own, with no other key between its press and release, normally opens Start. A
//! low-level keyboard hook on a thread of its own watches for that tap; on the release it sends the unassigned
//! VK 0xE8 first (as the Alt+Tab switcher does for Alt), so Windows sees a Win shortcut and keeps Start shut,
//! and asks the main thread for Spotlight. Win with any other key (Win+E, Win+Shift+S, Win+L) is left alone.
//! Ctrl+Esc still opens Start.
//!
//! The hook is installed the first time the setting is on and stays; with the setting off it passes every key
//! straight on.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static ON: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// the thread whose message loop turns WM_HOTKEY into Spotlight
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
/// a Win key is down
static HELD: AtomicBool = AtomicBool::new(false);
/// another key went down while it was
static COMBO: AtomicBool = AtomicBool::new(false);

/// Call on the main thread, at start and whenever the setting changes.
pub fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
    if !on || INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    std::thread::spawn(|| unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else { return };
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hinst, 0) else {
            INSTALLED.store(false, Ordering::Relaxed);
            return;
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
}

/// What a key event means for the Win tap. `win`: the key is a Win key; `down`: pressed (not released).
/// Returns true when this event ends a lone tap of Win.
fn step(win: bool, down: bool) -> bool {
    match (win, down) {
        (true, true) => {
            if !HELD.swap(true, Ordering::Relaxed) {
                COMBO.store(false, Ordering::Relaxed); // a fresh press (auto-repeat keeps HELD)
            }
            false
        }
        (true, false) => HELD.swap(false, Ordering::Relaxed) && !COMBO.load(Ordering::Relaxed),
        (false, true) => {
            if HELD.load(Ordering::Relaxed) {
                COMBO.store(true, Ordering::Relaxed);
            }
            false
        }
        (false, false) => false,
    }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
        },
    }
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 || !ON.load(Ordering::Relaxed) {
        return CallNextHookEx(HHOOK::default(), code, wp, lp);
    }
    let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
    if k.flags.0 & LLKHF_INJECTED.0 != 0 {
        return CallNextHookEx(HHOOK::default(), code, wp, lp); // our own 0xE8, or another tool's keys
    }
    let vk = k.vkCode as u16;
    let win = vk == VK_LWIN.0 || vk == VK_RWIN.0;
    let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    if step(win, down) {
        // a key between Win's press and release makes it a shortcut, so Start stays shut
        SendInput(&[key_input(0xE8, false), key_input(0xE8, true)], size_of::<INPUT>() as i32);
        let _ = PostThreadMessageW(MAIN_THREAD.load(Ordering::Relaxed), WM_HOTKEY, WPARAM(2), LPARAM(0));
    }
    CallNextHookEx(HHOOK::default(), code, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lone_tap_counts() {
        // tap
        assert!(!step(true, true));
        assert!(!step(true, true)); // auto-repeat
        assert!(step(true, false));
        // Win+E
        assert!(!step(true, true));
        assert!(!step(false, true));
        assert!(!step(false, false));
        assert!(!step(true, false));
        // a stray release with nothing held
        assert!(!step(true, false));
        // the next tap after a shortcut counts again
        assert!(!step(true, true));
        assert!(step(true, false));
    }
}
