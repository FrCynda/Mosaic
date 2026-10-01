//! Optional per-file created/accessed timestamps, kept out of the hot 32-byte `Rec` struct (see
//! `application/index.rs`) since Everything itself doesn't track them by default either. A
//! settings toggle (`index_timestamps`) turns this on; while off, nothing is stored and `dc:`/`da:`
//! predicates and Created/Accessed sort match nothing, exactly as before this feature existed.
//! Keyed by (drive, MFT record number) rather than path: stable across renames, unlike a path key.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

type Store = HashMap<(char, u32), (u32, u32)>; // (drive, record) -> (created_unix_s, accessed_unix_s)

static ENABLED: AtomicBool = AtomicBool::new(false);
static STORE: RwLock<Option<Store>> = RwLock::new(None);

fn path() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("datestore.bin")
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn the side-store on/off. Turning it on loads any data saved from an earlier session where it
/// was also on; turning it off just stops writes — it doesn't erase the file, so re-enabling later
/// doesn't need a full rebuild.
// ponytail: entries indexed from a cached index (not a fresh MFT scan) only get a created/accessed
// value once touched by a rename/create event after enabling; a full rebuild backfills the rest.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if on && STORE.read().is_none() {
        let saved: Store = std::fs::read(path()).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default();
        *STORE.write() = Some(saved);
    }
}

pub fn get(drive: char, record: u32) -> Option<(u32, u32)> {
    if !enabled() {
        return None;
    }
    STORE.read().as_ref()?.get(&(drive, record)).copied()
}

/// Record one entry's created/accessed time (Unix seconds; 0 = unknown, same convention as `Rec::mtime`).
/// A no-op while the toggle is off.
pub fn set(drive: char, record: u32, created: u32, accessed: u32) {
    if !enabled() {
        return;
    }
    STORE.write().get_or_insert_with(Default::default).insert((drive, record), (created, accessed));
}

/// Serializes any test that flips the global `ENABLED` toggle: `cargo test` runs tests in parallel
/// threads, and `ENABLED`/`STORE` are shared process-wide state, so two such tests (this module's own,
/// or `index.rs`'s datestore-integration test) would otherwise race and see each other's toggle.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn save() -> Result<(), String> {
    let g = STORE.read();
    let Some(store) = g.as_ref() else { return Ok(()) };
    let bytes = bincode::serialize(store).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(path().parent().unwrap());
    let tmp = path().with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_stores_and_returns_nothing() {
        let _g = test_lock();
        set_enabled(false);
        set('C', 5, 100, 200);
        assert_eq!(get('C', 5), None);
    }

    #[test]
    fn on_records_and_returns_what_was_set() {
        let _g = test_lock();
        set_enabled(true);
        set('Z', 42, 111, 222);
        assert_eq!(get('Z', 42), Some((111, 222)));
        assert_eq!(get('Z', 99), None);
        set_enabled(false);
        // toggling off stops new writes but doesn't drop what's already in memory this session
        set('Z', 42, 333, 444);
        assert_eq!(get('Z', 42), None); // reads are also gated by the toggle
    }
}
