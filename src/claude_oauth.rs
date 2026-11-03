//! Claude's real 5-hour / weekly limits (opt-in, `ClaudeUsageOAuth` in config.json).
//!
//! Claude Code keeps its OAuth login in `%USERPROFILE%\.claude\.credentials.json`; the same token can read
//! `GET https://api.anthropic.com/api/oauth/usage` (what Claude Code's own /usage shows). The endpoint is not
//! documented, so any field may go away; everything here fails soft. The token is only ever sent to
//! api.anthropic.com. MacDock never refreshes it (that would rotate the refresh token under Claude Code):
//! when it has expired we wait until Claude Code runs and renews it.

use std::ffi::c_void;
use std::path::Path;

use serde_json::Value;
use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

pub struct Window {
    pub label: &'static str,
    pub window_min: i64,
    pub used: f64,
    pub resets_at: i64,
}

/// Read the token and ask for the usage. Err carries a short reason for the flyout.
pub fn fetch(home: &Path, now: i64) -> Result<Vec<Window>, String> {
    let creds = std::fs::read_to_string(home.join(".claude").join(".credentials.json")).map_err(|_| "no Claude Code login".to_string())?;
    let v: Value = serde_json::from_str(&creds).map_err(|_| "unreadable credentials".to_string())?;
    let o = &v["claudeAiOauth"];
    let token = o["accessToken"].as_str().ok_or("no OAuth token (API key login?)")?;
    if o["expiresAt"].as_i64().is_some_and(|ms| ms / 1000 <= now) {
        return Err("login expired; open Claude Code to renew it".into());
    }
    let (status, body) = get("api.anthropic.com", "/api/oauth/usage", &format!("Authorization: Bearer {token}\r\nanthropic-beta: oauth-2025-04-20\r\nAccept: application/json"))?;
    if status == 401 || status == 403 {
        return Err("login rejected; open Claude Code to renew it".into());
    }
    if status == 429 {
        return Err("rate limited, retrying later".into());
    }
    if status != 200 {
        return Err(format!("usage endpoint answered {status}"));
    }
    parse(&body)
}

pub fn parse(body: &str) -> Result<Vec<Window>, String> {
    let v: Value = serde_json::from_str(body).map_err(|_| "unexpected answer".to_string())?;
    let mut out = Vec::new();
    for (key, label, min) in [
        ("five_hour", "5-hour", 300),
        ("seven_day", "Weekly", 10080),
        ("seven_day_opus", "Weekly Opus", 10080),
        ("seven_day_sonnet", "Weekly Sonnet", 10080),
    ] {
        let w = &v[key];
        if let Some(used) = w["utilization"].as_f64() {
            let resets_at = w["resets_at"].as_str().and_then(crate::ai_usage::parse_iso).unwrap_or(0);
            out.push(Window { label, window_min: min, used, resets_at });
        }
    }
    if out.is_empty() {
        return Err("no limits in the answer".into());
    }
    Ok(out)
}

struct H(*mut c_void);
impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// HTTPS GET with WinHTTP (system proxy settings, 10 s timeouts). Returns (status, body).
fn get(host: &str, path: &str, headers: &str) -> Result<(u32, String), String> {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (host_w, path_w) = (wide(host), wide(path));
    let hdr: Vec<u16> = headers.encode_utf16().collect();
    unsafe {
        let s = H(WinHttpOpen(w!("MacDock"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if s.0.is_null() {
            return Err("network unavailable".into());
        }
        let _ = WinHttpSetTimeouts(s.0, 10_000, 10_000, 10_000, 10_000);
        let c = H(WinHttpConnect(s.0, PCWSTR(host_w.as_ptr()), 443, 0));
        if c.0.is_null() {
            return Err("can't reach Anthropic".into());
        }
        let r = H(WinHttpOpenRequest(c.0, w!("GET"), PCWSTR(path_w.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if r.0.is_null() {
            return Err("request failed".into());
        }
        WinHttpSendRequest(r.0, Some(&hdr), None, 0, 0, 0).map_err(|_| "offline".to_string())?;
        WinHttpReceiveResponse(r.0, std::ptr::null_mut()).map_err(|_| "no answer".to_string())?;
        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(r.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut c_void), &mut len, std::ptr::null_mut())
            .map_err(|_| "bad answer".to_string())?;
        let mut body = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let mut n = 0u32;
            if WinHttpReadData(r.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n).is_err() || n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n as usize]);
            if body.len() > 1 << 20 {
                break;
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage() {
        let b = r#"{"five_hour":{"utilization":7.0,"resets_at":"2026-11-03T18:09:59.970040+00:00"},
                    "seven_day":{"utilization":65.0,"resets_at":"2026-11-07T05:59:59.970059+00:00"},
                    "seven_day_opus":null}"#;
        let w = parse(b).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].label, w[0].used), ("5-hour", 7.0));
        assert_eq!(w[1].resets_at, crate::ai_usage::parse_iso("2026-11-07T05:59:59Z").unwrap());
    }

    #[test]
    #[ignore] // live call with Jay's login: cargo test --release oauth_live -- --ignored --nocapture
    fn oauth_live() {
        let home = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap());
        match fetch(&home, crate::ai_usage::now()) {
            Ok(ws) => ws.iter().for_each(|w| println!("{} {}% resets {}", w.label, w.used, w.resets_at)),
            Err(e) => panic!("{e}"),
        }
    }
}
