//! Red unread badges for dock icons (port of Badges.cs). Windows keeps app badges inside its taskbar (not
//! readable while it is hidden), so the count comes from two places apps do expose: a "(N) " prefix in a
//! window title (WhatsApp Web, Gmail, many chat apps) and toasts still waiting in the Action Center that
//! arrived after the app was last in front.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// "(12) Inbox" → 12; "(99+) Chats" → 99.
pub fn from_title(title: &str) -> u32 {
    let Some(rest) = title.strip_prefix('(') else { return 0 };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 4 {
        return 0;
    }
    let after = &rest[digits.len()..];
    let after = after.strip_prefix('+').unwrap_or(after);
    if !after.starts_with(") ") && !after.starts_with(")\t") {
        return 0;
    }
    digits.parse().unwrap_or(0)
}

// ---------- Action Center toasts (%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db)

type Open = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
type Busy = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type Prepare = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int;
type Step = unsafe extern "C" fn(*mut c_void) -> c_int;
type Finalize = unsafe extern "C" fn(*mut c_void) -> c_int;
type ColText = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type ColI64 = unsafe extern "C" fn(*mut c_void, c_int) -> i64;

/// App id (AUMID) and arrival time (FILETIME UTC) of every toast in the Action Center.
/// winsqlite3.dll ships with Windows; loaded at run time so the build needs no import library.
pub fn toasts() -> Vec<(String, i64)> {
    let mut list = Vec::new();
    let db_path = format!("{}\\Microsoft\\Windows\\Notifications\\wpndatabase.db", std::env::var("LOCALAPPDATA").unwrap_or_default());
    if !std::path::Path::new(&db_path).exists() {
        return list;
    }
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("winsqlite3.dll")) else { return list };
        macro_rules! f {
            ($name:expr, $t:ty) => {
                match GetProcAddress(lib, $name) {
                    Some(p) => std::mem::transmute::<_, $t>(p),
                    None => return list,
                }
            };
        }
        let open: Open = f!(s!("sqlite3_open_v2"), Open);
        let close: Close = f!(s!("sqlite3_close"), Close);
        let busy: Busy = f!(s!("sqlite3_busy_timeout"), Busy);
        let prepare: Prepare = f!(s!("sqlite3_prepare_v2"), Prepare);
        let step: Step = f!(s!("sqlite3_step"), Step);
        let finalize: Finalize = f!(s!("sqlite3_finalize"), Finalize);
        let col_text: ColText = f!(s!("sqlite3_column_text"), ColText);
        let col_i64: ColI64 = f!(s!("sqlite3_column_int64"), ColI64);

        let Ok(path) = CString::new(db_path) else { return list };
        let mut db = std::ptr::null_mut();
        if open(path.as_ptr(), &mut db, 1 /* READONLY */, std::ptr::null()) != 0 {
            if !db.is_null() {
                close(db);
            }
            return list;
        }
        busy(db, 200);
        let sql = CString::new("select h.PrimaryId, n.ArrivalTime from Notification n join NotificationHandler h on h.RecordId = n.HandlerId where n.Type = 'toast'").unwrap();
        let mut st = std::ptr::null_mut();
        if prepare(db, sql.as_ptr(), -1, &mut st, std::ptr::null_mut()) == 0 {
            while step(st) == 100 {
                let t = col_text(st, 0);
                if !t.is_null() {
                    list.push((CStr::from_ptr(t).to_string_lossy().into_owned(), col_i64(st, 1)));
                }
            }
            finalize(st);
        }
        close(db);
    }
    list
}

// ---------- when each app was last looked at, so toasts already seen don't keep a badge up

pub struct Seen {
    map: HashMap<String, i64>, // lower-case AUMID → FILETIME
    baseline: i64,             // first run: toasts from before MacDock knew about them don't count
}

fn seen_path() -> String {
    format!("{}\\seen.json", crate::config::dir())
}

pub fn now_filetime() -> i64 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    (unix / 100) as i64 + 116_444_736_000_000_000
}

impl Seen {
    pub fn load() -> Seen {
        let mut map: HashMap<String, i64> = std::fs::read_to_string(seen_path())
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, i64>>(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        let baseline = match map.get("*") {
            Some(&b) => b,
            None => {
                let b = now_filetime();
                map.insert("*".into(), b);
                b
            }
        };
        let s = Seen { map, baseline };
        s.save();
        s
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(crate::config::dir());
        if let Ok(j) = serde_json::to_string(&self.map) {
            let _ = std::fs::write(seen_path(), j);
        }
    }

    pub fn at(&self, app: &str) -> i64 {
        self.map.get(&app.to_lowercase()).copied().unwrap_or(self.baseline)
    }

    pub fn mark(&mut self, app: &str) {
        self.map.insert(app.to_lowercase(), now_filetime());
        self.save();
    }
}
