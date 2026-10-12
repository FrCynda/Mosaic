//! On-disk index cache. Each volume's index is persisted with the USN journal
//! position captured before its scan, so a later run can load instantly and
//! replay only the changes since.
//!
//! Layout: a fixed header, then the record array and the name arena exactly as they sit in memory (no per-field
//! encoding): loading is one read per array, saving one write per array.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::application::Rec;

/// Bumped whenever the on-disk layout changes; mismatches are ignored on load.
/// v2 added per-entry creation/access times and DOS attributes; v3 is the compact record + name arena layout.
const FORMAT_VERSION: u32 = 5; // 4: 28-byte Rec (48-bit size); 5: raw arrays, repr(C) Rec

/// A persisted per-volume index plus its journal resume point. Loads straight into the index's own arrays.
pub struct Snapshot {
    pub volume_serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    pub recs: Vec<Rec>,
    pub names: Vec<u8>,
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("AllTheThings").join("cache"))
}

/// Header: version, drive, serial, size of a record (u32 each), journal id, next usn, record count, name bytes.
const HEADER: usize = 4 * 4 + 8 * 4;

fn rec_bytes(recs: &[Rec]) -> &[u8] {
    // SAFETY: `Rec` is `repr(C)`, integers only, no padding (`rec_stays_28_bytes`): every byte is initialized.
    unsafe { std::slice::from_raw_parts(recs.as_ptr().cast(), std::mem::size_of_val(recs)) }
}

/// Load a volume's cached snapshot, or `None` if absent/unreadable/incompatible.
pub fn load(drive: char) -> Option<Snapshot> {
    load_from(&cache_dir()?.join(format!("{drive}.idx")), drive)
}

pub fn load_from(path: &std::path::Path, drive: char) -> Option<Snapshot> {
    let mut file = File::open(path).ok()?;
    let mut h = [0u8; HEADER];
    file.read_exact(&mut h).ok()?;
    let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
    if u32_at(0) != FORMAT_VERSION || u32_at(4) != drive as u32 || u32_at(12) as usize != std::mem::size_of::<Rec>() {
        return None;
    }
    let (n, names_len) = (u64_at(32) as usize, u64_at(40) as usize);
    // a damaged header must not ask for terabytes: the arrays are at most what the file holds
    let left = file.metadata().ok()?.len() as usize - HEADER;
    if n.checked_mul(std::mem::size_of::<Rec>())?.checked_add(names_len)? != left {
        return None;
    }
    let mut recs = vec![Rec::default(); n];
    // SAFETY: as in `rec_bytes`; any bit pattern is a valid `Rec`.
    file.read_exact(unsafe { std::slice::from_raw_parts_mut(recs.as_mut_ptr().cast::<u8>(), n * std::mem::size_of::<Rec>()) }).ok()?;
    let mut names = vec![0u8; names_len];
    file.read_exact(&mut names).ok()?;
    Some(Snapshot { volume_serial: u32_at(8), journal_id: u64_at(16), next_usn: u64_at(24) as i64, recs, names })
}

/// Persist a volume snapshot, replacing any previous one atomically.
pub fn save(
    drive: char,
    volume_serial: u32,
    journal_id: u64,
    next_usn: i64,
    (recs, names): (&[Rec], &[u8]),
) -> io::Result<()> {
    let dir = cache_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA not set"))?;
    fs::create_dir_all(&dir)?;
    save_to(&dir.join(format!("{drive}.idx")), drive, volume_serial, journal_id, next_usn, (recs, names))
}

pub fn save_to(final_path: &std::path::Path, drive: char, volume_serial: u32, journal_id: u64, next_usn: i64, (recs, names): (&[Rec], &[u8])) -> io::Result<()> {
    let mut h = Vec::with_capacity(HEADER);
    for v in [FORMAT_VERSION, drive as u32, volume_serial, std::mem::size_of::<Rec>() as u32] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    for v in [journal_id, next_usn as u64, recs.len() as u64, names.len() as u64] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    let tmp_path = final_path.with_extension("idx.tmp");
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&h)?;
        file.write_all(rec_bytes(recs))?;
        file.write_all(names)?;
    }
    fs::rename(&tmp_path, final_path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trips_and_rejects_a_truncated_file() {
        let dir = std::env::temp_dir().join(format!("mosaic-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("C.idx");
        let recs = vec![crate::application::Rec::default(); 3];
        super::save_to(&path, 'C', 7, 9, 11, (&recs, b"abc")).unwrap();
        let s = super::load_from(&path, 'C').unwrap();
        assert_eq!((s.volume_serial, s.journal_id, s.next_usn, s.recs.len(), s.names.as_slice()), (7, 9, 11, 3, &b"abc"[..]));
        assert!(super::load_from(&path, 'D').is_none(), "another drive's cache");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(super::load_from(&path, 'C').is_none(), "truncated");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
