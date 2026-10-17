//! Updates: the newest release of FrCynda/Mosaic on GitHub, looked up with the curl.exe Windows ships, now and
//! every few hours. What the user chose about it (remind later, skip a version, never check) is kept in
//! `%LOCALAPPDATA%\Mosaic\update.json`. The window that shows it all is `native_update.rs`.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::{Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const API: &str = "https://api.github.com/repos/FrCynda/Mosaic/releases/latest";
const EVERY: Duration = Duration::from_secs(6 * 3600);
const NO_WINDOW: u32 = 0x0800_0000;

/// The update's colour: the tab strip's icon, the window's Update now.
pub const GREEN: u32 = 0x4bc882;

/// The release on offer.
#[derive(Clone, Default)]
pub struct Info {
    pub version: String,
    pub notes: String,
    pub installer: String, // MosaicSetup.exe's download link; empty: only the page
    pub page: String,
    pub date: String, // yyyy-mm-dd
    pub sha256: String, // GitHub's digest of the installer; empty if it gave none
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Prefs {
    pub check: bool,
    pub skip: String,       // the version not to be told about
    pub snooze_until: u64,  // unix seconds
    pub remind: usize,      // the last "remind me in" choice (index into REMIND)
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { check: true, skip: String::new(), snooze_until: 0, remind: 3 }
    }
}

/// "Remind me later" choices: (hours, the language key naming them).
pub const REMIND: [(u64, &str); 6] = [(1, "update.h1"), (4, "update.h4"), (12, "update.h12"), (24, "update.d1"), (72, "update.d3"), (168, "update.w1")];

/// How a download-and-install is going.
#[derive(Clone, PartialEq)]
pub enum Progress {
    Idle,
    Downloading,
    Installing, // the installer is up; it closes Mosaic itself and starts it again
    Failed(String),
}

/// What a check found.
pub enum Found {
    Newer,
    Current,
    Failed,
}

static PREFS: Mutex<Option<Prefs>> = Mutex::new(None);
static LATEST: Mutex<Option<Info>> = Mutex::new(None);
static PROGRESS: Mutex<Progress> = Mutex::new(Progress::Idle);

fn file() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("update.json")
}

pub fn prefs() -> Prefs {
    PREFS.lock().unwrap().get_or_insert_with(|| std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()).clone()
}

pub fn set_prefs(f: impl FnOnce(&mut Prefs)) {
    let mut p = prefs();
    f(&mut p);
    let _ = std::fs::create_dir_all(file().parent().unwrap());
    let _ = std::fs::write(file(), serde_json::to_string(&p).unwrap_or_default());
    *PREFS.lock().unwrap() = Some(p);
}

/// The running version. `MOSAIC_FAKE_VERSION` pretends to be another one, to see an update offered.
pub fn current() -> String {
    std::env::var("MOSAIC_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parts(v: &str) -> Vec<u64> {
    v.trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

/// Whether `a` is a later version than `b` ("1.0.10" over "1.0.9").
pub fn newer(a: &str, b: &str) -> bool {
    let (mut a, mut b) = (parts(a), parts(b));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The release to tell the user about: newer than this one, and not skipped or put off.
pub fn pending() -> Option<Info> {
    let p = prefs();
    let i = LATEST.lock().unwrap().clone()?;
    (p.check && newer(&i.version, &current()) && i.version != p.skip && now() >= p.snooze_until).then_some(i)
}

pub fn remind_in(hours: u64) {
    set_prefs(|p| p.snooze_until = now() + hours * 3600);
}

pub fn skip(version: &str) {
    set_prefs(|p| p.skip = version.to_string());
}

/// Release notes as plain lines: no markdown marks, bullets as •.
pub fn plain(md: &str) -> String {
    let link = regex::Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let lines: Vec<String> = md
        .lines()
        .map(|l| {
            let l = link.replace_all(l.trim_end(), "$1").replace("**", "").replace('`', "");
            let t = l.trim_start();
            match t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                Some(rest) => format!("• {rest}"),
                None => t.trim_start_matches('#').trim_start().to_string(),
            }
        })
        .collect();
    lines.join("\n").trim().to_string()
}

fn curl(args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new("curl.exe").args(["-sfL", "--max-time", "30", "-A", "Mosaic"]).args(args).creation_flags(NO_WINDOW).output().ok()?;
    o.status.success().then_some(o.stdout)
}

fn fetch() -> Option<Info> {
    let v: serde_json::Value = serde_json::from_slice(&curl(&["-H", "Accept: application/vnd.github+json", API])?).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let asset = v["assets"].as_array()?.iter().find(|a| a["name"] == "MosaicSetup.exe");
    let field = |k: &str| asset.and_then(|a| a[k].as_str()).unwrap_or_default().to_string();
    // only our own repository's release downloads are ever run; the version names a file, so only plain characters
    let installer = Some(field("browser_download_url")).filter(|u| u.starts_with("https://github.com/FrCynda/Mosaic/releases/download/")).unwrap_or_default();
    let version: String = s("tag_name").trim_start_matches(['v', 'V']).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    Some(Info { version, notes: plain(&s("body")), installer, page: s("html_url"), date: s("published_at").chars().take(10).collect(), sha256: field("digest").trim_start_matches("sha256:").to_lowercase() })
}

/// Look now; what was found is kept for [`pending`].
pub fn check() -> Found {
    match fetch() {
        Some(i) => {
            let newer = newer(&i.version, &current());
            *LATEST.lock().unwrap() = Some(i);
            if newer { Found::Newer } else { Found::Current }
        }
        None => Found::Failed,
    }
}

/// Look at once and then every few hours, telling window `main` (message `msg`, wParam 0) when the answer to
/// [`pending`] changed. Once per process.
pub fn start(main: isize, msg: u32) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(move || {
            let (mut seen, mut last) = (false, None::<SystemTime>);
            loop {
                if prefs().check && last.is_none_or(|t| t.elapsed().unwrap_or_default() >= EVERY) {
                    check();
                    last = Some(SystemTime::now());
                }
                let now_seen = pending().is_some();
                if now_seen != seen {
                    seen = now_seen;
                    post(main, msg, 0);
                }
                std::thread::sleep(Duration::from_secs(60)); // a remind-later running out brings the icon back
            }
        });
    });
}

pub fn post(main: isize, msg: u32, wp: usize) {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(main as _, msg, wp, 0) };
}

pub fn progress() -> Progress {
    PROGRESS.lock().unwrap().clone()
}

/// Download the installer and run it (one UAC prompt); it stops Mosaic, replaces it and starts it again.
pub fn install(i: &Info) {
    {
        let mut p = PROGRESS.lock().unwrap();
        if matches!(*p, Progress::Downloading | Progress::Installing) {
            return;
        }
        *p = Progress::Downloading;
    }
    let i = i.clone();
    std::thread::spawn(move || {
        let set = |p| *PROGRESS.lock().unwrap() = p;
        let exe = std::env::temp_dir().join(format!("MosaicSetup-{}.exe", i.version));
        let got = !i.installer.is_empty() && curl(&["-o", &exe.to_string_lossy(), &i.installer]).is_some() && exe.metadata().is_ok_and(|m| m.len() > 1_000_000);
        let hash = || {
            use sha2::{Digest, Sha256};
            std::fs::read(&exe).map(|b| Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()).unwrap_or_default()
        };
        if !got || (!i.sha256.is_empty() && hash() != i.sha256) {
            let _ = std::fs::remove_file(&exe);
            return set(Progress::Failed("download".into()));
        }
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let r = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                wide("runas").as_ptr(),
                wide(&exe.to_string_lossy()).as_ptr(),
                wide("/SILENT /SUPPRESSMSGBOXES /NORESTART /RELAUNCH=1").as_ptr(),
                std::ptr::null(),
                1,
            )
        };
        set(if r as usize > 32 { Progress::Installing } else { Progress::Failed("cancelled".into()) });
    });
}

/// Back to idle (a failure was read).
pub fn reset() {
    let mut p = PROGRESS.lock().unwrap();
    if matches!(*p, Progress::Failed(_)) {
        *p = Progress::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("1.0.1", "1.0.0"));
        assert!(newer("v1.0.10", "1.0.9"));
        assert!(newer("1.1", "1.0.5"));
        assert!(!newer("1.0.0", "1.0.0"));
        assert!(!newer("1.0", "1.0.0"));
        assert!(!newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn notes_lose_their_markdown() {
        assert_eq!(plain("## New\n- **Fast** [tabs](http://x)\n* `code`\n"), "New\n• Fast tabs\n• code");
    }
}
