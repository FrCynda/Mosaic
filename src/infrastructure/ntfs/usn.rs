//! Live index maintenance via the NTFS USN change journal. A background thread
//! tails the journal and applies create / delete / rename events to the shared
//! [`SearchIndex`], so results stay current without re-scanning the MFT.
//!
//! Best-effort: any journal failure (e.g. journal disabled) simply stops the
//! watcher, leaving the initial MFT snapshot intact.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::application::{Catalog, RawRecord, SearchIndex};

use super::mft::MftReader;
use super::volume::Volume;

// FSCTL control codes (CTL_CODE for the USN journal operations).
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;

// USN reason flags.
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_CLOSE: u32 = 0x8000_0000;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// Changes applied by all watchers, reported in the index status.
pub static CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Low 48 bits of an NTFS file reference are the MFT record number.
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[repr(C)]
#[derive(Clone, Copy)]
struct UsnJournalDataV0 {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
}

enum Op {
    Upsert(RawRecord),
    Delete(u64),
}

/// A live file change, resolved to a real path — what a USN event means to anything that cares
/// about file *content* (the content indexer), as opposed to just the filename index itself.
pub enum ChangeEvent {
    /// A file was created or modified; `mtime` is Unix seconds (0 if unknown).
    Upserted { path: String, mtime: i64, size: i64 },
    Deleted { path: String },
}

/// Where a watcher reports [`ChangeEvent`]s, for anything beyond the filename index itself to
/// consume (nothing does yet — see `contentindex.rs`). `None` costs nothing extra: the watcher
/// just skips the path lookups.
pub type ChangeSink = std::sync::mpsc::Sender<ChangeEvent>;

/// Handle to the running watcher thread; signals it to stop on drop.
pub struct UsnWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl UsnWatcher {
    /// Spawn a watcher tailing `drive`'s journal into `catalog`, starting from
    /// `start_usn` if given (the resume point after a cache catch-up), or the
    /// journal's current end otherwise. `sink`, if given, also gets a
    /// [`ChangeEvent`] per live file change (for anything beyond the filename
    /// index itself — nothing yet).
    pub fn spawn(drive: char, catalog: Arc<RwLock<Catalog>>, start_usn: Option<i64>, sink: Option<ChangeSink>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            if let Err(code) = run(drive, catalog, &stop_thread, start_usn, sink) {
                eprintln!("[usn] watcher for {drive}: stopped (OS error {code})");
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

/// A snapshot of a volume's USN journal position, used to resume after a load.
#[derive(Debug, Clone, Copy)]
pub struct JournalState {
    pub journal_id: u64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
}

/// Outcome of replaying journal changes onto a loaded index.
pub enum CatchUp {
    /// Caught up; resume the live watcher from `next_usn`.
    Caught { next_usn: i64 },
    /// The journal was recreated or the resume point was purged — rescan.
    Stale,
}

/// Query a volume's journal id and current end position.
pub fn journal_state(drive: char) -> Option<JournalState> {
    let volume = Volume::open(drive).ok()?;
    query_journal(&volume).ok()
}

/// Replay every change since `from_usn` onto `index`, bringing a loaded
/// snapshot up to date without re-reading the MFT.
pub fn catch_up(
    drive: char,
    snapshot_journal_id: u64,
    from_usn: i64,
    index: &mut SearchIndex,
    sink: Option<&ChangeSink>,
) -> CatchUp {
    // Prefer an MftReader so replayed changes can be enriched with the full
    // metadata the journal omits; fall back to a journal-only handle if the MFT
    // can't be opened, so a replay still beats a full rescan.
    let reader = MftReader::open(drive).ok();
    let fallback;
    let volume: &Volume = match reader.as_ref() {
        Some(r) => r.volume(),
        None => match Volume::open(drive) {
            Ok(v) => {
                fallback = v;
                &fallback
            }
            Err(_) => return CatchUp::Stale,
        },
    };
    let Ok(state) = query_journal(volume) else {
        return CatchUp::Stale;
    };
    // A new journal id or a purged resume point means we cannot trust a replay.
    if state.journal_id != snapshot_journal_id || from_usn < state.lowest_valid_usn {
        return CatchUp::Stale;
    }

    let mut cursor = from_usn;
    let mut out = vec![0u8; 64 * 1024];
    while cursor < state.next_usn {
        let request = ReadUsnJournalDataV0 {
            start_usn: cursor,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            usn_journal_id: snapshot_journal_id,
        };
        let returned = match unsafe {
            volume.device_io_control(
                FSCTL_READ_USN_JOURNAL,
                &request as *const _ as *const c_void,
                size_of::<ReadUsnJournalDataV0>() as u32,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
            )
        } {
            Ok(n) => n as usize,
            Err(_) => return CatchUp::Stale,
        };

        let next = i64::from_le_bytes(out[0..8].try_into().unwrap());
        if returned > 8 {
            for op in parse_records(&out[..returned], reader.as_ref()) {
                apply_op(index, op, sink); // the content index hears about changes made while we were off
            }
        }
        if next <= cursor {
            break; // no forward progress
        }
        cursor = next;
    }
    CatchUp::Caught { next_usn: cursor }
}

fn query_journal(volume: &Volume) -> Result<JournalState, u32> {
    let mut journal: UsnJournalDataV0 = unsafe { zeroed() };
    unsafe {
        volume.device_io_control(
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut journal as *mut _ as *mut c_void,
            size_of::<UsnJournalDataV0>() as u32,
        )?;
    }
    Ok(JournalState {
        journal_id: journal.journal_id,
        next_usn: journal.next_usn,
        lowest_valid_usn: journal.lowest_valid_usn,
    })
}

/// Apply one journal change to `idx`, and report it to `sink` (the content index) when there is one.
fn apply_op(idx: &mut SearchIndex, op: Op, sink: Option<&ChangeSink>) {
    match op {
        // resolved before deleting: the path can't be reconstructed once the link is gone
        Op::Delete(record_no) => {
            let path = sink.and_then(|_| idx.path_of(record_no as u32));
            idx.apply_delete(record_no);
            if let (Some(tx), Some(path)) = (sink, path) {
                let _ = tx.send(ChangeEvent::Deleted { path });
            }
        }
        // resolved after upserting: a newly-created record has no path until it's linked in
        Op::Upsert(raw) => {
            let want = sink.is_some() && !raw.is_dir;
            let (record_no, mtime, size) = (raw.record_no as u32, crate::application::index::filetime_to_unix_s(raw.modified_ft) as i64, raw.size.unwrap_or(0) as i64);
            idx.apply_upsert(raw);
            if want {
                if let (Some(tx), Some(path)) = (sink, idx.path_of(record_no)) {
                    let _ = tx.send(ChangeEvent::Upserted { path, mtime, size });
                }
            }
        }
    }
}

impl Drop for UsnWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(
    drive: char,
    catalog: Arc<RwLock<Catalog>>,
    stop: &AtomicBool,
    start_usn: Option<i64>,
    sink: Option<ChangeSink>,
) -> Result<(), u32> {
    // Keep an MftReader open for the watcher's lifetime so each live change can
    // be enriched with the timestamps/size the journal doesn't carry; share its
    // volume handle for the journal reads. Fall back to a journal-only handle if
    // the MFT can't be opened.
    let reader = MftReader::open(drive).ok();
    let fallback;
    let volume: &Volume = match reader.as_ref() {
        Some(r) => r.volume(),
        None => {
            fallback = Volume::open(drive).map_err(|_| 0u32)?;
            &fallback
        }
    };
    let state = query_journal(volume)?;

    let mut journal_id = state.journal_id;
    let mut next_usn = start_usn.unwrap_or(state.next_usn);
    let mut out = vec![0u8; 64 * 1024];
    // after a resync skipped changes, the index no longer matches any journal position: stop recording one, so
    // the cache keeps its last good position and the next start replays (or rescans) past the gap
    let mut gap = false;
    if let Some(idx) = catalog.write().volume_mut(drive) {
        idx.usn = Some(next_usn); // the loaded or scanned index is current up to here
    }

    // idle: the next read waits on the journal (a second at most, so the stop flag is still honoured soon) and
    // returns the moment a change lands, instead of polling it every 0.4 s
    let mut wait = false;
    while !stop.load(Ordering::Relaxed) {
        let request = ReadUsnJournalDataV0 {
            start_usn: next_usn,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 0,
            timeout: wait as u64, // seconds
            bytes_to_wait_for: wait as u64,
            usn_journal_id: journal_id,
        };
        let asked = Instant::now();

        let read = unsafe {
            volume.device_io_control(
                FSCTL_READ_USN_JOURNAL,
                &request as *const _ as *const c_void,
                size_of::<ReadUsnJournalDataV0>() as u32,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
            )
        };
        let returned = match read {
            Ok(n) => n as usize,
            // A read error must not kill the watcher — that would freeze live
            // updates for this volume until the app restarts. The usual cause is
            // the journal wrapping past our cursor after a churn burst
            // (ERROR_JOURNAL_ENTRY_DELETED) or being recreated; either way re-sync
            // to the current journal (accepting a bounded gap) and keep polling.
            Err(_) => {
                if let Ok(s) = query_journal(volume) {
                    journal_id = s.journal_id;
                    next_usn = s.next_usn;
                }
                if !gap {
                    gap = true;
                    if let Some(idx) = catalog.write().volume_mut(drive) {
                        idx.usn = None;
                    }
                }
                thread::sleep(Duration::from_millis(400));
                continue;
            }
        };

        let read_from = next_usn;
        next_usn = i64::from_le_bytes(out[0..8].try_into().unwrap());

        let ops = if returned > 8 {
            wait = false;
            parse_records(&out[..returned], reader.as_ref())
        } else {
            // Nothing new.
            // Once enough changes piled up, sort under a read lock and only compact under the write lock, so
            // searches (scrolling) never wait on the sort. Safe: this thread is the only one that mutates this volume.
            let order = catalog.read().volume(drive).filter(|i| i.rebuild_due()).map(|i| i.live_name_order());
            if let Some(order) = order {
                if let Some(idx) = catalog.write().volume_mut(drive) {
                    idx.rebuild(order);
                }
            }
            if wait && asked.elapsed() < Duration::from_millis(300) {
                thread::sleep(Duration::from_millis(400)); // the wait wasn't honoured (a volume that can't): poll
            }
            wait = true;
            Vec::new()
        };

        if !ops.is_empty() || (!gap && next_usn != read_from) {
            CHANGES.fetch_add(ops.len() as u64, Ordering::Relaxed); // per change: the cache saver weighs how many piled up
            let mut cat = catalog.write();
            if let Some(idx) = cat.volume_mut(drive) {
                for op in ops {
                    apply_op(idx, op, sink.as_ref());
                }
                if !gap {
                    idx.usn = Some(next_usn);
                }
            }
        }
    }
    Ok(())
}

/// Parse the USN_RECORD_V2 entries that follow the leading 8-byte next-USN. When
/// `reader` is available each upsert is enriched from the live MFT record with
/// the size and creation/modified/access times the journal does not carry.
fn parse_records(buf: &[u8], reader: Option<&MftReader>) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut p = 8usize;

    while p + 60 <= buf.len() {
        let rec_len = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        if rec_len < 60 || p + rec_len > buf.len() {
            break;
        }

        let file_ref = u64::from_le_bytes(buf[p + 8..p + 16].try_into().unwrap());
        let parent_ref = u64::from_le_bytes(buf[p + 16..p + 24].try_into().unwrap());
        let timestamp = i64::from_le_bytes(buf[p + 32..p + 40].try_into().unwrap());
        let reason = u32::from_le_bytes(buf[p + 40..p + 44].try_into().unwrap());
        let attrs = u32::from_le_bytes(buf[p + 52..p + 56].try_into().unwrap());
        let name_len = u16::from_le_bytes(buf[p + 56..p + 58].try_into().unwrap()) as usize;
        let name_off = u16::from_le_bytes(buf[p + 58..p + 60].try_into().unwrap()) as usize;

        // Act only on the coalesced final (close) event for each change.
        if reason & USN_REASON_CLOSE != 0 {
            let record_no = file_ref & RECORD_MASK;
            // High 16 bits of the reference are the record's sequence number,
            // used to detect MFT-record reuse when re-reading metadata.
            let seq = (file_ref >> 48) as u16;
            if reason & USN_REASON_FILE_DELETE != 0 {
                ops.push(Op::Delete(record_no));
            } else {
                let name_start = p + name_off;
                let name = if name_len > 0 && name_start + name_len <= buf.len() {
                    let units: Vec<u16> = buf[name_start..name_start + name_len]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&units)
                } else {
                    String::new()
                };
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                let raw = RawRecord {
                    record_no,
                    parent_no: parent_ref & RECORD_MASK,
                    seq: 0,
                    parent_seq: 0,
                    name,
                    is_dir,
                    // Journal-only fallbacks; overwritten by `enrich` when the
                    // MFT record can be read.
                    size: None,
                    modified_ft: timestamp as u64,
                    created_ft: 0,
                    accessed_ft: 0,
                    attributes: attrs,
                };
                ops.push(Op::Upsert(enrich(raw, seq, reader)));
            }
        }
        p += rec_len;
    }
    ops
}

/// Replace the journal's partial metadata with the authoritative values from the
/// live MFT record, keeping the journal's name/parent/is_dir for the changed
/// link. `seq` guards against the record having been reused since the event. A
/// failed or rejected read leaves the journal fallbacks in place.
fn enrich(mut raw: RawRecord, seq: u16, reader: Option<&MftReader>) -> RawRecord {
    if let Some(meta) = reader.and_then(|r| r.read_meta(raw.record_no, seq)) {
        raw.size = if raw.is_dir { None } else { meta.size };
        raw.modified_ft = meta.modified_ft;
        raw.created_ft = meta.created_ft;
        raw.accessed_ft = meta.accessed_ft;
        raw.attributes = meta.attributes;
    }
    raw
}
