//! Physical monitor coordinates; DIPs are local to each monitor's scale.
use windows::Win32::Foundation::{BOOL, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub id: HMONITOR,
    pub rect: RECT,
    pub scale: f64,
    pub primary: bool,
}

impl Display {
    fn read(id: HMONITOR) -> Option<Self> {
        unsafe {
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(id, &mut info).as_bool() { return None; }
            let (mut dx, mut dy) = (96, 96);
            let scale = if GetDpiForMonitor(id, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 { dx as f64 / 96.0 } else { 1.0 };
            Some(Self { id, rect: info.rcMonitor, scale, primary: info.dwFlags & 1 != 0 })
        }
    }

    pub fn primary() -> Self {
        unsafe {
            let id = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
            Self::read(id).unwrap_or_else(|| Self { id, primary: true, scale: 1.0, rect: RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) } })
        }
    }

    pub fn dimensions(self) -> (f64, f64, f64) {
        ((self.rect.right - self.rect.left) as f64 / self.scale, (self.rect.bottom - self.rect.top) as f64 / self.scale, self.scale)
    }

    pub fn bottom_strip(self, height: i32) -> RECT { RECT { top: self.rect.bottom - height, ..self.rect } }

    pub fn at_bottom(self, p: POINT) -> bool {
        let r = self.bottom_strip(2);
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    }
}

pub fn secondary() -> Vec<Display> {
    unsafe extern "system" fn each(id: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        if let Some(d) = Display::read(id).filter(|d| !d.primary) { (*(data.0 as *mut Vec<Display>)).push(d); }
        true.into()
    }
    let mut out = Vec::new();
    unsafe { let _ = EnumDisplayMonitors(HDC::default(), None, Some(each), LPARAM(&mut out as *mut _ as isize)); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secondary_above_left_with_mixed_dpi() {
        let d = Display { id: HMONITOR::default(), primary: false, scale: 1.5, rect: RECT { left: -1920, top: -1080, right: 0, bottom: 0 } };
        assert_eq!(d.dimensions(), (1280.0, 720.0, 1.5));
        assert_eq!(d.bottom_strip(90).top, -90);
        assert!(d.at_bottom(POINT { x: -1920, y: -2 }));
        assert!(!d.at_bottom(POINT { x: 0, y: -1 }));
        assert!(!d.at_bottom(POINT { x: -1, y: 0 }));
    }
}
