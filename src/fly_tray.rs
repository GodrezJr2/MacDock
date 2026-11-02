//! The ^ extra (port of TrayFlyout.cs): the notification area icons in a grid. Hosting starts when the flyout
//! opens (apps answer TaskbarCreated within a moment and the grid fills live) and stops when it closes.

use std::time::Instant;

use crate::flyout::{white, Cmd, Content, Spot, Ui};
use crate::tray;

const CELL: f32 = 44.0;
const ID_ICON: u32 = 100;

const WM_MOUSEMOVE: u32 = 0x200;
const WM_LBUTTONDOWN: u32 = 0x201;
const WM_LBUTTONUP: u32 = 0x202;
const WM_RBUTTONDOWN: u32 = 0x204;
const WM_RBUTTONUP: u32 = 0x205;
const WM_CONTEXTMENU: u32 = 0x7B;
const NIN_SELECT: u32 = 0x400;
const NIN_POPUPOPEN: u32 = 0x406;
const NIN_POPUPCLOSE: u32 = 0x407;

pub struct TrayFly {
    opened: Instant,
    hosted: bool,
    /// (hwnd, uid) per drawn cell, so a click finds its icon even if the list changed since
    shown: Vec<(u32, u32)>,
    hot: Option<u32>,
}

impl TrayFly {
    pub fn new() -> TrayFly {
        TrayFly { opened: Instant::now(), hosted: false, shown: Vec::new(), hot: None }
    }

    fn with_icon(&self, id: u32, f: impl FnOnce(&tray::Icon)) {
        let Some(&(h, u)) = self.shown.get(id.wrapping_sub(ID_ICON) as usize) else { return };
        tray::with_icons(|icons| {
            if let Some(ic) = icons.and_then(|v| v.iter().find(|i| i.hwnd == h && i.uid == u)) {
                f(ic);
            }
        });
    }
}

impl Content for TrayFly {
    fn width(&self) -> f32 {
        5.0 * CELL + 2.0 * 14.0 + 2.0 * 13.0 + 4.0
    }

    fn opened(&mut self, hwnd: isize) {
        self.hosted = tray::start(hwnd);
    }

    fn closing(&mut self) {
        if let Some(id) = self.hot.take() {
            self.with_icon(id, |ic| tray::send_mouse(ic, NIN_POPUPCLOSE));
        }
        tray::stop();
    }

    fn period_ms(&self) -> u32 {
        // repaint while "Loading…" can still turn into "Nothing running"
        500
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let mut shown = Vec::new();
        let h = tray::with_icons(|icons| {
            let visible: Vec<&tray::Icon> = icons.map(|v| v.iter().filter(|i| i.image.is_some() && !i.hidden).collect()).unwrap_or_default();
            let n = visible.len();
            // header: the hovered icon's tooltip, else the title; count on the right
            let hot_tip = self.hot.and_then(|id| visible.get(id.wrapping_sub(ID_ICON) as usize)).map(|i| match i.tip.lines().next().map(str::trim).filter(|t| !t.is_empty()) { Some(t) => t.to_string(), None => i.name.clone() }).filter(|t| !t.is_empty());
            if n > 0 {
                let cw = ui.text_w(&n.to_string(), 11.5, 400);
                ui.text_mid(w - 2.0 - cw, 9.0, &n.to_string(), 11.5, 400, white(0x80), cw + 2.0);
            }
            let title = hot_tip.unwrap_or_else(|| "Background apps".into());
            ui.text_mid(2.0, 9.0, &title, 13.0, 600, white(0xF2), w - 34.0);
            let mut y = 22.0;
            if n == 0 {
                let msg = if !self.hosted {
                    "The notification area could not be hosted."
                } else if self.opened.elapsed().as_secs_f32() < 1.4 {
                    "Loading…"
                } else {
                    "Nothing running in the background."
                };
                y += 6.0;
                let lh = ui.text_wrap(4.0, y, msg, 12.0, 400, white(0x80), w - 8.0);
                return y + lh + 2.0;
            }
            let ch = CELL - 6.0;
            for (i, ic) in visible.iter().enumerate() {
                let (cx, cy) = ((i % 5) as f32 * CELL, y + (i / 5) as f32 * ch);
                let id = ID_ICON + i as u32;
                if ui.hit(id, cx, cy, CELL, ch) {
                    ui.fill(cx, cy, CELL, ch, 9.0, white(0x24));
                }
                if let Some(img) = &ic.image {
                    ui.image(img, cx + (CELL - 20.0) / 2.0, cy + (ch - 20.0) / 2.0, 20.0, 20.0);
                }
                shown.push((ic.hwnd, ic.uid));
            }
            y += n.div_ceil(5) as f32 * ch;
            y
        });
        self.shown = shown;
        h
    }

    fn placed(&mut self, spots: &[Spot], origin: (i32, i32), dpi: f32) {
        let shown = &self.shown;
        tray::with_icons(|icons| {
            let Some(icons) = icons else { return };
            for s in spots.iter().filter(|s| s.id >= ID_ICON) {
                let Some(&(h, u)) = shown.get((s.id - ID_ICON) as usize) else { continue };
                if let Some(ic) = icons.iter_mut().find(|i| i.hwnd == h && i.uid == u) {
                    let (l, t) = (origin.0 + (s.x * dpi) as i32, origin.1 + (s.y * dpi) as i32);
                    ic.place = windows::Win32::Foundation::RECT { left: l, top: t, right: l + (s.w * dpi) as i32, bottom: t + (s.h * dpi) as i32 };
                }
            }
        });
    }

    fn hover(&mut self, id: Option<u32>) {
        let id = id.filter(|&i| i >= ID_ICON);
        if id == self.hot {
            return;
        }
        if let Some(old) = self.hot {
            self.with_icon(old, |ic| {
                if ic.version >= 4 {
                    tray::send_mouse(ic, NIN_POPUPCLOSE);
                }
            });
        }
        self.hot = id;
        if let Some(new) = id {
            self.with_icon(new, |ic| {
                tray::send_mouse(ic, WM_MOUSEMOVE);
                if ic.version >= 4 {
                    tray::send_mouse(ic, NIN_POPUPOPEN);
                }
            });
        }
    }

    fn press(&mut self, id: u32, _fx: f32) {
        self.with_icon(id, |ic| tray::send_mouse(ic, WM_LBUTTONDOWN));
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        if id < ID_ICON {
            return Cmd::None;
        }
        self.with_icon(id, |ic| {
            tray::send_mouse(ic, WM_LBUTTONUP);
            if ic.version >= 3 {
                tray::send_mouse(ic, NIN_SELECT);
            }
        });
        // a left click usually opens the app's window: get out of its way
        Cmd::Close
    }

    fn right_click(&mut self, id: u32) -> Cmd {
        // the app's menu takes the foreground, which closes us anyway
        self.with_icon(id, |ic| {
            tray::send_mouse(ic, WM_RBUTTONDOWN);
            tray::send_mouse(ic, WM_RBUTTONUP);
            if ic.version >= 3 {
                tray::send_mouse(ic, WM_CONTEXTMENU);
            }
        });
        Cmd::None
    }
}
