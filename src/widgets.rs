//! Desktop widgets (macOS Sonoma / Tahoe style): four customizable tiles in a 2x2 grid on the desktop
//! — analog clock, digital clock, today's date, battery ring, CPU / memory / disk bars, or hidden.
//! One layered window kept at the bottom of the z-order, so it sits on the wallpaper under every app.
//! The tiles are frosted Liquid Glass over the wallpaper (built once, rebuilt when the wallpaper or display changes);
//! only the contents redraw, every 5 s and only when something shown changed.
//! Left-click opens the matching app (Clock, Calendar, battery settings, Task Manager), left-drag freely
//! moves the widget with corner snapping, and right-click allows slot customization, position snapping,
//! and unhiding slots.

use std::cell::RefCell;
use std::f32::consts::PI;

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::gfx::{self, Font, Surface};
use crate::menu::Entry;

const TILE: f32 = 164.0; // DIPs
const GAP: f32 = 14.0;
const RADIUS: f32 = 26.0;
const MARGIN: f32 = 22.0; // shadow room
const TIMER_TICK: usize = 1;
/// a new wallpaper has faded in: cut the glass again
const TIMER_WALLPAPER: usize = 2;
const TICK_MS: u32 = 5000;

const RED: (u8, u8, u8) = (0xFF, 0x45, 0x3A);
const GREEN: (u8, u8, u8) = (0x30, 0xD1, 0x58);
const YELLOW: (u8, u8, u8) = (0xFF, 0xD6, 0x0A);
const BLUE: (u8, u8, u8) = (0x0A, 0x84, 0xFF);
const PURPLE: (u8, u8, u8) = (0xBF, 0x5A, 0xF2);
const ORANGE: (u8, u8, u8) = (0xFF, 0x9F, 0x0A);

const DAYS: [&str; 7] = ["SUNDAY", "MONDAY", "TUESDAY", "WEDNESDAY", "THURSDAY", "FRIDAY", "SATURDAY"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
];

thread_local! {
    static STATE: RefCell<Option<Widgets>> = const { RefCell::new(None) };
    /// show desktop is on: the desktop window sits in the topmost band, so we must too to stay visible
    static RAISED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Follow the desktop up while show desktop is on, back to the bottom when it ends.
pub fn set_raised(on: bool) {
    if RAISED.with(|r| r.replace(on)) == on {
        return;
    }
    STATE.with(|st| {
        if let Ok(b) = st.try_borrow() {
            if let Some(w) = b.as_ref() {
                let after = if on { HWND_TOPMOST } else { HWND_BOTTOM };
                unsafe {
                    let _ = SetWindowPos(w.hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
        }
    });
}

struct Widgets {
    hwnd: HWND,
    s: f32,          // device px per DIP
    pos: (i32, i32), // window top-left, screen px
    size: u32,       // window is square
    glass: Option<Pixmap>, // shadows + frosted tiles, no content
    face: Option<Pixmap>,  // clock dial without hands
    surface: Option<Surface>,
    cpu_prev: Option<(u64, u64)>, // (idle, total) 100 ns units
    cpu: f32,
    shown: String,   // what the last frame displayed; same again = no repaint
    widget_items: Vec<String>,
    widget_position: String,
    widget_offset: Option<(i32, i32)>,
    dragging: bool,
    drag_start: (i32, i32),
    initial_pos: (i32, i32),
}

fn col(c: (u8, u8, u8), a: u8) -> Color {
    // surfaces are BGRA
    Color::from_rgba8(c.2, c.1, c.0, a)
}

fn paint(c: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(col(c, a));
    p.anti_alias = true;
    p
}

fn ft(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// (percent, plugged in, charging); None without a battery.
fn power() -> Option<(u8, bool, bool)> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).ok()? };
    if ps.BatteryFlag == 128 || ps.BatteryLifePercent > 100 {
        return None;
    }
    let ac = ps.ACLineStatus == 1;
    Some((ps.BatteryLifePercent, ac, ac && ps.BatteryFlag & 8 != 0))
}

fn memory() -> f32 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe {
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            return m.dwMemoryLoad as f32 / 100.0;
        }
    }
    0.0
}

fn disk() -> f32 {
    let (mut free, mut total) = (0u64, 0u64);
    unsafe {
        if GetDiskFreeSpaceExW(w!("C:\\"), None, Some(&mut total), Some(&mut free)).is_ok() && total > 0 {
            return 1.0 - free as f32 / total as f32;
        }
    }
    0.0
}

/// Work area (screen px): the menu bar and dock reserve their strips.
fn work_area() -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    r
}

#[derive(Clone, Debug)]
enum Act {
    SetWidget(usize, &'static str),
    HideTile(usize),
    RestoreSlot(usize, &'static str),
    RestoreAllHidden,
    SetPos(&'static str),
    Reset,
    OpenSettings,
}

impl Widgets {
    pub fn widget_at(&self, slot: usize) -> &str {
        if slot < self.widget_items.len() {
            &self.widget_items[slot]
        } else {
            match slot {
                0 => "clock",
                1 => "date",
                2 => "battery",
                3 => "system",
                _ => "none",
            }
        }
    }

    /// (top-left px of tile, in client coords)
    fn tile_origin(&self, i: usize) -> (f32, f32) {
        let s = self.s;
        let tw = TILE * s;
        let gap = GAP * s;
        let m = MARGIN * s;
        let (col, row) = (i % 2, i / 2);
        (m + col as f32 * (tw + gap), m + row as f32 * (tw + gap))
    }

    fn place(&mut self) {
        self.s = unsafe { GetDpiForSystem() } as f32 / 96.0;
        let s = self.s;
        self.size = ((2.0 * MARGIN + 2.0 * TILE + GAP) * s).ceil() as u32;
        let wa = work_area();
        let margin_gap = (20.0 - MARGIN) * s;
        let top_gap = (16.0 - MARGIN) * s;
        match self.widget_position.as_str() {
            "top-left" => {
                self.pos = (wa.left + margin_gap as i32, wa.top + top_gap as i32);
            }
            "bottom-right" => {
                self.pos = (
                    wa.right - margin_gap as i32 - self.size as i32,
                    wa.bottom - margin_gap as i32 - self.size as i32,
                );
            }
            "bottom-left" => {
                self.pos = (
                    wa.left + margin_gap as i32,
                    wa.bottom - margin_gap as i32 - self.size as i32,
                );
            }
            "custom" => {
                if let Some(off) = self.widget_offset {
                    // Clamped to work area so widgets come back after a monitor or DPI change
                    let max_x = (wa.right - self.size as i32).max(wa.left);
                    let max_y = (wa.bottom - self.size as i32).max(wa.top);
                    self.pos = (off.0.clamp(wa.left, max_x), off.1.clamp(wa.top, max_y));
                } else {
                    self.pos = (wa.right - margin_gap as i32 - self.size as i32, wa.top + top_gap as i32);
                }
            }
            _ => {
                // "top-right" default
                self.pos = (wa.right - margin_gap as i32 - self.size as i32, wa.top + top_gap as i32);
            }
        }
        unsafe {
            let after = if RAISED.with(|r| r.get()) { HWND_TOPMOST } else { HWND_BOTTOM };
            let _ = SetWindowPos(self.hwnd, after, self.pos.0, self.pos.1, self.size as i32, self.size as i32, SWP_NOACTIVATE);
        }
        self.glass = None;
        self.shown.clear();
    }

    /// Build the shadow and frosted tile backings once.
    fn build_glass(&mut self) {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) };
        let n = self.size;
        let Some(mut pm) = Pixmap::new(n, n) else { return };
        // only the part of the wallpaper under us, blurred hard (frosted like macOS widgets)
        let full = gfx::wallpaper(sw, sh);
        let Some(mut under) = Pixmap::new(n, n) else { return };
        if let Some(full) = &full {
            under.draw_pixmap(-self.pos.0, -self.pos.1, full.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        } else {
            under.fill(Color::from_rgba8(0x40, 0x30, 0x28, 0xFF));
        }
        gfx::blur(&mut under, 26.0 * self.s);
        let (tw, r) = (TILE * self.s, RADIUS * self.s);
        for i in 0..4 {
            if self.widget_at(i) == "none" {
                continue;
            }
            let (x, y) = self.tile_origin(i);
            if let Some(path) = gfx::rrect(x, y + 4.0 * self.s, tw, tw, r) {
                if let Some((sh_pm, ox, oy)) = gfx::shadow(&path, 14.0 * self.s, (0, 0, 0, 0x50)) {
                    pm.draw_pixmap(ox as i32, oy as i32, sh_pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
            }
            if let Some((mut g, gx, gy)) = gfx::liquid_glass(&under, x, y, tw, tw, r, 14.0 * self.s, (0.0, 0.0), (0.0, 0.0)) {
                // a dark veil keeps white text readable on any wallpaper
                let veil = paint((0, 0, 0), 0x46);
                if let Some(p) = gfx::rrect(x - gx as f32, y - gy as f32, tw, tw, r) {
                    if let Some(mut veil_pm) = Pixmap::new(g.width(), g.height()) {
                        veil_pm.fill_path(&p, &veil, FillRule::Winding, Transform::identity(), None);
                        g.draw_pixmap(0, 0, veil_pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    }
                }
                pm.draw_pixmap(gx, gy, g.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
        self.glass = Some(pm);
    }

    /// Text helper. Anchor: 0 = left, 1 = center, 2 = right.
    fn text(&self, pm: &mut Pixmap, s: &str, dip: f32, weight: i32, c: (u8, u8, u8), a: u8, x: f32, y: f32, anchor: u8) -> f32 {
        let Some(t) = gfx::text_ex(s, Font { face: gfx::UI, px: (dip * self.s).round() as i32, weight }, c) else { return 0.0 };
        let w = t.width() as f32;
        let x = match anchor {
            1 => x - w / 2.0,
            2 => x - w,
            _ => x,
        };
        let mut p = PixmapPaint::default();
        p.opacity = a as f32 / 255.0;
        pm.draw_pixmap(x.round() as i32, y.round() as i32, t.as_ref(), &p, Transform::identity(), None);
        w
    }

    /// Analog clock dial: hour ticks and numerals.
    fn build_face(&mut self) {
        let s = self.s;
        let tw = TILE * s;
        let mut pm = Pixmap::new(tw as u32, tw as u32).unwrap();
        let cx = tw / 2.0;
        let cy = cx;
        let r = 62.0 * s;

        // hour markers
        for i in 0..12 {
            let a = i as f32 * PI / 6.0;
            let (sa, ca) = (a.sin(), -a.cos());
            let r0 = if i % 3 == 0 { r - 9.0 * s } else { r - 5.0 * s };
            let r1 = r;
            let mut pb = PathBuilder::new();
            pb.move_to(cx + r0 * sa, cy + r0 * ca);
            pb.line_to(cx + r1 * sa, cy + r1 * ca);
            if let Some(p) = pb.finish() {
                let mut stroke = Stroke::default();
                stroke.width = if i % 3 == 0 { 2.5 * s } else { 1.5 * s };
                stroke.line_cap = LineCap::Round;
                pm.stroke_path(&p, &paint((255, 255, 255), if i % 3 == 0 { 0xE0 } else { 0x80 }), &stroke, Transform::identity(), None);
            }
        }
        self.face = Some(pm);
    }

    fn hand(&self, pm: &mut Pixmap, cx: f32, cy: f32, angle: f32, len: f32, width: f32, c: (u8, u8, u8), tail: f32) {
        let (sa, ca) = (angle.sin(), -angle.cos());
        let mut pb = PathBuilder::new();
        pb.move_to(cx - tail * sa, cy - tail * ca);
        pb.line_to(cx + len * sa, cy + len * ca);
        if let Some(p) = pb.finish() {
            let mut st = Stroke::default();
            st.width = width;
            st.line_cap = LineCap::Round;
            pm.stroke_path(&p, &paint(c, 0xFF), &st, Transform::identity(), None);
        }
    }

    fn arc(&self, pm: &mut Pixmap, cx: f32, cy: f32, r: f32, width: f32, sweep: f32, c: (u8, u8, u8), a: u8) {
        let mut pb = PathBuilder::new();
        let steps = (sweep.abs() * 32.0).ceil().max(2.0) as usize;
        let start = -PI / 2.0;
        for i in 0..=steps {
            let th = start + sweep * (i as f32 / steps as f32);
            let (x, y) = (cx + r * th.cos(), cy + r * th.sin());
            if i == 0 { pb.move_to(x, y); } else { pb.line_to(x, y); }
        }
        if let Some(p) = pb.finish() {
            let mut st = Stroke::default();
            st.width = width;
            st.line_cap = LineCap::Round;
            pm.stroke_path(&p, &paint(c, a), &st, Transform::identity(), None);
        }
    }

    fn sample_cpu(&mut self) -> f32 {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe {
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                let id = ft(idle);
                let tot = ft(kernel) + ft(user);
                if let Some((pi, pt)) = self.cpu_prev {
                    let dtot = tot.saturating_sub(pt);
                    let didle = id.saturating_sub(pi);
                    if dtot > 0 {
                        self.cpu = (1.0 - didle as f32 / dtot as f32).clamp(0.0, 1.0);
                    }
                }
                self.cpu_prev = Some((id, tot));
            }
        }
        self.cpu
    }

    fn draw_digital_clock(&self, pm: &mut Pixmap, x: f32, y: f32) {
        let t = unsafe { GetLocalTime() };
        let s = self.s;
        let tw = TILE * s;
        let cx = x + tw / 2.0;
        let cy = y + tw / 2.0;

        let day_idx = (t.wDayOfWeek as usize).min(6);
        let header = format!("{}, {} {}", DAYS[day_idx], t.wDay, MONTHS[(t.wMonth as usize - 1).min(11)]);
        self.text(pm, &header.to_uppercase(), 10.0, 700, RED, 0xFF, cx, cy - 38.0 * s, 1);

        let hour = t.wHour;
        let time_str = format!("{:02}:{:02}", hour, t.wMinute);
        self.text(pm, &time_str, 38.0, 700, (255, 255, 255), 0xFF, cx, cy - 2.0 * s, 1);

        let ampm = if hour >= 12 { "PM" } else { "AM" };
        let sec_str = format!("{:02}s  {}", t.wSecond, ampm);
        self.text(pm, &sec_str, 12.0, 500, (255, 255, 255), 0xAA, cx, cy + 32.0 * s, 1);
    }

    fn render(&mut self) {
        let t = unsafe { GetLocalTime() };
        let s = self.s;
        let tw = TILE * s;

        // Skip render if nothing changed
        let base_key = format!(
            "{}:{}:{}:{}:{}:{}",
            self.widget_at(0),
            self.widget_at(1),
            self.widget_at(2),
            self.widget_at(3),
            t.wMinute,
            (self.cpu * 100.0) as u32
        );
        let has_digital = (0..4).any(|i| self.widget_at(i) == "digital_clock");
        let key = if has_digital {
            format!("{}:{}:{}", base_key, t.wSecond, t.wHour)
        } else {
            base_key
        };
        if self.shown == key && self.glass.is_some() {
            return;
        }
        self.shown = key;

        if self.glass.is_none() {
            self.build_glass();
        }
        let Some(glass) = self.glass.as_ref() else { return };
        let mut pm = glass.clone();

        for i in 0..4 {
            let kind = self.widget_at(i);
            if kind == "none" {
                continue;
            }
            let (x, y) = self.tile_origin(i);
            let cx = x + tw / 2.0;
            let cy = y + tw / 2.0;

            match kind {
                "clock" => {
                    if self.face.is_none() {
                        self.build_face();
                    }
                    if let Some(face) = self.face.as_ref() {
                        pm.draw_pixmap(x as i32, y as i32, face.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    }
                    let hr = (t.wHour % 12) as f32 + t.wMinute as f32 / 60.0;
                    let min = t.wMinute as f32 + t.wSecond as f32 / 60.0;
                    let sec = t.wSecond as f32;

                    self.hand(&mut pm, cx, cy, hr * PI / 6.0, 36.0 * s, 4.0 * s, (255, 255, 255), 6.0 * s);
                    self.hand(&mut pm, cx, cy, min * PI / 30.0, 52.0 * s, 2.5 * s, (255, 255, 255), 8.0 * s);
                    self.hand(&mut pm, cx, cy, sec * PI / 30.0, 56.0 * s, 1.5 * s, ORANGE, 12.0 * s);

                    let mut pb = PathBuilder::new();
                    pb.push_circle(cx, cy, 3.5 * s);
                    if let Some(p) = pb.finish() {
                        pm.fill_path(&p, &paint(ORANGE, 0xFF), FillRule::Winding, Transform::identity(), None);
                    }
                }
                "digital_clock" => {
                    self.draw_digital_clock(&mut pm, x, y);
                }
                "date" | "calendar" => {
                    let d = (t.wDayOfWeek as usize).min(6);
                    self.text(&mut pm, DAYS[d], 12.0, 700, RED, 0xFF, cx, y + 26.0 * s, 1);
                    let day_str = format!("{}", t.wDay);
                    self.text(&mut pm, &day_str, 56.0, 200, (255, 255, 255), 0xFF, cx, y + 44.0 * s, 1);
                    let m = (t.wMonth as usize).saturating_sub(1).min(11);
                    self.text(&mut pm, MONTHS[m], 13.0, 500, (255, 255, 255), 0xCC, cx, y + 124.0 * s, 1);
                }
                "battery" => {
                    let (pct, plugged, charging) = power().unwrap_or((100, true, false));
                    let r = 48.0 * s;
                    let w = 12.0 * s;
                    self.arc(&mut pm, cx, cy, r, w, 2.0 * PI, (255, 255, 255), 0x28);
                    let sweep = 2.0 * PI * (pct as f32 / 100.0);
                    let c = if pct <= 20 && !plugged { RED } else { GREEN };
                    self.arc(&mut pm, cx, cy, r, w, sweep, c, 0xFF);

                    let s_pct = format!("{}%", pct);
                    self.text(&mut pm, &s_pct, 24.0, 600, (255, 255, 255), 0xFF, cx, cy - 14.0 * s, 1);
                    let state = if charging { "Charging" } else if plugged { "Power Adapter" } else { "Battery" };
                    self.text(&mut pm, state, 10.0, 500, (255, 255, 255), 0xAA, cx, cy + 14.0 * s, 1);
                }
                "system" => {
                    let cpu = self.sample_cpu();
                    let mem = memory();
                    let dsk = disk();
                    self.text(&mut pm, "SYSTEM", 11.0, 700, (255, 255, 255), 0x90, x + 16.0 * s, y + 16.0 * s, 0);

                    let mut ry = y + 40.0 * s;
                    let pad = 16.0 * s;
                    let bw = tw - 2.0 * pad;
                    for (lbl, v, colr) in [("CPU", cpu, BLUE), ("RAM", mem, PURPLE), ("SSD", dsk, YELLOW)] {
                        self.text(&mut pm, lbl, 11.0, 600, (255, 255, 255), 0xE0, x + pad, ry, 0);
                        let pct_s = format!("{:.0}%", (v * 100.0).round());
                        self.text(&mut pm, &pct_s, 11.0, 500, (255, 255, 255), 0x90, x + tw - pad, ry, 2);

                        let by = ry + 16.0 * s;
                        if let Some(p) = gfx::rrect(x + pad, by, bw, 6.0 * s, 3.0 * s) {
                            pm.fill_path(&p, &paint((255, 255, 255), 0x24), FillRule::Winding, Transform::identity(), None);
                        }
                        if let Some(p) = gfx::rrect(x + pad, by, (bw * v).max(6.0 * s), 6.0 * s, 3.0 * s) {
                            pm.fill_path(&p, &paint(colr, 0xFF), FillRule::Winding, Transform::identity(), None);
                        }
                        ry += 36.0 * s;
                    }
                }
                _ => {}
            }
        }

        if self.surface.as_ref().is_none_or(|sf| sf.w != self.size) {
            self.surface = Surface::new(self.size, self.size);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.bytes().copy_from_slice(pm.data());
            sf.present(self.hwnd, self.pos.0, self.pos.1);
        }
        unsafe {
            let after = if RAISED.with(|r| r.get()) { HWND_TOPMOST } else { HWND_BOTTOM };
            let _ = SetWindowPos(self.hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Which visible (non-hidden) tile is under a client point (device px).
    fn tile_at(&self, px: f32, py: f32) -> Option<usize> {
        let tw = TILE * self.s;
        (0..4).find(|&i| {
            if self.widget_at(i) == "none" {
                return false;
            }
            let (x, y) = self.tile_origin(i);
            px >= x && px < x + tw && py >= y && py < y + tw
        })
    }

    /// Which 2x2 grid slot is under a client point (device px), even if currently hidden.
    fn slot_at(&self, px: f32, py: f32) -> Option<usize> {
        let tw = TILE * self.s;
        (0..4).find(|&i| {
            let (x, y) = self.tile_origin(i);
            px >= x && px < x + tw && py >= y && py < y + tw
        })
    }

    fn snap_or_save_drag(&mut self) {
        let s = self.s;
        let wa = work_area();
        let margin_gap = (20.0 - MARGIN) * s;
        let top_gap = (16.0 - MARGIN) * s;
        let tr = (wa.right - margin_gap as i32 - self.size as i32, wa.top + top_gap as i32);
        let tl = (wa.left + margin_gap as i32, wa.top + top_gap as i32);
        let br = (
            wa.right - margin_gap as i32 - self.size as i32,
            wa.bottom - margin_gap as i32 - self.size as i32,
        );
        let bl = (
            wa.left + margin_gap as i32,
            wa.bottom - margin_gap as i32 - self.size as i32,
        );

        let thresh = (80.0 * s) as f32;
        let dist = |p1: (i32, i32), p2: (i32, i32)| {
            let dx = (p1.0 - p2.0) as f32;
            let dy = (p1.1 - p2.1) as f32;
            (dx * dx + dy * dy).sqrt()
        };

        if dist(self.pos, tr) < thresh {
            self.pos = tr;
            self.widget_position = "top-right".into();
            self.widget_offset = None;
        } else if dist(self.pos, tl) < thresh {
            self.pos = tl;
            self.widget_position = "top-left".into();
            self.widget_offset = None;
        } else if dist(self.pos, br) < thresh {
            self.pos = br;
            self.widget_position = "bottom-right".into();
            self.widget_offset = None;
        } else if dist(self.pos, bl) < thresh {
            self.pos = bl;
            self.widget_position = "bottom-left".into();
            self.widget_offset = None;
        } else {
            self.widget_position = "custom".into();
            // Clamp to work area so widgets never get lost off-screen
            let max_x = (wa.right - self.size as i32).max(wa.left);
            let max_y = (wa.bottom - self.size as i32).max(wa.top);
            self.pos = (self.pos.0.clamp(wa.left, max_x), self.pos.1.clamp(wa.top, max_y));
            self.widget_offset = Some(self.pos);
        }
        Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
        unsafe {
            let after = if RAISED.with(|r| r.get()) { HWND_TOPMOST } else { HWND_BOTTOM };
            let _ = SetWindowPos(self.hwnd, after, self.pos.0, self.pos.1, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
        if let Some(sf) = self.surface.as_mut() {
            sf.present(self.hwnd, self.pos.0, self.pos.1);
        }
        self.glass = None;
        self.shown.clear();
        self.render();
    }

    fn build_context_menu(&self, slot: usize) -> (Vec<Entry>, Vec<Option<Act>>) {
        let cur_kind = self.widget_at(slot).to_string();
        let cur_pos = self.widget_position.clone();
        let slot_name = match slot {
            0 => "Top Left",
            1 => "Top Right",
            2 => "Bottom Left",
            3 => "Bottom Right",
            _ => "Tile",
        };
        let slot_label = format!("Tile {} - {} ({})", slot + 1, slot_name, match cur_kind.as_str() {
            "clock" => "Analog Clock",
            "digital_clock" => "Digital Clock",
            "date" | "calendar" => "Calendar & Date",
            "battery" => "Battery",
            "system" => "System",
            _ => "Hidden",
        });

        let mut entries = Vec::new();
        let mut acts = Vec::new();
        let mut add = |e: Entry, a: Option<Act>| {
            entries.push(e);
            acts.push(a);
        };

        add(Entry::item(slot_label).enabled(false), None);
        add(Entry::Sep, None);

        add(Entry::item("Analog Clock").checked(cur_kind == "clock"), Some(Act::SetWidget(slot, "clock")));
        add(Entry::item("Digital Clock").checked(cur_kind == "digital_clock"), Some(Act::SetWidget(slot, "digital_clock")));
        add(Entry::item("Calendar & Date").checked(cur_kind == "date" || cur_kind == "calendar"), Some(Act::SetWidget(slot, "date")));
        add(Entry::item("Battery").checked(cur_kind == "battery"), Some(Act::SetWidget(slot, "battery")));
        add(Entry::item("System (CPU / RAM / Disk)").checked(cur_kind == "system"), Some(Act::SetWidget(slot, "system")));
        add(Entry::item("Hide this tile").checked(cur_kind == "none"), Some(Act::HideTile(slot)));

        add(Entry::Sep, None);

        // If any slot is hidden, allow restoring directly without resetting position
        let hidden_slots: Vec<usize> = (0..4).filter(|&i| self.widget_at(i) == "none").collect();
        if !hidden_slots.is_empty() {
            for &h in &hidden_slots {
                if h != slot {
                    let name = match h {
                        0 => "Top Left",
                        1 => "Top Right",
                        2 => "Bottom Left",
                        3 => "Bottom Right",
                        _ => "Tile",
                    };
                    let default_for_slot = match h {
                        0 => ("clock", "Analog Clock"),
                        1 => ("date", "Calendar & Date"),
                        2 => ("battery", "Battery"),
                        3 => ("system", "System"),
                        _ => ("clock", "Analog Clock"),
                    };
                    add(
                        Entry::item(format!("Restore Slot {} ({}) as {}", h + 1, name, default_for_slot.1)),
                        Some(Act::RestoreSlot(h, default_for_slot.0)),
                    );
                }
            }
            add(Entry::item("Restore All Hidden Tiles"), Some(Act::RestoreAllHidden));
            add(Entry::Sep, None);
        }

        add(Entry::item("Snap: Top Right").checked(cur_pos == "top-right"), Some(Act::SetPos("top-right")));
        add(Entry::item("Snap: Top Left").checked(cur_pos == "top-left"), Some(Act::SetPos("top-left")));
        add(Entry::item("Snap: Bottom Right").checked(cur_pos == "bottom-right"), Some(Act::SetPos("bottom-right")));
        add(Entry::item("Snap: Bottom Left").checked(cur_pos == "bottom-left"), Some(Act::SetPos("bottom-left")));

        add(Entry::Sep, None);
        add(Entry::item("Reset Widgets Layout"), Some(Act::Reset));
        add(Entry::item("MacDock Settings…"), Some(Act::OpenSettings));

        (entries, acts)
    }

    fn apply_act(&mut self, act: Act) {
        match act {
            Act::SetWidget(slot, kind) => {
                while self.widget_items.len() <= slot {
                    self.widget_items.push("none".into());
                }
                self.widget_items[slot] = kind.into();
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.glass = None;
                self.shown.clear();
                self.render();
            }
            Act::HideTile(slot) => {
                while self.widget_items.len() <= slot {
                    self.widget_items.push("none".into());
                }
                self.widget_items[slot] = "none".into();
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.glass = None;
                self.shown.clear();
                self.render();
            }
            Act::RestoreSlot(slot, kind) => {
                while self.widget_items.len() <= slot {
                    self.widget_items.push("none".into());
                }
                self.widget_items[slot] = kind.into();
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.glass = None;
                self.shown.clear();
                self.render();
            }
            Act::RestoreAllHidden => {
                let defaults = ["clock", "date", "battery", "system"];
                for i in 0..4 {
                    if self.widget_at(i) == "none" {
                        while self.widget_items.len() <= i {
                            self.widget_items.push("none".into());
                        }
                        self.widget_items[i] = defaults[i].into();
                    }
                }
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.glass = None;
                self.shown.clear();
                self.render();
            }
            Act::SetPos(pos) => {
                self.widget_position = pos.into();
                self.widget_offset = None;
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.place();
                self.render();
            }
            Act::Reset => {
                self.widget_items = vec!["clock".into(), "date".into(), "battery".into(), "system".into()];
                self.widget_position = "top-right".into();
                self.widget_offset = None;
                Config::save_widget_layout(&self.widget_items, &self.widget_position, self.widget_offset);
                self.place();
                self.render();
            }
            Act::OpenSettings => {
                crate::push(crate::Ev::Settings(crate::fly_settings::Change::Open));
            }
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_TICK => {
            STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        w.render();
                    }
                }
            });
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_WALLPAPER => {
            let _ = KillTimer(h, TIMER_WALLPAPER);
            STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        w.place();
                        w.render();
                    }
                }
            });
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if msg == WM_SETTINGCHANGE && wp.0 == 0x14 /* SPI_SETDESKWALLPAPER */ {
                SetTimer(h, TIMER_WALLPAPER, 3500, None);
            }
            // wallpaper, work area (menu bar / dock reserve), resolution or scale changed
            STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        w.place();
                        w.render();
                    }
                }
            });
            return LRESULT(0);
        }
        WM_WINDOWPOSCHANGING => {
            // stay on the desktop: never above another window (with show desktop on, above the desktop window)
            let p = &mut *(lp.0 as *mut WINDOWPOS);
            p.hwndInsertAfter = if RAISED.with(|r| r.get()) { HWND_TOPMOST } else { HWND_BOTTOM };
            p.flags &= !SWP_NOZORDER;
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            let hit = STATE.with(|st| {
                let Ok(b) = st.try_borrow() else { return false };
                b.as_ref().and_then(|w| w.tile_at(x, y)).is_some()
            });
            if hit {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let _ = SetCapture(h);
                STATE.with(|st| {
                    if let Ok(mut b) = st.try_borrow_mut() {
                        if let Some(w) = b.as_mut() {
                            w.dragging = true;
                            w.drag_start = (p.x, p.y);
                            w.initial_pos = w.pos;
                        }
                    }
                });
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        if w.dragging {
                            let mut p = POINT::default();
                            let _ = GetCursorPos(&mut p);
                            let dx = p.x - w.drag_start.0;
                            let dy = p.y - w.drag_start.1;
                            w.pos = (w.initial_pos.0 + dx, w.initial_pos.1 + dy);
                            let after = if RAISED.with(|r| r.get()) { HWND_TOPMOST } else { HWND_BOTTOM };
                            let _ = SetWindowPos(w.hwnd, after, w.pos.0, w.pos.1, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                            if let Some(sf) = w.surface.as_mut() {
                                sf.present(w.hwnd, w.pos.0, w.pos.1);
                            }
                        }
                    }
                }
            });
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        w.dragging = false;
                    }
                }
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let was_dragging = STATE.with(|st| {
                if let Ok(mut b) = st.try_borrow_mut() {
                    if let Some(w) = b.as_mut() {
                        if w.dragging {
                            w.dragging = false;
                            let _ = ReleaseCapture();
                            let mut p = POINT::default();
                            let _ = GetCursorPos(&mut p);
                            let moved = (p.x - w.drag_start.0).abs() > 4 || (p.y - w.drag_start.1).abs() > 4;
                            if moved {
                                w.snap_or_save_drag();
                                return true;
                            }
                        }
                    }
                }
                false
            });
            if was_dragging {
                return LRESULT(0);
            }

            let (x, y) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            let tile = STATE.with(|st| {
                let Ok(b) = st.try_borrow() else { return None };
                b.as_ref().and_then(|w| w.tile_at(x, y))
            });
            if let Some(t) = tile {
                let target = match STATE.with(|st| {
                    let Ok(b) = st.try_borrow() else { return None };
                    b.as_ref().map(|w| w.widget_at(t).to_string())
                }) {
                    Some(k) if k == "clock" || k == "digital_clock" => "ms-clock:",
                    Some(k) if k == "date" || k == "calendar" => "outlookcal:",
                    Some(k) if k == "battery" => "ms-settings:batterysaver",
                    Some(k) if k == "system" => "taskmgr.exe",
                    _ => "",
                };
                if !target.is_empty() {
                    crate::shell::launch(target);
                }
            }
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let menu_info = STATE.with(|st| {
                let Ok(b) = st.try_borrow() else { return None };
                let w = b.as_ref()?;
                let cx = (p.x - w.pos.0) as f32;
                let cy = (p.y - w.pos.1) as f32;
                // Use slot_at so clicking empty/hidden slot areas allows configuring that slot
                let slot = w.slot_at(cx, cy).or_else(|| w.tile_at(cx, cy)).unwrap_or(0);
                Some((w.hwnd, w.s as f64, w.build_context_menu(slot)))
            });
            if let Some((hwnd, dpi, (entries, acts))) = menu_info {
                let chosen = crate::menu::popup(hwnd, &entries, crate::menu::Anchor::Point(p.x, p.y), dpi);
                if let Some(act) = chosen.and_then(|i| acts.get(i).cloned()).flatten() {
                    STATE.with(|st| {
                        if let Ok(mut b) = st.try_borrow_mut() {
                            if let Some(w) = b.as_mut() {
                                w.apply_act(act);
                            }
                        }
                    });
                }
            }
            return LRESULT(0);
        }
        WM_NCHITTEST => {
            // Active tiles and slot areas take clicks; margins outside the 2x2 grid fall through to desktop
            let (sx, sy) = ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32);
            let hit = STATE.with(|st| {
                let Ok(b) = st.try_borrow() else { return None };
                let w = b.as_ref()?;
                let rx = sx - w.pos.0 as f32;
                let ry = sy - w.pos.1 as f32;
                // Active tiles take clicks
                w.tile_at(rx, ry)
            });
            return LRESULT(if hit.is_some() { HTCLIENT as isize } else { HTTRANSPARENT as isize });
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Create the widget window and start its 5 s tick.
pub fn start() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockWidgets"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("MacDockWidgets"), w!("MacDock Widgets"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return;
        };
        let cfg = Config::load();
        let mut wd = Widgets {
            hwnd,
            s: 1.0,
            pos: (0, 0),
            size: 1,
            glass: None,
            face: None,
            surface: None,
            cpu_prev: None,
            cpu: 0.0,
            shown: String::new(),
            widget_items: cfg.widget_items,
            widget_position: cfg.widget_position,
            widget_offset: cfg.widget_offset,
            dragging: false,
            drag_start: (0, 0),
            initial_pos: (0, 0),
        };
        wd.place();
        wd.render();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        STATE.with(|st| *st.borrow_mut() = Some(wd));
        SetTimer(hwnd, TIMER_TICK, TICK_MS, None);
    }
}
