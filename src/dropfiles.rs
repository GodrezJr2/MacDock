//! Files dragged onto the dock (like macOS): over an app icon they open with that app, over the Trash they go
//! to the Recycle Bin. An OLE drop target on the dock window; it only queues events, the dock decides.
//! The dock publishes where its icons are (screen px) so DragOver can show "no drop" over gaps and dividers.

use std::cell::RefCell;

use windows::core::{implement, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDataObject, CLSCTX_LOCAL_SERVER, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, DragQueryFileW, IApplicationActivationManager, SHCreateShellItemArrayFromIDLists,
    SHFileOperationW, SHParseDisplayName, ShellExecuteW, FOF_ALLOWUNDO, FO_DELETE, HDROP, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::dock::DockEv;

/// What an icon does with dropped files.
#[derive(Clone, Copy, PartialEq)]
pub enum Zone {
    Open,
    Trash,
}

thread_local! {
    /// (left, top, right, bottom) in screen px per droppable icon; empty while the dock is hidden.
    static ZONES: RefCell<Vec<(i32, i32, i32, i32, Zone)>> = const { RefCell::new(Vec::new()) };
}

pub fn set_zones(z: Vec<(i32, i32, i32, i32, Zone)>) {
    ZONES.with(|c| *c.borrow_mut() = z);
}

fn zone_at(x: i32, y: i32) -> Option<Zone> {
    ZONES.with(|c| c.borrow().iter().find(|r| x >= r.0 && x < r.2 && y >= r.1 && y < r.3).map(|r| r.4))
}

fn effect_at(pt: &POINTL) -> DROPEFFECT {
    match zone_at(pt.x, pt.y) {
        Some(Zone::Open) => DROPEFFECT_COPY,
        Some(Zone::Trash) => DROPEFFECT_MOVE,
        None => DROPEFFECT_NONE,
    }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

fn files_of(data: &IDataObject) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(mut med) = data.GetData(&hdrop_format()) else { return out };
        let h = HDROP(med.u.hGlobal.0);
        let n = DragQueryFileW(h, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(h, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(h, i, Some(&mut buf)) as usize;
            out.push(String::from_utf16_lossy(&buf[..got]));
        }
        ReleaseStgMedium(&mut med);
    }
    out
}

#[implement(IDropTarget)]
struct Target {
    has_files: std::cell::Cell<bool>,
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let ok = data.is_some_and(|d| unsafe { d.QueryGetData(&hdrop_format()) }.is_ok());
        self.has_files.set(ok);
        crate::push(crate::Ev::Dock(DockEv::DragFiles(ok)));
        unsafe { *effect = if ok { effect_at(pt) } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        crate::push(crate::Ev::Dock(DockEv::Wake));
        unsafe { *effect = if self.has_files.get() { effect_at(pt) } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        self.has_files.set(false);
        crate::push(crate::Ev::Dock(DockEv::DragFiles(false)));
        Ok(())
    }

    fn Drop(&self, data: Option<&IDataObject>, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let e = if self.has_files.get() { effect_at(pt) } else { DROPEFFECT_NONE };
        unsafe { *effect = e };
        self.has_files.set(false);
        crate::push(crate::Ev::Dock(DockEv::DragFiles(false)));
        if e != DROPEFFECT_NONE {
            if let Some(d) = data {
                let files = files_of(d);
                if !files.is_empty() {
                    crate::push(crate::Ev::Dock(DockEv::DropFiles(files, pt.x, pt.y)));
                }
            }
        }
        Ok(())
    }
}

/// Make the dock window a drop target (OleInitialize must have run on this thread).
pub fn register(hwnd: HWND) {
    let t: IDropTarget = Target { has_files: std::cell::Cell::new(false) }.into();
    unsafe {
        let _ = RegisterDragDrop(hwnd, &t);
    }
}

fn quote(f: &str) -> String {
    format!("\"{f}\"")
}

/// Open `files` with a packaged app (by AUMID), so e.g. Photos gets the files instead of a plain launch.
fn open_packaged(aumid: &str, files: &[String]) -> bool {
    unsafe {
        let Ok(mgr) = CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) else {
            return false;
        };
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for f in files {
            let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(f.as_str()), None, &mut p, 0, None).is_ok() && !p.is_null() {
                pidls.push(p);
            }
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
        let ok = match SHCreateShellItemArrayFromIDLists(&consts) {
            Ok(arr) => mgr.ActivateForFile(&HSTRING::from(aumid), &arr, &HSTRING::from("open")).is_ok(),
            Err(_) => false,
        };
        for p in pidls {
            CoTaskMemFree(Some(p as *const _));
        }
        ok
    }
}

fn is_shell(t: &str) -> bool {
    t.len() > 6 && t.is_char_boundary(6) && t[..6].eq_ignore_ascii_case("shell:")
}

/// Open `files` with an app. Desktop apps (also when pinned as shell:AppsFolder\<AUMID>, resolved to their exe)
/// get the files as arguments; packaged apps are activated for the files by AUMID. If neither works, each file
/// opens with its default app.
pub fn open_with(target: Option<&str>, aumid: Option<&str>, files: &[String]) {
    let mut exe = target.filter(|t| !is_shell(t)).map(str::to_string);
    if exe.is_none() {
        exe = target.filter(|t| is_shell(t)).and_then(crate::shell::link_target).filter(|p| !is_shell(p));
    }
    let packaged = exe.as_deref().is_none_or(|e| e.to_lowercase().contains(r"\windowsapps\"));
    if packaged && aumid.is_some_and(|a| open_packaged(a, files)) {
        return;
    }
    let Some(exe) = exe else {
        for f in files {
            crate::shell::launch(f);
        }
        return;
    };
    let args = files.iter().map(|f| quote(f)).collect::<Vec<_>>().join(" ");
    unsafe {
        ShellExecuteW(HWND::default(), &HSTRING::from("open"), &HSTRING::from(exe), &HSTRING::from(args), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Move `files` to the Recycle Bin (undoable, Explorer's own confirm setting applies).
pub fn recycle(files: &[String]) {
    let mut from: Vec<u16> = Vec::new();
    for f in files {
        from.extend(f.encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: FOF_ALLOWUNDO.0 as u16,
        ..Default::default()
    };
    unsafe {
        let _ = SHFileOperationW(&mut op);
    }
}
