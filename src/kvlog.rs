//! Persistence for the path-keyed caches (`hashcache`, `metadata`): a full snapshot plus an append-only log of the
//! changes since. A sync appends just what it changed (KBs); rewriting the whole snapshot (tens of MB) after every
//! sync — once a minute while such a query is up, on a disk where something always changes — wrote GBs a day.
//! The log is folded into a fresh snapshot once it outgrows a quarter of it.
//!
//! Both are zstd (each log frame its own): paths compress ~5x, so every append and fold writes a fifth of the bytes.
//! Earlier versions wrote plain bincode, still read; such a log is folded away at the next save.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

const ZSTD: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

fn log_path(base: &Path) -> PathBuf {
    base.with_extension("log")
}

/// Logs found torn at load: anything appended after the tear would never be read back, so the next save folds instead.
static TORN: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// `b` decompressed, if it's zstd. Of a torn last frame, whatever decoded before the tear.
fn unpack(b: Vec<u8>) -> Vec<u8> {
    if !b.starts_with(&ZSTD) {
        return b;
    }
    let mut out = Vec::new();
    let _ = zstd::Decoder::new(&b[..]).and_then(|mut d| d.read_to_end(&mut out));
    out
}

/// The snapshot with the log replayed over it. A torn last frame (a crash mid-append) is ignored.
pub fn load<V: DeserializeOwned>(base: &Path) -> HashMap<String, V> {
    let mut map: HashMap<String, V> = std::fs::read(base).ok().and_then(|b| bincode::deserialize(&unpack(b)).ok()).unwrap_or_default();
    let log = unpack(std::fs::read(log_path(base)).unwrap_or_default());
    let mut r = &log[..];
    while let Ok(batch) = bincode::deserialize_from::<_, Vec<(String, Option<V>)>>(&mut r) {
        for (k, v) in batch {
            match v {
                Some(v) => map.insert(k, v),
                None => map.remove(&k),
            };
        }
    }
    let raw = std::fs::read(log_path(base)).unwrap_or_default();
    if !r.is_empty() || raw.starts_with(&ZSTD) && zstd::decode_all(&raw[..]).is_err() {
        TORN.lock().unwrap_or_else(|e| e.into_inner()).push(base.to_path_buf());
    }
    map
}

/// Records `changed` keys (their value in `map` now, or removed if gone).
pub fn save<V: Serialize>(base: &Path, map: &HashMap<String, V>, changed: &[String]) -> Result<(), String> {
    if changed.is_empty() {
        return Ok(());
    }
    let frame: Vec<(&String, Option<&V>)> = changed.iter().map(|k| (k, map.get(k))).collect();
    let frame = zstd::encode_all(&bincode::serialize(&frame).map_err(|e| e.to_string())?[..], 3).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(base.parent().unwrap());
    let (log, size) = (log_path(base), |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len()));
    let mut head = [0u8; 4];
    let log_ok = size(&log) == 0 || std::fs::File::open(&log).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == ZSTD;
    let torn = TORN.lock().unwrap_or_else(|e| e.into_inner()).contains(&base.to_path_buf());
    if log_ok && !torn && size(base) > 0 && (size(&log) + frame.len() as u64) * 4 <= size(base) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
        return f.write_all(&frame).map_err(|e| e.to_string());
    }
    let tmp = base.with_extension("tmp");
    let z = zstd::encode_all(&bincode::serialize(map).map_err(|e| e.to_string())?[..], 3).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, z).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, base).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(log);
    TORN.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| b != base);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn appends_changes_then_folds_them_into_the_snapshot() {
        let dir = std::env::temp_dir().join(format!("kvlog_test_{}", std::process::id()));
        let base = dir.join("store.bin");
        let mut m: std::collections::HashMap<String, u64> = (0..100).map(|i| (format!("k{i}"), i)).collect();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&base, bincode::serialize(&m).unwrap()).unwrap(); // an uncompressed snapshot of earlier versions
        assert_eq!(super::load::<u64>(&base), m);
        std::fs::remove_file(&base).unwrap();
        super::save(&base, &m, &["k0".into()]).unwrap(); // no snapshot yet: written whole
        assert!(!super::log_path(&base).exists());
        m.insert("k1".into(), 111);
        m.remove("k2");
        super::save(&base, &m, &["k1".into(), "k2".into()]).unwrap(); // small: appended
        assert!(super::log_path(&base).exists());
        assert_eq!(super::load::<u64>(&base), m);
        let before = m.clone();
        m.insert("k3".into(), 333);
        super::save(&base, &m, &["k3".into()]).unwrap();
        assert_eq!(super::load::<u64>(&base), m);
        let log = std::fs::read(super::log_path(&base)).unwrap();
        std::fs::write(super::log_path(&base), &log[..log.len() - 1]).unwrap(); // a crash mid-append
        assert_eq!(super::load::<u64>(&base), before, "the torn frame dropped, the one before kept");
        let mut after = before.clone();
        after.insert("k4".into(), 444);
        super::save(&base, &after, &["k4".into()]).unwrap(); // after a tear: folded, not appended past it
        assert_eq!(super::load::<u64>(&base), after, "a change saved after a tear is read back");
        m.insert("k4".into(), 444);
        std::fs::write(super::log_path(&base), bincode::serialize(&vec![("k1", Some(111u64)), ("k2", None), ("k3", Some(333))]).unwrap()).unwrap();
        assert_eq!(super::load::<u64>(&base), m, "an uncompressed log of earlier versions");
        super::save(&base, &m, &["k3".into()]).unwrap(); // not appended to: folded
        assert!(!super::log_path(&base).exists());
        assert_eq!(super::load::<u64>(&base), m);
        let all: Vec<String> = m.keys().cloned().collect();
        super::save(&base, &m, &all).unwrap(); // log would outgrow the snapshot: folded
        assert!(!super::log_path(&base).exists());
        assert_eq!(super::load::<u64>(&base), m);
        let _ = std::fs::remove_dir_all(dir);
    }
}
