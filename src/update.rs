//! Update check: asks GitHub for the latest MacDock release at start and every 6 hours, on a worker thread.
//! A newer version than this build puts a dot on the menu bar logo and an "Update Available" item in its menu,
//! which offers to install it: the release's macdock.exe is downloaded, checked against its .sha256 and put in
//! place of the running exe, and MacDock restarts. Nothing is downloaded or installed without that click.

use std::sync::Mutex;
use std::time::Duration;

const REPO: &str = "GodrezJr2/MacDock";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub struct Release {
    pub version: String,
    pub url: String,
    /// the tag exactly as GitHub has it ("v0.5.0"), which the asset URLs use
    pub tag: String,
}

static LATEST: Mutex<Option<Release>> = Mutex::new(None);

/// An install in progress, and the outcome of the last one: Ok(version) or Err((version, page, why)).
type Outcome = Result<String, (String, String, String)>;
static INSTALLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static OUTCOME: Mutex<Option<Outcome>> = Mutex::new(None);

/// false while another install is running
pub fn begin_install() -> bool {
    !INSTALLING.swap(true, std::sync::atomic::Ordering::SeqCst)
}

pub fn finish_install(r: Outcome) {
    *OUTCOME.lock().unwrap() = Some(r);
}

/// The install stays "in progress" until its outcome is taken here, so a second click can't start another worker
/// in between; after a successful one it stays so for good, as MacDock is about to restart.
pub fn take_install() -> Option<Outcome> {
    let r = OUTCOME.lock().unwrap().take()?;
    if r.is_err() {
        INSTALLING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    Some(r)
}

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
        return Err(crate::i18n::fmt!("GitHub answered {status}", "GitHub 返回状态码 {status}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| crate::i18n::t("bad answer").to_string())?;
    let tag = v["tag_name"].as_str().unwrap_or_default().to_string();
    let url = v["html_url"].as_str().map(str::to_string).unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest"));
    let found = newer(&tag, CURRENT).then(|| (tag.trim_start_matches(['v', 'V']).to_string(), url));
    *LATEST.lock().unwrap() = found.clone().map(|(version, url)| Release { version, url, tag: tag.clone() });
    Ok(found)
}

/// The newer release found by the last check, if any: (version, page url).
pub fn available() -> Option<(String, String)> {
    LATEST.lock().unwrap().as_ref().map(|r| (r.version.clone(), r.url.clone()))
}

const UA: &str = "User-Agent: MacDock\r\n";

/// "macdock.exe" -> "macdock.exe.update-new" / "macdock.exe.update-old", next to it. Names only the updater uses,
/// so clean-up never touches a backup someone made by hand.
fn beside(exe: &std::path::Path, ext: &str) -> std::path::PathBuf {
    let mut p = exe.as_os_str().to_owned();
    p.push(ext);
    p.into()
}

/// Download release `version`'s macdock.exe, check it against the release's macdock.exe.sha256 and swap it in for
/// the running exe, which Windows lets us rename (not delete) while it runs: it becomes macdock.exe.update-old and the next
/// start removes it. The caller restarts MacDock. On an error the running exe is left where it was.
pub fn install(version: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tag = LATEST.lock().unwrap().as_ref().filter(|r| r.version == version).map(|r| r.tag.clone()).unwrap_or_else(|| format!("v{version}"));
    let base = format!("/{REPO}/releases/download/{tag}");
    let (st, bin) = crate::claude_oauth::get_bytes("github.com", &format!("{base}/macdock.exe"), UA, 64 << 20)?;
    if st != 200 {
        return Err(crate::i18n::fmt!("GitHub answered {st} for macdock.exe", "下载 macdock.exe 时 GitHub 返回状态码 {st}"));
    }
    let (st, sum) = crate::claude_oauth::get_bytes("github.com", &format!("{base}/macdock.exe.sha256"), UA, 4096)?;
    if st != 200 {
        return Err(crate::i18n::fmt!("GitHub answered {st} for macdock.exe.sha256", "下载 macdock.exe.sha256 时 GitHub 返回状态码 {st}"));
    }
    let want = String::from_utf8_lossy(&sum).split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if want.len() != 64 || hex(&sha256(&bin)) != want {
        return Err(crate::i18n::t("the download does not match its checksum").into());
    }
    if bin.len() < 64 * 1024 || !bin.starts_with(b"MZ") {
        return Err(crate::i18n::t("the download is not a program").into());
    }
    let (new, old) = (beside(&exe, ".update-new"), beside(&exe, ".update-old"));
    std::fs::write(&new, &bin).map_err(|e| crate::i18n::fmt!("could not write next to macdock.exe ({e})", "无法写入 macdock.exe 所在目录（{e}）"))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(crate::i18n::fmt!("could not move the running macdock.exe aside ({e})", "无法备份当前的 macdock.exe（{e}）"));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let back = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_file(&new);
        return Err(match back {
            Ok(()) => crate::i18n::fmt!("could not put the new macdock.exe in place ({e})", "无法替换 macdock.exe（{e}）"),
            Err(b) => crate::i18n::fmt!("could not put the new macdock.exe in place ({e}), nor the old one back ({b}): rename {} to macdock.exe", "替换 macdock.exe 失败（{e}），恢复旧版也失败（{b}）。请将 {} 重命名为 macdock.exe", old.display()),
        });
    }
    Ok(())
}

/// Remove the macdock.exe.update-old an update left behind. The old process may still be on its way out, so a few tries.
pub fn cleanup() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = beside(&exe, ".update-old");
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// SHA-256 (FIPS 180-4), for checking a download against the release's checksum file.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
        0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
        0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
        0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
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
    use super::{hex, newer, sha256};

    #[test]
    fn sha256_known_answers() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex(&sha256(long)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        assert_eq!(hex(&sha256(&vec![b'a'; 1_000_000])), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    /// Downloads the latest release and checks it, without swapping anything: cargo test -- --ignored live_download
    #[test]
    #[ignore]
    fn live_download() {
        let (_, body) = crate::claude_oauth::get("api.github.com", "/repos/GodrezJr2/MacDock/releases/latest", "Accept: application/vnd.github+json\r\nUser-Agent: MacDock\r\n").unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let tag = v["tag_name"].as_str().unwrap().to_string();
        let base = format!("/GodrezJr2/MacDock/releases/download/{tag}");
        let (st, bin) = crate::claude_oauth::get_bytes("github.com", &format!("{base}/macdock.exe"), super::UA, 64 << 20).unwrap();
        let (_, sum) = crate::claude_oauth::get_bytes("github.com", &format!("{base}/macdock.exe.sha256"), super::UA, 4096).unwrap();
        println!("{tag}: {st}, {} bytes", bin.len());
        assert_eq!(st, 200);
        assert!(String::from_utf8_lossy(&sum).starts_with(&hex(&sha256(&bin))));
    }

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
