//! On-disk index cache. Each volume's index is persisted with the USN journal
//! position captured before its scan, so a later run can load instantly and
//! replay only the changes since.
//!
//! Layout: a fixed header, then the record array and the name arena exactly as they sit in memory (no per-field
//! encoding), as one zstd stream: loading is one read per array, saving one write per array, at a fraction of the
//! bytes.
//!
//! Beside it, `<drive>.log`: the changes applied since, appended a few minutes' worth at a time ([`append`]) with
//! the journal position they bring it to, each frame zstd-compressed. Rewriting the whole 100+ MB snapshot instead,
//! at every stop and every 100k changes, was hundreds of MB of SSD writes a day; the log is folded into a new
//! snapshot once it outgrows half of it.
//!
//! Crash safety: the snapshot is synced before it replaces the old one, and its stream is checksummed; a log frame
//! is length-prefixed and checksummed, so a torn or damaged one ends the log there. Anything lost is replayed from
//! the USN journal, which is what the saved position points into.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::application::index::Change;
use crate::application::Rec;

/// Bumped whenever the on-disk layout changes; mismatches are ignored on load.
/// v2 added per-entry creation/access times and DOS attributes; v3 is the compact record + name arena layout.
const FORMAT_VERSION: u32 = 7; // 4: 28-byte Rec (48-bit size); 5: raw arrays, repr(C) Rec; 6: zstd; 7: 24-byte Rec

/// A persisted per-volume index plus its journal resume point. Loads straight into the index's own arrays.
pub struct Snapshot {
    pub volume_serial: u32,
    pub journal_id: u64,
    /// Where the journal replay resumes: past the snapshot and its log.
    pub next_usn: i64,
    pub recs: Vec<Rec>,
    pub names: Vec<u8>,
    /// Changes since the snapshot, to replay over it in order ([`changes`] of each, then `SearchIndex::replay`):
    /// compressed frames, already checked.
    pub log: Vec<Vec<u8>>,
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("AllTheThings").join("cache"))
}

/// Header: version, drive, serial, size of a record (u32 each), journal id, next usn, record count, name bytes.
const HEADER: usize = 4 * 4 + 8 * 4;

fn rec_bytes(recs: &[Rec]) -> &[u8] {
    // SAFETY: `Rec` is `repr(C)`, integers only, no padding (`rec_stays_24_bytes`): every byte is initialized.
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
    let mut z = zstd::Decoder::new(file).ok()?;
    // a damaged header must not abort the process on an impossible allocation: that's a cache miss
    let mut recs = Vec::new();
    recs.try_reserve_exact(n).ok()?;
    recs.resize(n, Rec::default());
    // SAFETY: as in `rec_bytes`; any bit pattern is a valid `Rec`.
    z.read_exact(unsafe { std::slice::from_raw_parts_mut(recs.as_mut_ptr().cast::<u8>(), n * std::mem::size_of::<Rec>()) }).ok()?;
    let mut names = Vec::new();
    names.try_reserve_exact(names_len).ok()?;
    names.resize(names_len, 0);
    z.read_exact(&mut names).ok()?;
    // reading on to the end checks the stream's checksum, and that it holds nothing more than the header said
    if z.read(&mut [0u8; 1]).ok()? != 0 {
        return None;
    }
    let (next_usn, log) = read_log(&path.with_extension("log"), u64_at(24) as i64);
    Some(Snapshot { volume_serial: u32_at(8), journal_id: u64_at(16), next_usn, recs, names, log })
}

/// The log's changes past `from` (the snapshot's position) and the position they reach. Frames from before the
/// snapshot (a stop between writing it and deleting the old log) are skipped; a torn last frame (a crash
/// mid-append) is cut off, so the next append doesn't land behind it.
fn read_log(path: &Path, from: i64) -> (i64, Vec<Vec<u8>>) {
    let bytes = fs::read(path).unwrap_or_default();
    let (mut at, mut usn, mut log) = (0, from, Vec::new());
    while at < bytes.len() {
        let len = bytes.get(at..at + 4).map_or(usize::MAX, |l| u32::from_le_bytes(l.try_into().unwrap()) as usize);
        let frame = bytes.get(at + 4..).and_then(|b| b.get(..len));
        let Some((frame, (to, _))) = frame.and_then(|z| Some((z, decode(z)?))) else {
            let _ = fs::OpenOptions::new().write(true).open(path).and_then(|f| f.set_len(at as u64));
            break;
        };
        at += 4 + len;
        if to > usn {
            usn = to;
            log.push(frame.to_vec());
        }
    }
    (usn, log)
}

fn decode(frame: &[u8]) -> Option<(i64, Vec<Change>)> {
    bincode::deserialize(&zstd::decode_all(frame).ok()?).ok()
}

/// The changes in a frame of [`Snapshot::log`].
pub fn changes(frame: &[u8]) -> Vec<Change> {
    decode(frame).map_or_else(Vec::new, |f| f.1) // checked as it was read
}

/// Add `changes`, which bring the saved index to journal position `usn`, to `drive`'s log. `false` when the
/// snapshot must be saved whole instead ([`save`]): there's none, or the log would outgrow half of it.
pub fn append(drive: char, usn: i64, changes: &[Change]) -> io::Result<bool> {
    let snap = cache_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA not set"))?.join(format!("{drive}.idx"));
    append_to(&snap, usn, changes)
}

pub(crate) fn append_to(snap: &Path, usn: i64, changes: &[Change]) -> io::Result<bool> {
    let z = compress(&bincode::serialize(&(usn, changes)).map_err(io::Error::other)?, 3)?;
    let frame = [&(z.len() as u32).to_le_bytes()[..], &z].concat();
    let log = snap.with_extension("log");
    let size = |p: &Path| fs::metadata(p).map_or(0, |m| m.len());
    if size(snap) == 0 || (size(&log) + frame.len() as u64) * 2 > size(snap) {
        return Ok(false);
    }
    // a failed write may leave part of the frame: the caller's whole save then supersedes it
    fs::OpenOptions::new().create(true).append(true).open(&log)?.write_all(&frame)?;
    Ok(true)
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
        // level 1: the whole index in a few hundred ms, under the read lock the watcher waits on
        let mut z = zstd::Encoder::new(file, 1)?;
        z.include_checksum(true)?;
        z.write_all(rec_bytes(recs))?;
        z.write_all(names)?;
        // on disk before it replaces the old one: a power cut can't leave a renamed but unwritten cache
        z.finish()?.sync_data()?;
    }
    fs::rename(&tmp_path, final_path)?;
    let _ = fs::remove_file(final_path.with_extension("log")); // in the snapshot now (left over: skipped on load)
    Ok(())
}

/// A checksummed zstd frame of `raw`.
fn compress(raw: &[u8], level: i32) -> io::Result<Vec<u8>> {
    let mut z = zstd::Encoder::new(Vec::new(), level)?;
    z.include_checksum(true)?;
    z.write_all(raw)?;
    z.finish()
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
