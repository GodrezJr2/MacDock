//! Update check: asks GitHub for the latest MacDock release at start and every 6 hours, on a worker thread.
//! A newer version than this build puts a dot on the menu bar logo and an "Update Available" item in its menu,
//! which opens the release page. Nothing is downloaded or installed by itself.

use std::sync::Mutex;
use std::time::Duration;

const REPO: &str = "GodrezJr2/MacDock";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub struct Release {
    pub version: String,
    pub url: String,
}

static LATEST: Mutex<Option<Release>> = Mutex::new(None);

/// "v0.2.10" -> [0, 2, 10]
fn parse(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)).collect()
}

pub fn newer(a: &str, b: &str) -> bool {
    let (mut x, mut y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x > y
}

/// One request to the GitHub API. Ok(Some) = a newer release exists.
pub fn check() -> Result<Option<(String, String)>, String> {
    let (status, body) = crate::claude_oauth::get(
        "api.github.com",
        &format!("/repos/{REPO}/releases/latest"),
        "Accept: application/vnd.github+json\r\nUser-Agent: MacDock\r\n",
    )?;
    if status != 200 {
        return Err(format!("GitHub answered {status}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "bad answer".to_string())?;
    let tag = v["tag_name"].as_str().unwrap_or_default().to_string();
    let url = v["html_url"].as_str().map(str::to_string).unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest"));
    let found = newer(&tag, CURRENT).then(|| (tag.trim_start_matches('v').to_string(), url));
    *LATEST.lock().unwrap() = found.clone().map(|(version, url)| Release { version, url });
    Ok(found)
}

/// The newer release found by the last check, if any: (version, page url).
pub fn available() -> Option<(String, String)> {
    LATEST.lock().unwrap().as_ref().map(|r| (r.version.clone(), r.url.clone()))
}

pub fn start() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20)); // let start-up and the network settle
        loop {
            let _ = check();
            std::thread::sleep(Duration::from_secs(6 * 3600));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions() {
        assert!(newer("v0.2.0", "0.1.2"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("v0.1.2", "0.1.2"));
        assert!(!newer("0.1.1", "0.1.2"));
        assert!(!newer("", "0.1.2"));
    }

    /// Hits the real GitHub API: cargo test -- --ignored live_check
    #[test]
    #[ignore]
    fn live_check() {
        let r = super::check();
        println!("{:?}", r);
        assert!(r.is_ok());
    }
}
