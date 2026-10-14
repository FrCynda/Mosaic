//! Recent files and folders: those opened in Mosaic, newest first, then those Windows' Recent shortcuts point at
//! (Windows keeps none when "Show recently opened items" is off). Mosaic's own live in memory and are written once,
//! at exit, to `recent.txt`, and only when they changed: no write per file opened.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const MAX: usize = 200;

/// (folder?, path), newest first; `None` until read from disk.
static OWN: Mutex<Option<Vec<(bool, String)>>> = Mutex::new(None);
/// What this session changed: noted (newest last), forgotten, everything cleared. Merged into the file at exit,
/// so two windows don't undo each other's.
static NOTED: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());
static FORGOT: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CLEARED: AtomicBool = AtomicBool::new(false);
/// Windows' list, and when it was read.
static WIN: Mutex<Option<(Instant, Vec<(bool, String)>)>> = Mutex::new(None);
static WIN_BUSY: AtomicBool = AtomicBool::new(false);
/// Bumped whenever a list changes: whoever shows one knows to read it again.
static GEN: AtomicU64 = AtomicU64::new(1);

fn file() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("recent.txt")
}

fn read_file() -> Vec<(bool, String)> {
    std::fs::read_to_string(file()).unwrap_or_default().lines().filter_map(|l| l.split_once('\t')).map(|(k, p)| (k == "d", p.to_string())).collect()
}

fn own<R>(f: impl FnOnce(&mut Vec<(bool, String)>) -> R) -> R {
    let mut g = OWN.lock();
    f(g.get_or_insert_with(read_file))
}

pub fn generation() -> u64 {
    GEN.load(Ordering::Relaxed)
}

/// `path` (a folder: `dir`) was just opened.
pub fn note(path: &str, dir: bool) {
    let path = path.trim_end_matches('\\');
    if path.len() < 3 {
        return; // a drive's top: This PC lists those
    }
    let low = path.to_lowercase();
    own(|v| {
        if v.first().is_some_and(|f| f.1.to_lowercase() == low) {
            return;
        }
        v.retain(|e| e.1.to_lowercase() != low);
        v.insert(0, (dir, path.to_string()));
        v.truncate(MAX);
        NOTED.lock().push((dir, path.to_string()));
        GEN.fetch_add(1, Ordering::Relaxed);
    });
}

/// `path` off the recent lists (Mosaic's; a Windows shortcut stays Windows').
pub fn forget(path: &str) {
    let low = path.to_lowercase();
    own(|v| v.retain(|e| e.1.to_lowercase() != low));
    NOTED.lock().retain(|e| e.1.to_lowercase() != low);
    FORGOT.lock().push(low);
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// Mosaic's recent lists emptied.
pub fn clear() {
    own(Vec::clear);
    NOTED.lock().clear();
    CLEARED.store(true, Ordering::Relaxed);
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// At exit: this session's changes into `recent.txt` (as it is now: another window may have written it).
pub fn save() {
    let noted = std::mem::take(&mut *NOTED.lock());
    let forgot = std::mem::take(&mut *FORGOT.lock());
    let cleared = CLEARED.swap(false, Ordering::Relaxed);
    if noted.is_empty() && forgot.is_empty() && !cleared {
        return;
    }
    let mut v: Vec<(bool, String)> = noted.into_iter().rev().collect();
    if !cleared {
        v.extend(read_file().into_iter().filter(|e| !forgot.contains(&e.1.to_lowercase())));
    }
    let mut seen = HashSet::new();
    v.retain(|e| seen.insert(e.1.to_lowercase()));
    v.truncate(MAX);
    let text: String = v.iter().map(|(d, p)| format!("{}\t{p}\n", if *d { "d" } else { "f" })).collect();
    let _ = std::fs::write(file(), text);
}

/// Windows' Recent shortcuts, resolved: kept a minute, read again on a thread of its own (a COM call each).
fn windows() -> Vec<(bool, String)> {
    let known = WIN.lock().clone();
    if known.as_ref().is_none_or(|w| w.0.elapsed() > Duration::from_secs(60)) && !WIN_BUSY.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let at = PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join(r"Microsoft\Windows\Recent");
            let mut links: Vec<_> = std::fs::read_dir(at).into_iter().flatten().flatten().filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"))).filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
            links.sort_by(|a, b| b.0.cmp(&a.0));
            let v: Vec<(bool, String)> = links.iter().take(MAX).filter_map(|(_, l)| crate::shell::link_target(l.to_str()?)).filter_map(|t| Some((std::fs::metadata(&t).ok()?.is_dir(), t))).collect();
            let old = WIN.lock().replace((Instant::now(), v.clone()));
            if old.is_none_or(|o| o.1 != v) {
                GEN.fetch_add(1, Ordering::Relaxed);
            }
            WIN_BUSY.store(false, Ordering::SeqCst);
        });
    }
    known.map(|w| w.1).unwrap_or_default()
}

/// The `n` newest recent files (or `dirs`) that are still there: Mosaic's, then Windows'.
pub fn list(dirs: bool, n: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let own: Vec<(bool, String)> = own(|v| v.clone());
    own.into_iter()
        .chain(windows())
        .filter(|e| e.0 == dirs && seen.insert(e.1.to_lowercase()))
        .map(|e| e.1)
        // a share isn't asked (it may not answer); a local path that's gone is left out
        .filter(|p| p.starts_with("\\\\") || std::path::Path::new(p).exists())
        .take(n)
        .collect()
}
