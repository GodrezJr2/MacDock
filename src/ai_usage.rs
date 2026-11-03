//! AI coding-agent usage read from the agents' own local logs (nothing leaves the machine):
//!
//! - Claude Code: `%USERPROFILE%\.claude\projects\**\*.jsonl`, one line per API message with `message.usage`.
//!   A message streamed in several blocks repeats the same usage on several lines, so lines are deduped by
//!   `message.id`. Tokens counted = input + cache writes + output (cache reads are cheap and would swamp it).
//!   Claude Code logs no limit, so a percentage only exists when config.json sets `ClaudeLimit5h` (tokens per
//!   5-hour block). Blocks follow ccusage: start at the hour of the first message, last 5 h.
//! - Codex: `%USERPROFILE%\.codex\sessions\**\*.jsonl`, `event_msg` / `token_count` lines with the turn's
//!   `last_token_usage` and the server's own `rate_limits` (used_percent, window_minutes, resets_at).
//!   Tokens counted = uncached input + output.
//!
//! A worker thread reads only what was appended since the last pass (byte offset per file), keeps 7 days of
//! events and publishes a `Summary` that the menu bar and the flyout read.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const KEEP_SECS: i64 = 7 * 86400;
const BLOCK_SECS: i64 = 5 * 3600;

#[derive(Clone, Debug, Default)]
pub struct Limit {
    /// "5-hour", "Weekly", "30-day", …
    pub label: String,
    pub used: f64,
    /// unix seconds; 0 = unknown
    pub resets_at: i64,
    /// percentage points used in the last 5 / 30 minutes
    pub d5: f64,
    pub d30: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Provider {
    pub name: &'static str,
    /// unix seconds of the newest event
    pub last: i64,
    /// tokens in the last 5 / 30 minutes
    pub tok5: u64,
    pub tok30: u64,
    pub today: u64,
    /// Claude: current 5 h block (start, tokens)
    pub block: Option<(i64, u64)>,
    pub limits: Vec<Limit>,
    pub plan: Option<String>,
    /// tokens per hour, oldest first, the last entry is the current hour
    pub hourly: [u64; 24],
}

impl Provider {
    pub fn rate5(&self) -> f64 {
        self.tok5 as f64 / 300.0
    }
    pub fn rate30(&self) -> f64 {
        self.tok30 as f64 / 1800.0
    }
    /// The limit closest to running out.
    pub fn worst(&self) -> Option<&Limit> {
        self.limits.iter().max_by(|a, b| a.used.total_cmp(&b.used))
    }
}

/// One agent session (a Claude Code conversation with its subagents, or a Codex thread).
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub provider: &'static str,
    /// the agent's own title, else the project folder
    pub title: String,
    pub project: String,
    pub model: Option<String>,
    pub branch: Option<String>,
    pub last: i64,
    pub tok5: u64,
    pub tok30: u64,
    /// tokens over the last 7 days
    pub total: u64,
}

impl Session {
    pub fn rate5(&self) -> f64 {
        self.tok5 as f64 / 300.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub now: i64,
    pub claude: Option<Provider>,
    pub codex: Option<Provider>,
    /// sessions with tokens in the last 30 minutes, newest first
    pub sessions: Vec<Session>,
}

impl Summary {
    pub fn providers(&self) -> impl Iterator<Item = &Provider> {
        self.claude.iter().chain(self.codex.iter())
    }
    pub fn rate5(&self) -> f64 {
        self.providers().map(|p| p.rate5()).sum()
    }
    pub fn rate30(&self) -> f64 {
        self.providers().map(|p| p.rate30()).sum()
    }
    /// Highest used percentage across all known limits.
    pub fn worst_pct(&self) -> Option<f64> {
        self.providers().filter_map(|p| p.worst().map(|l| l.used)).reduce(f64::max)
    }
}

static SUMMARY: Mutex<Option<Arc<Summary>>> = Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
static STARTED: OnceLock<()> = OnceLock::new();

pub fn summary() -> Option<Arc<Summary>> {
    SUMMARY.lock().ok()?.clone()
}

/// Bumped on every publish, so readers can tell whether to redraw.
pub fn generation() -> u64 {
    GEN.load(Ordering::Relaxed)
}

/// Stop reading while a game is fullscreen.
pub fn set_paused(on: bool) {
    PAUSED.store(on, Ordering::Relaxed);
}

/// Start the worker once. `claude_limit` = tokens per 5 h block, for a Claude percentage.
/// `notify` is called after each publish that changed something.
pub fn start(claude_limit: Option<u64>, notify: impl Fn() + Send + 'static) {
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("ai-usage".into())
        .spawn(move || {
            let mut st = State::new(claude_limit);
            let mut last_key = None;
            loop {
                if !PAUSED.load(Ordering::Relaxed) {
                    let s = st.pass();
                    let key = s.key();
                    *SUMMARY.lock().unwrap() = Some(Arc::new(s));
                    if Some(key) != last_key {
                        last_key = Some(key);
                        GEN.fetch_add(1, Ordering::Relaxed);
                        notify();
                    }
                }
                // on battery read less often
                std::thread::sleep(Duration::from_secs(if on_battery() { 30 } else { 10 }));
            }
        })
        .ok();
}

fn on_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).is_ok() && ps.ACLineStatus == 0 }
}

impl Summary {
    /// What the menu bar shows changes only with these.
    fn key(&self) -> (u64, u64, u64, i64, usize) {
        let pct = self.worst_pct().map(|p| (p * 10.0) as u64).unwrap_or(u64::MAX);
        let today: u64 = self.providers().map(|p| p.today).sum();
        (self.rate5().round() as u64, pct, today, self.now / 60, self.sessions.len())
    }
}

// ------------------------------------------------------------------ time

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Seconds east of UTC for the local zone (now).
fn utc_offset() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    let r = unsafe { GetTimeZoneInformation(&mut tz) };
    let bias = tz.Bias + if r == 2 { tz.DaylightBias } else { tz.StandardBias };
    -(bias as i64) * 60
}

/// "2026-11-03T12:59:55.662Z" → unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + se;
    // a "+07:00" style suffix
    if let Some(tz) = s.get(19..).map(|r| r.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit())) {
        if let (Some(sign), Some(hh), Some(mm)) = (tz.chars().next(), tz.get(1..3), tz.get(4..6)) {
            if sign == '+' || sign == '-' {
                let off = hh.parse::<i64>().ok()? * 3600 + mm.parse::<i64>().ok()? * 60;
                t -= if sign == '+' { off } else { -off };
            }
        }
    }
    Some(t)
}

// ------------------------------------------------------------------ reading

#[derive(Default)]
struct Tail {
    offset: u64,
}

/// A rate-limit report from Codex.
#[derive(Clone, Copy)]
struct RlSample {
    ts: i64,
    secondary: bool,
    used: f64,
    window_min: i64,
    resets_at: i64,
}

#[derive(Default)]
struct SessState {
    title: Option<String>,
    /// the user renamed it (custom-title beats ai-title)
    custom: bool,
    cwd: Option<String>,
    model: Option<String>,
    branch: Option<String>,
    /// (ts, tokens) of the last 30 minutes
    recent: Vec<(i64, u64)>,
    /// (ts, tokens) for the 7-day total
    total: Vec<(i64, u64)>,
    last: i64,
}

impl SessState {
    fn add(&mut self, ts: i64, tok: u64) {
        self.recent.push((ts, tok));
        self.total.push((ts, tok));
        self.last = self.last.max(ts);
    }
}

struct State {
    home: PathBuf,
    /// key "c:<sessionId>" (Claude) / "x:<thread id>" (Codex)
    sessions: HashMap<String, SessState>,
    /// Codex file -> its thread id (from the file's session_meta line)
    codex_ids: HashMap<PathBuf, String>,
    codex_titles: HashMap<String, String>,
    claude_limit: Option<u64>,
    tails: HashMap<PathBuf, Tail>,
    claude: Vec<(i64, u64)>,
    claude_ids: HashSet<String>,
    codex: Vec<(i64, u64)>,
    /// last cumulative total per Codex file, to drop repeated token_count lines
    codex_seen: HashMap<PathBuf, u64>,
    rl: Vec<RlSample>,
    codex_plan: Option<String>,
    have_claude: bool,
    have_codex: bool,
}

impl State {
    fn new(claude_limit: Option<u64>) -> State {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        State {
            home,
            sessions: HashMap::new(),
            codex_ids: HashMap::new(),
            codex_titles: HashMap::new(),
            claude_limit,
            tails: HashMap::new(),
            claude: Vec::new(),
            claude_ids: HashSet::new(),
            codex: Vec::new(),
            codex_seen: HashMap::new(),
            rl: Vec::new(),
            codex_plan: None,
            have_claude: false,
            have_codex: false,
        }
    }

    fn pass(&mut self) -> Summary {
        let now = now();
        let since = SystemTime::now() - Duration::from_secs(KEEP_SECS as u64);
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| self.home.join(".claude")).join("projects");
        let codex_dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| self.home.join(".codex")).join("sessions");
        self.have_claude = claude_dir.is_dir();
        self.have_codex = codex_dir.is_dir();

        let mut files = Vec::new();
        walk(&claude_dir, since, &mut files);
        for f in files.drain(..) {
            self.read(&f, true);
        }
        walk(&codex_dir, since, &mut files);
        for f in files.drain(..) {
            self.read(&f, false);
        }
        self.read_codex_titles(&codex_dir.with_file_name("session_index.jsonl"));

        let cut = now - KEEP_SECS;
        self.claude.retain(|e| e.0 >= cut);
        self.codex.retain(|e| e.0 >= cut);
        self.rl.retain(|e| e.ts >= cut);
        if self.claude_ids.len() > 200_000 {
            self.claude_ids.clear(); // old ids only matter for lines being re-read, which offsets prevent
        }
        self.claude.sort_unstable_by_key(|e| e.0);
        self.codex.sort_unstable_by_key(|e| e.0);
        self.rl.sort_unstable_by_key(|e| e.ts);

        let off = utc_offset();
        let mut s = Summary { now, ..Default::default() };
        self.sessions.retain(|_, st| st.last >= cut || st.last == 0);
        for (key, st) in self.sessions.iter_mut() {
            st.recent.retain(|e| e.0 >= now - 1800);
            st.total.retain(|e| e.0 >= cut);
            if st.recent.is_empty() {
                continue;
            }
            let codex = key.starts_with("x:");
            let project = st.cwd.as_deref().map(folder_name).unwrap_or_default();
            let title = if codex { self.codex_titles.get(&key[2..]).cloned() } else { st.title.clone() };
            s.sessions.push(Session {
                provider: if codex { "Codex" } else { "Claude Code" },
                title: title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| if project.is_empty() { "Session".into() } else { project.clone() }),
                project,
                model: st.model.clone(),
                branch: st.branch.clone(),
                last: st.last,
                tok5: st.recent.iter().filter(|e| e.0 >= now - 300).map(|e| e.1).sum(),
                tok30: st.recent.iter().map(|e| e.1).sum(),
                total: st.total.iter().map(|e| e.1).sum(),
            });
        }
        s.sessions.sort_by(|a, b| b.last.cmp(&a.last));
        if self.have_claude && !self.claude.is_empty() {
            let mut p = provider("Claude Code", &self.claude, now, off);
            p.block = current_block(&self.claude, now);
            if let (Some(lim), Some((start, tok))) = (self.claude_limit.filter(|l| *l > 0), p.block) {
                let pct = |t: u64| t as f64 * 100.0 / lim as f64;
                p.limits.push(Limit { label: "5-hour".into(), used: pct(tok), resets_at: start + BLOCK_SECS, d5: pct(p.tok5), d30: pct(p.tok30) });
            }
            s.claude = Some(p);
        }
        if self.have_codex && (!self.codex.is_empty() || !self.rl.is_empty()) {
            let mut p = provider("Codex", &self.codex, now, off);
            p.limits = codex_limits(&self.rl, now);
            p.plan = self.codex_plan.clone();
            s.codex = Some(p);
        }
        s
    }

    fn read(&mut self, path: &Path, claude: bool) {
        let Ok(mut f) = std::fs::File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let tail = self.tails.entry(path.to_path_buf()).or_default();
        if len < tail.offset {
            tail.offset = 0; // rewritten
        }
        if len == tail.offset || f.seek(SeekFrom::Start(tail.offset)).is_err() {
            return;
        }
        let mut r = BufReader::with_capacity(1 << 16, (&mut f).take(len - tail.offset));
        let mut line = Vec::new();
        let mut done = 0u64;
        let start = tail.offset;
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if line.last() != Some(&b'\n') {
                        break; // half-written; read it next pass
                    }
                    done += n as u64;
                    if let Ok(s) = std::str::from_utf8(&line) {
                        if claude {
                            self.claude_line(s);
                        } else {
                            self.codex_line(s, path);
                        }
                    }
                }
            }
        }
        if let Some(t) = self.tails.get_mut(path) {
            t.offset = start + done;
        }
    }

    fn claude_line(&mut self, s: &str) {
        if s.contains("\"type\":\"ai-title\"") || s.contains("\"type\":\"custom-title\"") {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                let custom = v["type"] == "custom-title";
                let t = if custom { v["customTitle"].as_str() } else { v["aiTitle"].as_str() };
                if let (Some(id), Some(t)) = (v["sessionId"].as_str(), t) {
                    let st = self.sessions.entry(format!("c:{id}")).or_default();
                    if custom || !st.custom {
                        st.title = Some(t.to_string());
                        st.custom |= custom;
                    }
                }
            }
            return;
        }
        if !s.contains("\"type\":\"assistant\"") || !s.contains("\"usage\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(s) else { return };
        if v["type"] != "assistant" {
            return;
        }
        let m = &v["message"];
        let u = &m["usage"];
        if !u.is_object() {
            return;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { return };
        if let Some(id) = m["id"].as_str() {
            let key = format!("{id}:{}", v["requestId"].as_str().unwrap_or(""));
            if !self.claude_ids.insert(key) {
                return;
            }
        }
        let g = |k: &str| u[k].as_u64().unwrap_or(0);
        let tok = g("input_tokens") + g("cache_creation_input_tokens") + g("output_tokens");
        if tok > 0 {
            self.claude.push((ts, tok));
            if let Some(id) = v["sessionId"].as_str() {
                let st = self.sessions.entry(format!("c:{id}")).or_default();
                st.add(ts, tok);
                // subagent lines carry the parent's session id: keep the main conversation's cwd / model
                if v["isSidechain"] != true || st.cwd.is_none() {
                    if let Some(c) = v["cwd"].as_str() {
                        st.cwd = Some(c.to_string());
                    }
                    if let Some(md) = m["model"].as_str().filter(|md| !md.starts_with('<')) {
                        st.model = Some(md.to_string());
                    }
                    st.branch = v["gitBranch"].as_str().filter(|b| !b.is_empty() && *b != "HEAD").map(String::from);
                }
            }
        }
    }

    fn codex_line(&mut self, s: &str, path: &Path) {
        if s.contains("\"type\":\"session_meta\"") || s.contains("\"type\":\"turn_context\"") {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                let p = &v["payload"];
                if v["type"] == "session_meta" {
                    if let Some(id) = p["id"].as_str() {
                        self.codex_ids.insert(path.to_path_buf(), id.to_string());
                        let st = self.sessions.entry(format!("x:{id}")).or_default();
                        st.cwd = p["cwd"].as_str().map(String::from);
                        st.branch = p["git"]["branch"].as_str().map(String::from);
                    }
                } else if v["type"] == "turn_context" {
                    if let Some(id) = self.codex_ids.get(path) {
                        let st = self.sessions.entry(format!("x:{id}")).or_default();
                        if let Some(md) = p["model"].as_str() {
                            st.model = Some(md.to_string());
                        }
                        if let Some(c) = p["cwd"].as_str() {
                            st.cwd = Some(c.to_string());
                        }
                    }
                }
            }
            return;
        }
        if !s.contains("\"token_count\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(s) else { return };
        let p = &v["payload"];
        if p["type"] != "token_count" {
            return;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { return };
        let info = &p["info"];
        if let Some(total) = info["total_token_usage"]["total_tokens"].as_u64() {
            let prev = self.codex_seen.insert(path.to_path_buf(), total);
            if prev != Some(total) {
                let u = &info["last_token_usage"];
                let g = |k: &str| u[k].as_u64().unwrap_or(0);
                let tok = g("input_tokens").saturating_sub(g("cached_input_tokens")) + g("output_tokens");
                if tok > 0 {
                    self.codex.push((ts, tok));
                    if let Some(id) = self.codex_ids.get(path) {
                        self.sessions.entry(format!("x:{id}")).or_default().add(ts, tok);
                    }
                }
            }
        }
        let rl = &p["rate_limits"];
        if let Some(plan) = rl["plan_type"].as_str() {
            self.codex_plan = Some(plan.to_string());
        }
        for (key, secondary) in [("primary", false), ("secondary", true)] {
            let w = &rl[key];
            if let Some(used) = w["used_percent"].as_f64() {
                self.rl.push(RlSample {
                    ts,
                    secondary,
                    used,
                    window_min: w["window_minutes"].as_i64().unwrap_or(0),
                    resets_at: w["resets_at"].as_i64().unwrap_or(0),
                });
            }
        }
    }
}

impl State {
    /// ~/.codex/session_index.jsonl: {"id", "thread_name", "updated_at"} per line, appended on rename.
    fn read_codex_titles(&mut self, path: &Path) {
        let Ok(mut f) = std::fs::File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let tail = self.tails.entry(path.to_path_buf()).or_default();
        if len < tail.offset {
            tail.offset = 0;
        }
        if len == tail.offset || f.seek(SeekFrom::Start(tail.offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if (&mut f).take(len - tail.offset).read_to_string(&mut buf).is_err() {
            return;
        }
        let done = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
        tail.offset += done as u64;
        for line in buf[..done].lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let (Some(id), Some(t)) = (v["id"].as_str(), v["thread_name"].as_str()) {
                    self.codex_titles.insert(id.to_string(), t.to_string());
                }
            }
        }
    }
}

/// Last component of a path ("D:\\Project\\MacDock" -> "MacDock").
fn folder_name(p: &str) -> String {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/']).next().unwrap_or(t).to_string()
}

/// "claude-opus-5-5" -> "Opus 5.5", "claude-sonnet-4-5-20250929" -> "Sonnet 4.5"; others unchanged.
pub fn model_name(m: &str) -> String {
    let Some(rest) = m.strip_prefix("claude-") else { return m.to_string() };
    let parts: Vec<&str> = rest.split('-').filter(|p| !(p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))).collect();
    let Some((name, ver)) = parts.split_first() else { return m.to_string() };
    let mut n = name.to_string();
    if let Some(c) = n.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    if ver.is_empty() { n } else { format!("{n} {}", ver.join(".")) }
}

/// *.jsonl under `dir` modified since `since`.
fn walk(dir: &Path, since: SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, since, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since) {
            out.push(p);
        }
    }
}

fn provider(name: &'static str, ev: &[(i64, u64)], now: i64, off: i64) -> Provider {
    let day_start = (now + off).div_euclid(86400) * 86400 - off;
    let hour = now.div_euclid(3600) * 3600;
    let mut p = Provider { name, last: ev.last().map(|e| e.0).unwrap_or(0), ..Default::default() };
    for &(ts, tok) in ev {
        if ts >= now - 300 {
            p.tok5 += tok;
        }
        if ts >= now - 1800 {
            p.tok30 += tok;
        }
        if ts >= day_start {
            p.today += tok;
        }
        let h = (hour - ts.div_euclid(3600) * 3600) / 3600;
        if (0..24).contains(&h) {
            p.hourly[23 - h as usize] += tok;
        }
    }
    p
}

/// ccusage-style 5 h blocks: one starts at the hour of the first message after the previous block ended.
fn current_block(ev: &[(i64, u64)], now: i64) -> Option<(i64, u64)> {
    let mut cur: Option<(i64, u64)> = None;
    for &(ts, tok) in ev {
        match cur {
            Some((start, ref mut t)) if ts < start + BLOCK_SECS => *t += tok,
            _ => cur = Some((ts.div_euclid(3600) * 3600, tok)),
        }
    }
    cur.filter(|(start, _)| now < start + BLOCK_SECS)
}

fn window_label(min: i64) -> String {
    match min {
        300 => "5-hour".into(),
        10080 => "Weekly".into(),
        43200 => "30-day".into(),
        m if m > 0 && m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}-hour", m / 60),
        m if m > 0 => format!("{m}-min"),
        _ => "Limit".into(),
    }
}

/// Latest report per window, plus how many points it moved in the last 5 / 30 minutes.
fn codex_limits(rl: &[RlSample], now: i64) -> Vec<Limit> {
    let mut out = Vec::new();
    for secondary in [false, true] {
        let s: Vec<&RlSample> = rl.iter().filter(|r| r.secondary == secondary).collect();
        let Some(last) = s.last() else { continue };
        if last.resets_at > 0 && last.resets_at <= now {
            // the window rolled over since Codex last reported: it starts from zero
            out.push(Limit { label: window_label(last.window_min), used: 0.0, resets_at: 0, d5: 0.0, d30: 0.0 });
            continue;
        }
        // value at `t`: newest report at or before t in the same window (a reset in between counts from 0)
        let at = |t: i64| -> f64 {
            match s.iter().rev().find(|r| r.ts <= t) {
                Some(r) if r.resets_at == last.resets_at || r.resets_at == 0 => r.used,
                Some(_) => 0.0,
                None => s.first().filter(|r| r.resets_at == last.resets_at).map(|r| r.used).unwrap_or(0.0),
            }
        };
        let d = |secs: i64| (last.used - at(now - secs)).max(0.0);
        out.push(Limit { label: window_label(last.window_min), used: last.used, resets_at: last.resets_at, d5: d(300), d30: d(1800) });
    }
    out
}

// ------------------------------------------------------------------ formatting

/// 950 → "950", 12_300 → "12.3k", 4_500_000 → "4.5M"
pub fn compact(n: f64) -> String {
    let a = n.abs();
    if a >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", n / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

/// 8100 s → "2h 15m", 300 → "5m", 3 days → "3d 4h"
pub fn span(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{}m", m.max(if s > 0 { 1 } else { 0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-11-03T12:59:55.662Z"), Some(1793710795));
        assert_eq!(parse_iso("2026-11-03T19:59:55+07:00"), Some(1793710795));
    }

    #[test]
    fn names() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_name("gpt-5.5-codex"), "gpt-5.5-codex");
        assert_eq!(folder_name("D:\\Project\\MacDock\\"), "MacDock");
    }

    #[test]
    fn blocks() {
        let h = 3600;
        let ev = [(10 * h + 120, 5), (12 * h, 5), (16 * h, 1)];
        // 10:00-15:00 block ends; 16:00 starts a new one
        assert_eq!(current_block(&ev, 16 * h + 10), Some((16 * h, 1)));
        assert_eq!(current_block(&ev[..2], 12 * h + 10), Some((10 * h, 10)));
        assert_eq!(current_block(&ev[..2], 15 * h + 1), None);
    }

    #[test]
    fn codex_deltas() {
        let r = |ts, used| RlSample { ts, secondary: false, used, window_min: 300, resets_at: 100_000 };
        let rl = [r(1000, 10.0), r(2000, 14.0), r(2700, 15.0), r(2900, 17.0)];
        let l = codex_limits(&rl, 3000);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].used, 17.0);
        assert_eq!(l[0].d5, 2.0); // at 2700 it was 15
        assert_eq!(l[0].d30, 7.0); // at 1200 it was 10
    }

    #[test]
    #[ignore] // reads the real logs: cargo test --release ai_real -- --ignored --nocapture
    fn ai_real() {
        let t = std::time::Instant::now();
        let mut st = State::new(None);
        let s = st.pass();
        println!("first pass {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let _ = st.pass();
        println!("second pass {:?}", t.elapsed());
        println!("{s:#?}");
    }
}
