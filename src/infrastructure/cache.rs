//! On-disk index cache. Each volume's index is persisted with the USN journal
//! position captured before its scan, so a later run can load instantly and
//! replay only the changes since.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::application::Rec;

/// Bumped whenever the on-disk layout changes; mismatches are ignored on load.
/// v2 added per-entry creation/access times and DOS attributes; v3 is the compact record + name arena layout.
const FORMAT_VERSION: u32 = 3;

/// A persisted per-volume index plus its journal resume point. Loads straight into the index's own arrays.
#[derive(Deserialize)]
pub struct Snapshot {
    pub format_version: u32,
    pub drive: char,
    pub volume_serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    pub recs: Vec<Rec>,
    pub names: Vec<u8>,
}

/// The same layout, borrowed, so saving never copies the index.
#[derive(Serialize)]
struct SnapshotRef<'a> {
    format_version: u32,
    drive: char,
    volume_serial: u32,
    journal_id: u64,
    next_usn: i64,
    recs: &'a [Rec],
    names: &'a [u8],
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("AllTheThings").join("cache"))
}

/// Load a volume's cached snapshot, or `None` if absent/unreadable/incompatible.
pub fn load(drive: char) -> Option<Snapshot> {
    let path = cache_dir()?.join(format!("{drive}.idx"));
    let file = File::open(path).ok()?;
    let snapshot: Snapshot = bincode::deserialize_from(BufReader::new(file)).ok()?;
    if snapshot.format_version != FORMAT_VERSION || snapshot.drive != drive {
        return None;
    }
    Some(snapshot)
}

/// Persist a volume snapshot, replacing any previous one atomically.
pub fn save(
    drive: char,
    volume_serial: u32,
    journal_id: u64,
    next_usn: i64,
    (recs, names): (&[Rec], &[u8]),
) -> io::Result<()> {
    let snapshot = SnapshotRef {
        format_version: FORMAT_VERSION,
        drive,
        volume_serial,
        journal_id,
        next_usn,
        recs,
        names,
    };

    let dir = cache_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA not set"))?;
    fs::create_dir_all(&dir)?;

    let final_path = dir.join(format!("{drive}.idx"));
    let tmp_path = dir.join(format!("{drive}.idx.tmp"));
    {
        let file = File::create(&tmp_path)?;
        bincode::serialize_into(BufWriter::new(file), &snapshot).map_err(io::Error::other)?;
    }
    fs::rename(&tmp_path, &final_path)
}
