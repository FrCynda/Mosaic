//! Persistence for the path-keyed caches (`hashcache`, `metadata`): a full snapshot plus an append-only log of the
//! changes since. A sync appends just what it changed (KBs); rewriting the whole snapshot (tens of MB) after every
//! sync — once a minute while such a query is up, on a disk where something always changes — wrote GBs a day.
//! The log is folded into a fresh snapshot once it outgrows a quarter of it.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

fn log_path(base: &Path) -> PathBuf {
    base.with_extension("log")
}

/// The snapshot with the log replayed over it. A torn last frame (a crash mid-append) is ignored.
pub fn load<V: DeserializeOwned>(base: &Path) -> HashMap<String, V> {
    let mut map: HashMap<String, V> = std::fs::read(base).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default();
    let log = std::fs::read(log_path(base)).unwrap_or_default();
    let mut r = &log[..];
    while let Ok(batch) = bincode::deserialize_from::<_, Vec<(String, Option<V>)>>(&mut r) {
        for (k, v) in batch {
            match v {
                Some(v) => map.insert(k, v),
                None => map.remove(&k),
            };
        }
    }
    map
}

/// Records `changed` keys (their value in `map` now, or removed if gone).
pub fn save<V: Serialize>(base: &Path, map: &HashMap<String, V>, changed: &[String]) -> Result<(), String> {
    if changed.is_empty() {
        return Ok(());
    }
    let frame: Vec<(&String, Option<&V>)> = changed.iter().map(|k| (k, map.get(k))).collect();
    let frame = bincode::serialize(&frame).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(base.parent().unwrap());
    let (log, size) = (log_path(base), |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len()));
    if size(base) > 0 && (size(&log) + frame.len() as u64) * 4 <= size(base) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
        return f.write_all(&frame).map_err(|e| e.to_string());
    }
    let tmp = base.with_extension("tmp");
    std::fs::write(&tmp, bincode::serialize(map).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, base).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(log);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn appends_changes_then_folds_them_into_the_snapshot() {
        let dir = std::env::temp_dir().join(format!("kvlog_test_{}", std::process::id()));
        let base = dir.join("store.bin");
        let mut m: std::collections::HashMap<String, u64> = (0..100).map(|i| (format!("k{i}"), i)).collect();
        super::save(&base, &m, &["k0".into()]).unwrap(); // no snapshot yet: written whole
        assert!(!super::log_path(&base).exists());
        m.insert("k1".into(), 111);
        m.remove("k2");
        super::save(&base, &m, &["k1".into(), "k2".into()]).unwrap(); // small: appended
        assert!(super::log_path(&base).exists());
        assert_eq!(super::load::<u64>(&base), m);
        let all: Vec<String> = m.keys().cloned().collect();
        super::save(&base, &m, &all).unwrap(); // log would outgrow the snapshot: folded
        assert!(!super::log_path(&base).exists());
        assert_eq!(super::load::<u64>(&base), m);
        let _ = std::fs::remove_dir_all(dir);
    }
}
