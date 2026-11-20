//! The clock extra: a month calendar (today marked, arrows step months, the title jumps back to today) with
//! rows for the notification centre and the date & time settings.

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::flyout::{white, Cmd, Content, Ui, ACCENT4};

const CELL: f32 = 36.0;
const ID_PREV: u32 = 1;
const ID_NEXT: u32 = 2;
const ID_TITLE: u32 = 3;
const ID_NOTIF: u32 = 4;
const ID_SETTINGS: u32 = 5;

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

fn leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in(y: i32, m: i32) -> i32 {
    [31, if leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(m - 1) as usize]
}

/// 0 = Sunday (Sakamoto).
fn weekday(y: i32, m: i32, d: i32) -> i32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d).rem_euclid(7)
}

fn today() -> (i32, i32, i32) {
    let t = unsafe { GetLocalTime() };
    (t.wYear as i32, t.wMonth as i32, t.wDay as i32)
}

pub struct CalFly {
    year: i32,
    month: i32,
}

impl CalFly {
    pub fn new() -> CalFly {
        let (y, m, _) = today();
        CalFly { year: y, month: m }
    }

    fn step(&mut self, by: i32) {
        let i = self.year * 12 + (self.month - 1) + by;
        self.year = i.div_euclid(12);
        self.month = i.rem_euclid(12) + 1;
    }
}

impl Content for CalFly {
    fn width(&self) -> f32 {
        7.0 * CELL + 2.0 * 14.0 + 2.0 * 13.0
    }

    /// the day can roll over while open
    fn period_ms(&self) -> u32 {
        30_000
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let (ty, tm, td) = today();

        // today in full
        let wd = DAYS[weekday(ty, tm, td) as usize];
        ui.text_mid(2.0, 9.0, &format!("{wd}, {td} {} {ty}", MONTHS[(tm - 1) as usize]), 13.0, 600, white(0xF2), w - 4.0);
        let mut y = 26.0;
        ui.divider(y);
        y += 10.0;

        // month title (click = back to today) and arrows
        let title = format!("{} {}", MONTHS[(self.month - 1) as usize], self.year);
        let tw = ui.text_w(&title, 14.0, 600);
        let away = (self.year, self.month) != (ty, tm);
        if ui.hit(ID_TITLE, 0.0, y, tw + 12.0, 26.0) && away {
            ui.fill(0.0, y, tw + 12.0, 26.0, 7.0, white(0x1E));
        }
        ui.text_mid(6.0, y + 13.0, &title, 14.0, 600, if away { ACCENT4 } else { white(0xF2) }, 0.0);
        for (id, x, g) in [(ID_PREV, w - 56.0, '\u{E76B}'), (ID_NEXT, w - 28.0, '\u{E76C}')] {
            if ui.hit(id, x, y, 26.0, 26.0) {
                ui.fill(x, y, 26.0, 26.0, 7.0, white(0x1E));
            }
            ui.glyph(x + 13.0, y + 13.0, g, 11.0, white(0xC8));
        }
        y += 32.0;

        // weekday heads
        for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
            let cw = ui.text_w(d, 11.0, 600);
            ui.text_mid(i as f32 * CELL + (CELL - cw) / 2.0, y + 8.0, d, 11.0, 600, white(0x70), 0.0);
        }
        y += 20.0;

        // the grid: always 6 rows so the panel never changes height while stepping months
        let first = weekday(self.year, self.month, 1);
        let n = days_in(self.year, self.month);
        let (py, pm) = if self.month == 1 { (self.year - 1, 12) } else { (self.year, self.month - 1) };
        let prev_n = days_in(py, pm);
        let rh = CELL - 4.0;
        for cell in 0..42 {
            let (col, row) = (cell % 7, cell / 7);
            let d = cell - first + 1;
            let (num, inside) = if d < 1 {
                (prev_n + d, false)
            } else if d > n {
                (d - n, false)
            } else {
                (d, true)
            };
            let (cx, cy) = (col as f32 * CELL + CELL / 2.0, y + row as f32 * rh + rh / 2.0);
            let is_today = inside && (self.year, self.month, d) == (ty, tm, td);
            if is_today {
                ui.circle(cx, cy, 13.0, ACCENT4);
            }
            let s = num.to_string();
            let c = if is_today {
                white(0xFF)
            } else if !inside {
                white(0x48)
            } else if col == 0 || col == 6 {
                white(0x98)
            } else {
                white(0xE6)
            };
            let sw = ui.text_w(&s, 12.5, if is_today { 600 } else { 400 });
            ui.text_mid(cx - sw / 2.0, cy, &s, 12.5, if is_today { 600 } else { 400 }, c, 0.0);
        }
        y += 6.0 * rh + 6.0;

        ui.divider(y);
        y += 6.0;
        for (id, label) in [(ID_NOTIF, "Notifications"), (ID_SETTINGS, "Date & Time Settings…")] {
            ui.row(id, 0.0, y, w, 28.0);
            ui.text_mid(8.0, y + 14.0, label, 13.0, 400, white(0xE6), w - 16.0);
            y += 28.0;
        }
        y
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_PREV => self.step(-1),
            ID_NEXT => self.step(1),
            ID_TITLE => *self = CalFly::new(),
            ID_NOTIF => return Cmd::Launch("ms-actioncenter:".into()),
            ID_SETTINGS => return Cmd::Launch("ms-settings:dateandtime".into()),
            _ => {}
        }
        Cmd::None
    }

    fn wheel(&mut self, delta: i32) {
        self.step(if delta > 0 { -1 } else { 1 });
    }
}
