//! The in-memory file index for a single volume: builds from a
//! [`VolumeEnumerator`], answers queries via a compiled [`Matcher`],
//! reconstructs full paths on demand, and accepts live mutations from the USN
//! change-journal watcher.
//!
//! Layout is built for memory: one 32-byte [`Rec`] per file plus every name in a single byte arena, stored
//! lowercased (what search and sorting read) with a little case data to restore the original spelling for
//! display. No per-file heap allocations, so ~2M files take ~120 MB instead of ~900 MB.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

// `Hit`/`SearchResult` are `Serialize` for the UI and `Deserialize` so a service
// client can decode them off the IPC pipe.

use crate::application::error::IndexResult;
use crate::application::indexer::{RawRecord, VolumeEnumerator};
use crate::application::search::{EntryView, Matcher, SearchOptions, SortKey};

/// NTFS reserves record 5 for the volume root directory.
const ROOT_RECORD: u32 = 5;
/// Seconds between the Windows (1601) and Unix (1970) epochs.
const FILETIME_UNIX_DIFF_S: i64 = 11_644_473_600;
/// Guard against malformed parent cycles when walking to the root. Set far above
/// any real NTFS nesting (a legitimate tree never approaches this) so the cap
/// only ever fires on a corrupt parent cycle â€” not on a genuinely deep path,
/// which would otherwise be silently truncated to a wrong-but-absolute path.
const MAX_PATH_DEPTH: usize = 4096;
/// "No entry" in the record map.
const NONE: u32 = u32::MAX;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// `Rec::bits`, low to high: lowercased name length (10 bits: an NTFS name is at most 255 UTF-16 units, so at most
/// 765 bytes lowercased), case kind (2; 0: the name is already all lowercase, no case data), attributes (13, see
/// [`ATTRS`]), then size bits 32..39 (7, see [`Rec::size`]).
const LEN_MASK: u32 = 0x3FF;
const CASE_SHIFT: u32 = 10;
/// The name part of `Rec::bits`, as [`push_name`] returns it.
const NAME_BITS: u32 = 0xFFF;
const ATTR_SHIFT: u32 = 12;
const SIZE_SHIFT: u32 = 25;
/// ASCII name: one bit per byte (set = uppercase) follows the lowercased bytes.
const CASE_MASK: u32 = 1;
/// Anything else: a u16 length and the original UTF-8 follow the lowercased bytes.
const CASE_FULL: u32 = 2;
/// The `FILE_ATTRIBUTE_*` flags kept, by bit: read-only, hidden, system, directory, archive, temporary, sparse,
/// reparse point, compressed, offline, not content indexed, encrypted, recall on data access. An entry with none of
/// them reads as NORMAL (0x80), as Windows reports it; the others don't occur on NTFS files or aren't used here.
const ATTRS: [u32; 13] = [0x1, 0x2, 0x4, 0x10, 0x20, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x40_0000];
const DIR_BIT: u32 = 1 << (ATTR_SHIFT + 3);

/// Retired + unsorted entries that trigger a [`SearchIndex::rebuild`]. Until then deletes and renames only retire
/// a slot and new names queue in a small unsorted tail, so the ~150 ms full name sort runs rarely, not on every
/// file created anywhere on the drive.
const REBUILD_AT: usize = 50_000;
/// Room kept for new names after a rebuild (bytes): about `REBUILD_AT` average names.
const NAME_ROOM: usize = REBUILD_AT * 32;

pub(crate) fn filetime_to_unix_s(ft: u64) -> u32 {
    if ft == 0 {
        0
    } else {
        ((ft / 10_000_000) as i64 - FILETIME_UNIX_DIFF_S).clamp(1, u32::MAX as i64) as u32
    }
}

/// One file or folder, 24 bytes (no 8-byte field, so no padding). Created/accessed times aren't kept (Everything doesn't index them by default
/// either); `dc:`/`da:` filters therefore match nothing.
/// `repr(C)` with no padding and only integer fields, so the cache stores the array's bytes as they are.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct Rec {
    /// Low 32 bits of the size. A folder holds the total of everything under it (see
    /// [`SearchIndex::sum_folders`]). See [`Rec::size`].
    size_lo: u32,
    /// MFT record number; `NONE` marks a retired slot (deleted, or renamed into a new slot).
    record: u32,
    /// MFT record number of the containing folder.
    parent: u32,
    /// Offset of the lowercased name (then its case data) in the name arena.
    name: u32,
    /// Last modified, Unix seconds; 0 = unknown.
    mtime: u32,
    /// Name length and case kind, attributes, high size bits: see [`LEN_MASK`].
    bits: u32,
}

impl Rec {
    /// The stored size is 39 bits: bytes below 256 GB, whole KB at and above (to 256 TB; the size column shows
    /// 0.1 GB there). All ones for unknown sizes.
    const NO_SIZE: u64 = (1 << 39) - 1;
    const IN_KB: u64 = 1 << 38;
    /// Bytes; `u64::MAX` for unknown sizes.
    fn size(&self) -> u64 {
        match ((self.bits >> SIZE_SHIFT) as u64) << 32 | self.size_lo as u64 {
            Self::NO_SIZE => u64::MAX,
            s if s & Self::IN_KB != 0 => (s & !Self::IN_KB) << 10,
            s => s,
        }
    }
    fn with_size(self, size: u64) -> Rec {
        let s = match size {
            u64::MAX => Self::NO_SIZE,
            s if s < Self::IN_KB => s,
            s => Self::IN_KB | (s.saturating_add(512) >> 10).min(Self::IN_KB - 2),
        };
        Rec { size_lo: s as u32, bits: self.bits & !(u32::MAX << SIZE_SHIFT) | ((s >> 32) as u32) << SIZE_SHIFT, ..self }
    }
    /// `FILE_ATTRIBUTE_*`.
    fn attrs(&self) -> u32 {
        let a = self.bits >> ATTR_SHIFT;
        match ATTRS.iter().enumerate().filter(|&(k, _)| a >> k & 1 != 0).fold(0, |m, (_, f)| m | f) {
            0 => 0x80, // FILE_ATTRIBUTE_NORMAL
            m => m,
        }
    }
    fn with_attrs(self, attrs: u32) -> Rec {
        let a = ATTRS.iter().enumerate().filter(|&(_, f)| attrs & f != 0).fold(0, |m, (k, _)| m | 1 << k);
        Rec { bits: self.bits & !(0x1FFF << ATTR_SHIFT) | a << ATTR_SHIFT, ..self }
    }
    /// Point at a name in the arena: the pair [`push_name`] returns.
    fn set_name(&mut self, (name, bits): (u32, u32)) {
        self.name = name;
        self.bits = self.bits & !NAME_BITS | bits;
    }
    fn case_kind(&self) -> u32 {
        self.bits >> CASE_SHIFT & 3
    }
    fn is_dir(&self) -> bool {
        self.bits & DIR_BIT != 0
    }
    /// What this entry adds to its folder's total: a file's size, a folder's own total.
    fn weight(&self) -> u64 {
        match self.size() {
            u64::MAX => 0,
            s => s,
        }
    }
    fn lower_len(&self) -> usize {
        (self.bits & LEN_MASK) as usize
    }
    fn modified_ms(&self) -> Option<i64> {
        (self.mtime != 0).then(|| self.mtime as i64 * 1000)
    }
}

/// One search result row, serialized to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub name: String,
    pub path: String,
    /// Size in bytes (a folder: everything under it); `-1` when unknown.
    pub size: i64,
    /// Last-modified Unix milliseconds; `0` when unknown.
    pub modified: i64,
    /// Creation Unix milliseconds; always `0` (not indexed).
    pub created: i64,
    /// Last-accessed Unix milliseconds; always `0` (not indexed).
    pub accessed: i64,
    /// Windows DOS file attributes bitmask.
    pub attributes: u32,
    #[serde(rename = "isDir")]
    pub is_dir: bool,
}

/// The full response for one search.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchResult {
    pub total: usize,
    #[serde(rename = "tookMs")]
    pub took_ms: u128,
    pub hits: Vec<Hit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The result set was truncated (more matched than were returned/scanned).
    /// Set by content search when the candidate pool hit its cap.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub capped: bool,
    /// A folder listing's version: the same one again is the same rows in the same order (nothing in or below the
    /// folder changed), so the GUI can keep what it shows. `None` for other searches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<u64>,
}

impl SearchResult {
    pub fn error(message: String, took_ms: u128) -> Self {
        Self {
            took_ms,
            error: Some(message),
            ..Self::default()
        }
    }
}

/// In-memory file index for a single volume.
pub struct SearchIndex {
    recs: Vec<Rec>,
    /// Every name, lowercased, each followed by its case data (see `CASE_*`).
    names: Vec<u8>,
    /// Retired slots in `recs` (their names stay in the arena until the next rebuild).
    dead: usize,
    /// `recs[..sorted]` is in name order (lowercased; retired slots included, skipped when read): laid out so by
    /// each rebuild, which saves a name-order array beside it (4 bytes an entry). Later slots are the unsorted tail
    /// of new names.
    sorted: usize,
    /// Bumped by every rebuild: positions from before it point at other entries.
    epoch: u64,
    /// MFT record number -> its (first) position in `recs`, or `NONE`.
    first: Vec<u32>,
    /// Further positions of hardlinked records: one MFT record with several names appears once per name, and
    /// the live update/delete paths must touch every link. Rare, hence a side map.
    more: HashMap<u32, Vec<u32>>,
    drive: char,
    /// The journal position this index has applied every change up to (set by the live watcher). The cache is
    /// saved with it, so the next start replays exactly what the saved copy is missing.
    pub usn: Option<i64>,
    /// Folder path (lowercased) -> its MFT record, for `dir_record`: each folder walked to once, not per listing.
    /// Checked on use (the record must still be that folder), so renames and deletes never serve a stale answer.
    dirs: parking_lot::Mutex<HashMap<String, u32>>,
    /// The last `TOUCHED` live changes, oldest first: (stamp, record, a folder it was or now is in, the slot it
    /// changed or retired or `NONE`). Lets a folder listing tell whether anything in or below its folder changed
    /// since it was ranked (`changed_under`), and the everything view take in just what changed (`patch_all`), so a
    /// busy disk elsewhere doesn't re-rank every entry to show the same files again.
    touched: std::collections::VecDeque<(u64, u32, u32, u32)>,
    /// Live changes applied so far (the last one's stamp).
    stamp: u64,
    /// Changes applied since the cache last saved, for its change log (`infrastructure::cache::append`). `None` when
    /// that log can't stand for them any more (too many piled up, or a save failed): the next save writes it all.
    /// Each with whether it added or moved the entry (see [`coalesce`]).
    unsaved: parking_lot::Mutex<Option<Vec<(Change, bool)>>>,
}

/// See [`SearchIndex::rebuild_plan`].
pub struct RebuildPlan {
    order: Vec<u32>,
    names: Vec<u8>,
    name_at: Vec<u32>,
}

/// Changes `SearchIndex::touched` remembers; a listing more changes old than that re-ranks.
const TOUCHED: usize = 4096;
/// Unsaved changes kept at most (~100 bytes each), should saves stop (a journal gap leaves no position to save).
const UNSAVED_MAX: usize = 200_000;

/// A live change as applied: the saved index plus its changes, replayed in order, is the index again.
#[derive(Serialize, Deserialize)]
pub enum Change {
    Upsert(RawRecord),
    Delete(u64),
}

/// `changes` (each with whether it must stay: see below) less the ones replay doesn't need: an update in place to an
/// entry (its size, times, name), followed later by an update to the same link (same parent and name) or by its
/// deletion. Most changes are such: a file written, then written again or deleted, within minutes. Replay ends the
/// same: the entry sits in the same folder either way, and folder totals are sums, so its old bytes come off and its
/// newest go on at the later change instead. What added an entry or moved it stays, at its place: what follows can
/// depend on where it is (the files made inside a new folder; its old folder deleted and that record reused).
fn coalesce(changes: Vec<(Change, bool)>) -> Vec<Change> {
    let (mut updated, mut deleted) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let mut kept: Vec<Change> = changes
        .into_iter()
        .rev()
        .filter(|(c, stays)| match c {
            Change::Delete(r) => {
                deleted.insert(*r);
                true
            }
            Change::Upsert(raw) => {
                let later = !updated.insert((raw.record_no, raw.parent_no, raw.name.clone())) || deleted.contains(&raw.record_no);
                *stays || !later
            }
        })
        .map(|(c, _)| c)
        .collect();
    kept.reverse();
    kept
}

/// Append `name` (at most 255 UTF-16 units) to the arena: lowercased bytes, then case data. Returns the
/// `Rec::name` offset and the name part of `Rec::bits`.
fn push_name(names: &mut Vec<u8>, name: &str) -> (u32, u32) {
    let off = names.len() as u32;
    if name.is_ascii() {
        names.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        if !name.bytes().any(|b| b.is_ascii_uppercase()) {
            return (off, name.len() as u32);
        }
        for chunk in name.as_bytes().chunks(8) {
            names.push(chunk.iter().enumerate().fold(0u8, |m, (k, b)| m | ((b.is_ascii_uppercase() as u8) << k)));
        }
        return (off, name.len() as u32 | CASE_MASK << CASE_SHIFT);
    }
    let lower = name.to_lowercase();
    names.extend_from_slice(lower.as_bytes());
    if lower == name {
        return (off, lower.len() as u32);
    }
    names.extend_from_slice(&(name.len() as u16).to_le_bytes());
    names.extend_from_slice(name.as_bytes());
    (off, lower.len() as u32 | CASE_FULL << CASE_SHIFT)
}

/// A record for `r`, minus its name; `None` for records that are never search hits or can't be stored.
fn new_rec(r: &RawRecord) -> Option<Rec> {
    if r.name.is_empty() || r.record_no == ROOT_RECORD as u64 || r.record_no >= NONE as u64 || r.parent_no >= NONE as u64 {
        return None;
    }
    // longer than NTFS allows: wouldn't fit `LEN_MASK`
    if r.name.encode_utf16().nth(255).is_some() {
        return None;
    }
    let size = if r.is_dir { 0 } else { r.size.unwrap_or(u64::MAX) };
    let attrs = if r.is_dir { r.attributes | FILE_ATTRIBUTE_DIRECTORY } else { r.attributes & !FILE_ATTRIBUTE_DIRECTORY };
    Some(
        Rec { record: r.record_no as u32, parent: r.parent_no as u32, mtime: filetime_to_unix_s(r.modified_ft), ..Rec::default() }
            .with_attrs(attrs)
            .with_size(size),
    )
}

/// Retire entries whose parent chain never reaches the drive root (leftovers of deleted folders, such as Windows
/// Update's pending deletes in WinSxS): they have no real path, so they'd list as drive-root items.
pub fn drop_orphans(recs: &mut [Rec]) {
    let n = recs.iter().filter(|r| r.record != NONE).map(|r| r.record as usize + 1).max().unwrap_or(0).max(ROOT_RECORD as usize + 1);
    let mut pos = vec![NONE; n];
    for (i, r) in recs.iter().enumerate().filter(|(_, r)| r.record != NONE) {
        pos[r.record as usize] = i as u32;
    }
    let mut state = vec![0u8; n]; // 0 unknown, 1 reaches the root, 2 doesn't
    state[ROOT_RECORD as usize] = 1;
    let mut walked = Vec::new();
    let mut bad = Vec::new();
    for (i, r) in recs.iter().enumerate().filter(|(_, r)| r.record != NONE) {
        walked.clear();
        let mut cur = r.parent as usize;
        let verdict = loop {
            match state.get(cur) {
                None => break 2,
                Some(&s) if s != 0 => break s,
                _ => {}
            }
            walked.push(cur);
            let p = pos[cur];
            if p == NONE || walked.len() > MAX_PATH_DEPTH {
                break 2;
            }
            cur = recs[p as usize].parent as usize;
        };
        for &w in &walked {
            state[w] = verdict;
        }
        if verdict == 2 {
            bad.push(i);
        }
    }
    for i in bad {
        recs[i].record = NONE;
    }
}

impl SearchIndex {
    /// Build the index by streaming every record from `enumerator`, publishing
    /// the running entry count into `progress` for the status bar.
    pub fn build_from<E: VolumeEnumerator>(enumerator: &mut E, progress: &AtomicUsize) -> IndexResult<Self> {
        let drive = enumerator.drive();
        let mut recs: Vec<Rec> = Vec::with_capacity(1 << 19);
        let mut names: Vec<u8> = Vec::with_capacity(16 << 20);
        let mut seqs: Vec<u16> = Vec::new(); // sequence number per record number
        let mut parent_seqs: Vec<u16> = Vec::new(); // parallel to `recs`
        enumerator.enumerate(&mut |r: RawRecord| {
            if let Some(mut rec) = new_rec(&r) {
                if seqs.len() <= rec.record as usize {
                    seqs.resize(rec.record as usize + 1, 0);
                }
                seqs[rec.record as usize] = r.seq;
                parent_seqs.push(r.parent_seq);
                crate::datestore::set(drive, rec.record, filetime_to_unix_s(r.created_ft), filetime_to_unix_s(r.accessed_ft));
                rec.set_name(push_name(&mut names, &r.name));
                recs.push(rec);
                if recs.len() & 0x3FFF == 0 {
                    progress.store(recs.len(), Ordering::Relaxed);
                }
            }
        })?;
        progress.store(recs.len(), Ordering::Relaxed);
        // a parent reference whose sequence differs from the folder now in that slot points at a deleted folder
        // whose record number was reused (e.g. leftovers showing up inside C:\Windows)
        for (rec, &ps) in recs.iter_mut().zip(&parent_seqs) {
            let live = seqs.get(rec.parent as usize).copied().unwrap_or(0);
            if ps != 0 && live != 0 && ps != live && rec.parent != ROOT_RECORD {
                rec.record = NONE;
            }
        }
        drop_orphans(&mut recs);
        recs.shrink_to_fit();
        names.shrink_to_fit();
        Ok(Self::from_parts(drive, recs, names))
    }

    /// Rebuild the record map and the sorted name view around stored records and names (retired slots dropped).
    pub fn from_parts(drive: char, recs: Vec<Rec>, names: Vec<u8>) -> Self {
        let dead = recs.iter().filter(|r| r.record == NONE).count();
        let mut idx = Self {
            recs,
            names,
            dead,
            sorted: 0,
            epoch: 0,
            first: Vec::new(),
            more: HashMap::new(),
            drive,
            usn: None,
            dirs: Default::default(),
            touched: Default::default(),
            stamp: 0,
            unsaved: parking_lot::Mutex::new(Some(Vec::new())),
        };
        let plan = idx.rebuild_plan();
        idx.rebuild(plan);
        idx
    }

    /// Records and name arena, for persistence to the on-disk cache (see [`Self::from_parts`]).
    pub fn parts(&self) -> (&[Rec], &[u8]) {
        (&self.recs, &self.names)
    }

    /// Live entries.
    pub fn len(&self) -> usize {
        self.recs.len() - self.dead
    }

    /// Whether position `i` exists (it may be retired: then it still shows its last name).
    pub fn contains(&self, i: u32) -> bool {
        (i as usize) < self.recs.len()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Live changes applied so far: what a cached listing notes, to ask `changed_under` later.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    fn touch(&mut self, record: u32, parent: u32, slot: u32) {
        self.stamp += 1;
        if self.touched.len() == TOUCHED {
            self.touched.pop_front();
        }
        self.touched.push_back((self.stamp, record, parent, slot));
    }

    fn log(&mut self, stays: bool, c: impl FnOnce() -> Change) {
        let u = self.unsaved.get_mut();
        match u {
            Some(v) if v.len() < UNSAVED_MAX => v.push((c(), stays)),
            _ => *u = None,
        }
    }

    /// The changes since the last save (left empty), less those a later one overrides, or `None` when only a whole
    /// save will do.
    pub fn take_unsaved(&self) -> Option<Vec<Change>> {
        self.unsaved.lock().as_mut().map(|v| coalesce(std::mem::take(v)))
    }

    /// The index was saved whole (`true`), or a save failed (`false`: the next one must be whole).
    pub fn saved_whole(&self, ok: bool) {
        *self.unsaved.lock() = ok.then(Vec::new);
    }

    /// Apply a saved index's change log, as when they happened. They're already saved, so not logged again.
    pub fn replay(&mut self, changes: Vec<Change>) {
        *self.unsaved.get_mut() = None;
        for c in changes {
            match c {
                Change::Upsert(raw) => self.apply_upsert(raw),
                Change::Delete(record_no) => self.apply_delete(record_no),
            }
        }
        *self.unsaved.get_mut() = Some(Vec::new());
    }

    /// The changes since `since`, newest first; `None` when too many changes ago to tell.
    fn changes_since(&self, since: u64) -> Option<impl Iterator<Item = &(u64, u32, u32, u32)>> {
        let known = self.stamp == since || self.touched.front().is_some_and(|&(s, ..)| s <= since + 1);
        known.then(|| self.touched.iter().rev().take_while(move |e| e.0 > since))
    }

    /// Whether a change since `since` could alter a listing of folders `dirs`: one of them, or anything at any
    /// depth below one (a folder's total size counts everything under it). True when too many changes ago to tell.
    pub fn changed_under(&self, dirs: &[u32], since: u64) -> bool {
        let Some(mut changes) = self.changes_since(since) else { return true };
        changes.any(|&(_, record, parent, _)| {
            if dirs.contains(&record) {
                return true;
            }
            let mut cur = parent;
            for _ in 0..MAX_PATH_DEPTH {
                if dirs.contains(&cur) {
                    return true;
                }
                match self.first.get(cur as usize).filter(|&&p| p != NONE && cur != ROOT_RECORD) {
                    Some(&p) => cur = self.recs[p as usize].parent,
                    None => break,
                }
            }
            false
        })
    }

    /// The folders a plain folder listing (`child:`, or Home's `parent:` terms) shows the contents of; `None` for
    /// any other search, or when one of them isn't there.
    pub fn listing_dirs(&self, matcher: &Matcher) -> Option<Vec<u32>> {
        if let Some(t) = matcher.only_child() {
            return self.dir_record(t, false).map(|d| vec![d]);
        }
        matcher.only_parents()?.into_iter().map(|t| self.dir_record(t, true)).collect()
    }

    fn is_live(&self, i: usize) -> bool {
        self.recs[i].record != NONE
    }

    fn retire(&mut self, i: usize) {
        self.recs[i].record = NONE;
        self.dead += 1;
    }

    pub fn rebuild_due(&self) -> bool {
        self.dead + (self.recs.len() - self.sorted) >= REBUILD_AT
    }

    /// Live positions sorted by name, for [`Self::rebuild`]. Takes `&self` so it can run under a read lock:
    /// sorting ~2M names takes ~150 ms, and a write lock would stall every search meanwhile.
    pub fn live_name_order(&self) -> Vec<u32> {
        let mut v: Vec<u32> = (0..self.recs.len() as u32).into_par_iter().filter(|&i| self.is_live(i as usize)).collect();
        v.par_sort_unstable_by(|&a, &b| self.lower(a as usize).cmp(self.lower(b as usize)));
        v
    }

    /// What [`Self::rebuild`] installs, worked out under a read lock (`&self`): the name order, and the new name
    /// arena with where each entry's name now sits. Sorting and laying out ~2.5M names takes a few hundred ms; under
    /// the write lock that would stall every search meanwhile.
    pub fn rebuild_plan(&self) -> RebuildPlan {
        let order = self.live_name_order();
        // names laid out in name order, each distinct one once: entries with the same name (`index.js`, `LICENSE`, a
        // folder of `.gitignore`s) share its bytes, ~40% of the arena on a typical drive. Equal names sit together in
        // `order`; `run` holds where each case spelling of the current lowercased name (`run_lower`) was stored.
        let mut name_at = vec![0u32; self.recs.len()];
        let mut names: Vec<u8> = Vec::with_capacity(self.names.len() / 2 + NAME_ROOM);
        let mut run: Vec<(usize, usize)> = Vec::new();
        let mut run_lower = 0..0;
        for &i in &order {
            let i = i as usize;
            let (s, n, l) = (self.recs[i].name as usize, self.stored_len(i), self.recs[i].lower_len());
            let b = &self.names[s..s + n];
            if names[run_lower.clone()] != b[..l] {
                run.clear();
            }
            let at = match run.iter().find(|&&(rs, rn)| names[rs..rs + rn] == *b) {
                Some(&(rs, _)) => rs,
                None => {
                    let at = names.len();
                    if run.is_empty() {
                        run_lower = at..at + l;
                    }
                    run.push((at, n));
                    names.extend_from_slice(b);
                    at
                }
            };
            name_at[i] = at as u32;
        }
        names.shrink_to(names.len() + NAME_ROOM);
        RebuildPlan { order, names, name_at }
    }

    /// Drop retired slots and their names, and install a [`Self::rebuild_plan`]: the entries laid out in name order.
    /// O(n) copying; the caller guarantees no mutation happened since the plan was made.
    pub fn rebuild(&mut self, plan: RebuildPlan) {
        let RebuildPlan { order, names, name_at } = plan;
        let live = self.len();
        // room for the entries that arrive until the next rebuild: a Vec growing by doubling would reserve a
        // second copy of the whole index (~100 MB for 2M files) on the first file created after startup
        let mut recs = Vec::with_capacity(live + REBUILD_AT);
        recs.extend(order.iter().map(|&i| Rec { name: name_at[i as usize], ..self.recs[i as usize] }));
        debug_assert_eq!(recs.len(), live);
        drop((name_at, order));
        self.recs = recs;
        self.names = names;
        self.dead = 0;
        self.sorted = live;
        self.epoch += 1;
        let max = self.recs.iter().map(|r| r.record).max().unwrap_or(0) as usize;
        self.first = vec![NONE; max + 1];
        self.more = HashMap::new();
        for i in 0..self.recs.len() {
            self.link_add(self.recs[i].record, i as u32);
        }
        self.sum_folders();
    }

    /// Every folder's total size, from scratch: each folder's own files summed, then that sum added to each of
    /// its ancestors. Runs at load and on every rebuild, which also squares away any drift the live updates
    /// (`add_up`) picked up from out-of-order journal events. Memory only: nothing is read from or written to disk.
    fn sum_folders(&mut self) {
        // each folder's own files, summed by position
        let mut own = vec![0u64; self.recs.len()];
        for r in self.recs.iter().filter(|r| r.record != NONE && !r.is_dir()) {
            if let Some(&p) = self.first.get(r.parent as usize).filter(|&&p| p != NONE) {
                own[p as usize] += r.weight();
            }
        }
        // then each one's sum climbs to its ancestors, summed exactly before it's stored (sizes from 256 GB up are
        // kept in KB: adding there one folder at a time would round at every step)
        let mut total = own.clone();
        for (p, &b) in own.iter().enumerate() {
            if b == 0 || !self.recs[p].is_dir() || self.recs[p].record == NONE {
                continue;
            }
            let mut parent = self.recs[p].parent;
            for _ in 0..MAX_PATH_DEPTH {
                match self.first.get(parent as usize) {
                    Some(&a) if parent != ROOT_RECORD && a != NONE && self.recs[a as usize].is_dir() => {
                        total[a as usize] += b;
                        parent = self.recs[a as usize].parent;
                    }
                    _ => break,
                }
            }
        }
        drop(own);
        for (r, &b) in self.recs.iter_mut().zip(&total) {
            if r.is_dir() {
                *r = r.with_size(b);
            }
        }
    }

    /// Add `delta` bytes to folder `parent` (an MFT record) and every folder above it.
    fn add_up(&mut self, mut parent: u32, delta: i64) {
        if delta == 0 {
            return;
        }
        for _ in 0..MAX_PATH_DEPTH {
            if parent == ROOT_RECORD {
                return;
            }
            let Some(&p) = self.first.get(parent as usize).filter(|&&p| p != NONE) else {
                return;
            };
            let r = &mut self.recs[p as usize];
            if !r.is_dir() {
                return;
            }
            *r = r.with_size((r.weight() as i64 + delta).max(0) as u64);
            parent = r.parent;
        }
    }

    pub fn drive(&self) -> char {
        self.drive
    }

    /// Lowercased name of entry `i`: what search and sorting compare.
    fn lower(&self, i: usize) -> &str {
        let r = &self.recs[i];
        let s = r.name as usize;
        // SAFETY: the arena only ever receives whole UTF-8 strings (`push_name`, `compact`) and every
        // record points at the start of one.
        unsafe { std::str::from_utf8_unchecked(&self.names[s..s + r.lower_len()]) }
    }

    /// Original-case name of entry `i`, for display.
    fn name(&self, i: usize) -> Cow<'_, str> {
        let r = &self.recs[i];
        let lower = self.lower(i);
        let tail = &self.names[r.name as usize + lower.len()..];
        match r.case_kind() {
            CASE_MASK => Cow::Owned(
                lower
                    .bytes()
                    .enumerate()
                    .map(|(k, b)| if tail[k / 8] >> (k % 8) & 1 == 1 { b.to_ascii_uppercase() as char } else { b as char })
                    .collect(),
            ),
            CASE_FULL => {
                let n = u16::from_le_bytes([tail[0], tail[1]]) as usize;
                // SAFETY: as in `lower`: copied from a `&str` by `push_name`.
                Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(&tail[2..2 + n]) })
            }
            _ => Cow::Borrowed(lower),
        }
    }

    /// Arena bytes entry `i`'s name occupies, case data included.
    fn stored_len(&self, i: usize) -> usize {
        let r = &self.recs[i];
        let n = r.lower_len();
        n + match r.case_kind() {
            CASE_MASK => n.div_ceil(8),
            CASE_FULL => {
                let at = r.name as usize + n;
                2 + u16::from_le_bytes([self.names[at], self.names[at + 1]]) as usize
            }
            _ => 0,
        }
    }

    // ---- MFT record number -> positions ----

    fn link_add(&mut self, record: u32, pos: u32) {
        let r = record as usize;
        if r >= self.first.len() {
            if r >= self.first.capacity() {
                self.first.reserve_exact(r + 1 - self.first.len() + REBUILD_AT); // not doubling: see `rebuild`
            }
            self.first.resize(r + 1, NONE);
        }
        if self.first[r] == NONE {
            self.first[r] = pos;
        } else {
            self.more.entry(record).or_default().push(pos);
        }
    }

    /// The absolute path of `record`'s first surviving link. Used to turn a USN event's bare
    /// record number back into a real path â€” e.g. for the content indexer to open and re-read.
    pub fn path_of(&self, record: u32) -> Option<String> {
        let &p = self.first.get(record as usize).filter(|&&p| p != NONE)?;
        Some(self.build_path(p as usize, false))
    }

    fn positions(&self, record: u32) -> Vec<u32> {
        let mut v = Vec::new();
        if let Some(&p) = self.first.get(record as usize).filter(|&&p| p != NONE) {
            v.push(p);
            v.extend(self.more.get(&record).into_iter().flatten().copied());
        }
        v
    }

    /// The position of `from` for `record` is now `to` (a rename moved the entry to a new slot).
    fn link_move(&mut self, record: u32, from: u32, to: u32) {
        let r = record as usize;
        if self.first.get(r) == Some(&from) {
            self.first[r] = to;
        } else if let Some(p) = self.more.get_mut(&record).and_then(|v| v.iter_mut().find(|p| **p == from)) {
            *p = to;
        }
    }

    /// Run a compiled search and return up to `opts.limit` rows with full paths.
    pub fn search(&self, opts: &SearchOptions, matcher: &Matcher) -> SearchResult {
        let order = self.ranked(opts, matcher);
        let hits = order.iter().skip(opts.offset).take(opts.limit).map(|&i| self.hit(i)).collect();
        SearchResult {
            total: order.len(),
            took_ms: 0,
            hits,
            error: None,
            capped: false,
            stamp: None,
        }
    }

    /// Every match as entry positions in final display order (sort, grouping and
    /// direction applied). Cheap to page through: see `Catalog`'s view cache.
    pub fn ranked(&self, opts: &SearchOptions, matcher: &Matcher) -> Vec<u32> {
        let all = matcher.matches_all();
        let shown = |r: &Rec| shown(r, opts);
        // a plain folder listing: compare parent record numbers instead of rebuilding every path
        let child_of = matcher.only_child().map(|t| self.dir_record(t, false));
        let folders: Option<Vec<u32>> = matcher.only_parents().map(|ts| ts.into_iter().filter_map(|t| self.dir_record(t, true)).collect());
        // Matched in one pass in entry order: records and names are read sequentially (the name order hops at
        // random through 100+ MB), and neighbours mostly share a folder, so a path search builds each folder's path
        // once per chunk. 0 = no match, 1 = match, 2 = a folder (or folder shortcut) when grouping folders first.
        const CHUNK: usize = 8192;
        let ff = opts.folders_first;
        let mut flags = vec![0u8; self.recs.len()];
        flags.par_chunks_mut(CHUNK).enumerate().for_each(|(c, out)| {
            let mut scratch = PathScratch::default();
            for (k, o) in out.iter_mut().enumerate() {
                let i = c * CHUNK + k;
                let r = &self.recs[i];
                let hit = r.record != NONE
                    && shown(r)
                    && match (&folders, child_of) {
                        (Some(set), _) => set.contains(&r.record),
                        (None, Some(dir)) => dir == Some(r.parent),
                        (None, None) => all || self.entry_matches(i, matcher, &mut scratch),
                    };
                if hit {
                    *o = if ff && self.folder_like(i) { 2 } else { 1 };
                }
            }
        });
        let keep = |i: u32| flags[i as usize] != 0;
        let (sort, asc) = (opts.sort, opts.ascending);
        let mut m: Vec<u32>;
        if matches!(sort, SortKey::Name) {
            // Name order without a full sort: filter the presorted view, then slot in the (small) sorted tail.
            m = (0..self.sorted as u32).into_par_iter().filter(|&i| keep(i)).collect();
            let mut tail: Vec<u32> = (self.sorted as u32..self.recs.len() as u32).into_par_iter().filter(|&i| keep(i)).collect();
            tail.par_sort_unstable_by(|&a, &b| self.lower(a as usize).cmp(self.lower(b as usize)));
            m = self.merge_by_name(m, tail);
        } else {
            m = (0..self.recs.len() as u32).into_par_iter().filter(|&i| keep(i)).collect();
        }
        if matches!(sort, SortKey::Name) {
        } else if matches!(sort, SortKey::Path) {
            // Key on a case-folded path so ordering matches the case-insensitive Name
            // column (and Everything/Explorer). Fold once per entry, not per comparison.
            let mut keyed: Vec<(u32, String)> = m.par_iter().map(|&i| (i, self.build_path(i as usize, true))).collect();
            keyed.par_sort_unstable_by(|a, b| a.1.cmp(&b.1));
            m = keyed.into_iter().map(|k| k.0).collect();
        } else if matches!(sort, SortKey::Ext) {
            // the key read once per entry, not twice per comparison; its first 8 bytes packed big-endian into an
            // integer that orders like the string, so most comparisons never touch the names
            let mut keyed: Vec<(u64, &str, u32)> = m
                .par_iter()
                .map(|&i| {
                    let e = self.ext(i as usize);
                    let mut b = [0u8; 8];
                    let n = e.len().min(8);
                    b[..n].copy_from_slice(&e.as_bytes()[..n]);
                    (u64::from_be_bytes(b), e, i)
                })
                .collect();
            keyed.par_sort_unstable();
            m = keyed.into_iter().map(|k| k.2).collect();
        } else {
            let mut keyed: Vec<(i64, u32)> = m.par_iter().map(|&i| (self.num_key(i as usize, sort), i)).collect();
            keyed.par_sort_unstable();
            m = keyed.into_iter().map(|k| k.1).collect();
        }
        if !asc {
            m.reverse();
        }
        if ff {
            // stable split: folders and folder shortcuts first, each group keeping the column order. O(n), so the
            // presorted name order still serves folders-first searches without a full sort.
            let mut dirs: Vec<u32> = m.par_iter().copied().filter(|&i| flags[i as usize] == 2).collect();
            dirs.par_extend(m.par_iter().copied().filter(|&i| flags[i as usize] == 1));
            m = dirs;
        }
        m
    }

    /// Merge name-sorted `tail` into name-sorted `head`: a binary search per tail entry, so a few thousand new
    /// names cost microseconds instead of comparing all ~2M.
    fn merge_by_name(&self, head: Vec<u32>, tail: Vec<u32>) -> Vec<u32> {
        if tail.is_empty() {
            return head;
        }
        let mut out = Vec::with_capacity(head.len() + tail.len());
        let mut start = 0;
        for t in tail {
            let key = self.lower(t as usize);
            let k = start + head[start..].partition_point(|&h| self.lower(h as usize) <= key);
            out.extend_from_slice(&head[start..k]);
            out.push(t);
            start = k;
        }
        out.extend_from_slice(&head[start..]);
        out
    }

    /// [`Self::ranked`] of a search matching everything by name (the empty search box, `order`) as it stood at stamp
    /// `since`, brought up to date with the changes since: the slots they touched leave, and the live ones come
    /// back in at their place. Two passes over the order instead of matching, filtering and splitting every entry.
    /// False when too many changes ago to tell (`order` is then untouched).
    pub fn patch_all(&self, order: &mut Vec<u32>, since: u64, opts: &SearchOptions) -> bool {
        let Some(changes) = self.changes_since(since) else { return false };
        let mut gone = vec![0u64; self.recs.len().div_ceil(64)];
        let mut back = Vec::new();
        for &(_, record, _, slot) in changes {
            for p in self.positions(record).into_iter().chain((slot != NONE).then_some(slot)) {
                let (w, b) = (p as usize / 64, 1u64 << (p % 64));
                if gone[w] & b == 0 {
                    gone[w] |= b;
                    if self.is_live(p as usize) && shown(&self.recs[p as usize], opts) {
                        back.push(p);
                    }
                }
            }
        }
        if gone.iter().all(|&w| w == 0) {
            return true;
        }
        order.retain(|&p| gone[p as usize / 64] & (1 << (p % 64)) == 0);
        // the order `ranked` gives: folders first (when grouped), then by name either way
        let ff = opts.folders_first;
        let cmp = |a: u32, b: u32| {
            let group = (ff && self.folder_like(b as usize)).cmp(&(ff && self.folder_like(a as usize)));
            let name = self.lower(a as usize).cmp(self.lower(b as usize));
            group.then(if opts.ascending { name } else { name.reverse() })
        };
        back.sort_unstable_by(|&a, &b| cmp(a, b));
        let mut out = Vec::with_capacity(order.len() + back.len());
        let mut start = 0;
        for t in back {
            let k = start + order[start..].partition_point(|&h| cmp(h, t).is_le());
            out.extend_from_slice(&order[start..k]);
            out.push(t);
            start = k;
        }
        out.extend_from_slice(&order[start..]);
        *order = out;
        true
    }

    /// Listed with the folders when they're grouped first: a folder, or a shortcut to one.
    fn folder_like(&self, i: usize) -> bool {
        self.recs[i].is_dir() || (self.is_lnk(i) && self.lnk_to_folder(i))
    }

    fn is_lnk(&self, i: usize) -> bool {
        !self.recs[i].is_dir() && self.lower(i).ends_with(".lnk")
    }

    /// Whether a .lnk points at a folder, read from the link's header once per (file, modified time).
    // ponytail: cache never drops deleted links (a few bytes each); prune on rescan if it ever matters
    fn lnk_to_folder(&self, i: usize) -> bool {
        static CACHE: std::sync::OnceLock<parking_lot::Mutex<HashMap<(char, u32, u32), bool>>> = std::sync::OnceLock::new();
        let r = &self.recs[i];
        let key = (self.drive, r.record, r.mtime);
        let cache = CACHE.get_or_init(Default::default);
        if let Some(&v) = cache.lock().get(&key) {
            return v;
        }
        // never open an online-only cloud file: reading it would download it
        let v = r.attrs() & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) == 0 && {
            use std::io::Read;
            let mut head = [0u8; 0x1C];
            std::fs::File::open(self.build_path(i, false)).and_then(|mut f| f.read_exact(&mut head)).is_ok()
                && lnk_header_is_folder(&head)
        };
        cache.lock().insert(key, v);
        v
    }

    /// Sort key of a numeric column. Name, Ext and Path are ranked separately.
    fn num_key(&self, i: usize, sort: SortKey) -> i64 {
        let r = &self.recs[i];
        match sort {
            SortKey::Size => size_key(r),
            SortKey::Modified => r.mtime as i64,
            SortKey::Attributes => r.attrs() as i64,
            SortKey::Created => self.created_accessed(i).0 as i64,
            SortKey::Accessed => self.created_accessed(i).1 as i64,
            SortKey::Name | SortKey::Ext | SortKey::Path => 0,
        }
    }

    /// (created, accessed) Unix seconds for entry `i`, from the opt-in side-store (`crate::datestore`);
    /// `(0, 0)` while it's off or the entry predates enabling it.
    fn created_accessed(&self, i: usize) -> (u32, u32) {
        crate::datestore::get(self.drive, self.recs[i].record).unwrap_or_default()
    }

    /// Ext sort key: blank for directories (whose Ext cell renders empty).
    fn ext(&self, i: usize) -> &str {
        if self.recs[i].is_dir() {
            ""
        } else {
            ext_of(self.lower(i))
        }
    }

    /// The result row for one entry position (as returned by [`Self::ranked`]).
    pub fn hit(&self, i: u32) -> Hit {
        let i = i as usize;
        let r = &self.recs[i];
        let (created, accessed) = self.created_accessed(i);
        Hit {
            name: self.name(i).into_owned(),
            path: self.build_path(i, false),
            size: size_key(r),
            modified: r.modified_ms().unwrap_or(0),
            created: created as i64,
            accessed: accessed as i64,
            attributes: r.attrs(),
            is_dir: r.is_dir(),
        }
    }

    /// MFT record of the folder at `path` (`c:`, any case; `c:` is the drive root), found by walking down from
    /// the root one name at a time. `None` when it is on another drive or doesn't exist.
    fn dir_record(&self, path: &str, last_any: bool) -> Option<u32> {
        let path = path.to_lowercase();
        let (drive, rest) = path.split_once(':')?;
        if drive.len() != 1 || !drive.starts_with(self.drive.to_ascii_lowercase()) {
            return None;
        }
        let parts: Vec<&str> = rest.split('\\').filter(|p| !p.is_empty()).collect();
        // start below the deepest folder already known: a full scan per *new* level only, usually just the last
        let prefix = |k: usize| format!("{drive}:\\{}", parts[..k].join("\\"));
        let (mut cur, mut from) = (ROOT_RECORD, 0);
        for k in (1..=parts.len()).rev() {
            let p = prefix(k);
            let known = self.dirs.lock().get(&p).copied();
            if let Some(r) = known.filter(|&r| self.is_folder_at(r, &p)) {
                (cur, from) = (r, k);
                break;
            }
        }
        for (n, part) in parts.iter().enumerate().skip(from) {
            let pos = (0..self.recs.len())
                .into_par_iter()
                .find_any(|&i| self.is_live(i) && self.recs[i].parent == cur && (self.recs[i].is_dir() || (last_any && n + 1 == parts.len())) && self.lower(i) == *part)?;
            cur = self.recs[pos].record;
            if self.recs[pos].is_dir() {
                let mut dirs = self.dirs.lock();
                if dirs.len() > 50_000 {
                    dirs.clear();
                }
                dirs.insert(prefix(n + 1), cur);
            }
        }
        Some(cur)
    }

    /// Whether record `r` is still the live folder at `path` (lowercased).
    fn is_folder_at(&self, r: u32, path: &str) -> bool {
        self.first.get(r as usize).is_some_and(|&p| p != NONE && self.is_live(p as usize) && self.recs[p as usize].is_dir() && self.build_path(p as usize, true) == path)
    }

    fn entry_matches(&self, i: usize, matcher: &Matcher, s: &mut PathScratch) -> bool {
        let r = &self.recs[i];
        let lower = self.lower(i);
        // original-case strings only for match-case searches: everything else reads the lowercased arena
        let case = matcher.match_case();
        let name = if case { self.name(i) } else { Cow::Borrowed(lower) };
        s.path.clear();
        s.path_lower.clear();
        if matcher.needs_path() {
            // the folder's path (cached), then this entry's name: the same string `build_path` makes
            let parent = r.parent;
            let (dir, dir_lower) = s.dirs.entry(parent).or_insert_with(|| match self.first.get(parent as usize) {
                Some(&p) if p != NONE && parent != ROOT_RECORD => {
                    (if case { self.build_path(p as usize, false) } else { String::new() }, self.build_path(p as usize, true))
                }
                _ => (format!("{}:", self.drive), format!("{}:", self.drive.to_ascii_lowercase())),
            });
            if case {
                s.path.push_str(dir);
                s.path.push('\\');
                s.path.push_str(&name);
            }
            s.path_lower.push_str(dir_lower);
            s.path_lower.push('\\');
            s.path_lower.push_str(lower);
        }
        let (created, accessed) = self.created_accessed(i);
        matcher.eval(&EntryView {
            name: &name,
            name_lower: lower,
            path: &s.path,
            path_lower: &s.path_lower,
            is_dir: r.is_dir(),
            size: (r.size() != u64::MAX).then_some(r.size()),
            modified_ms: r.modified_ms(),
            created_ms: (created != 0).then_some(created as i64 * 1000),
            accessed_ms: (accessed != 0).then_some(accessed as i64 * 1000),
            attributes: r.attrs(),
        })
    }

    /// Insert or replace an entry (USN create / rename / metadata change).
    pub fn apply_upsert(&mut self, raw: RawRecord) {
        let Some(mut rec) = new_rec(&raw) else {
            return;
        };
        crate::datestore::set(self.drive, rec.record, filetime_to_unix_s(raw.created_ft), filetime_to_unix_s(raw.accessed_ft));
        let positions = self.positions(rec.record);
        let target = match positions.as_slice() {
            // Unknown record â€” a new file or the first link.
            [] => None,
            // Single-link file: replace that one row in place. A USN event can't
            // say which old (parent, name) it replaced, but for a sole link a
            // rename, a move, and a metadata change all update the same row â€” so
            // match by record only (preserving the pre-hardlink behaviour).
            [p] => Some(*p as usize),
            // Hardlinked record: the event names one link, so match it by parent
            // + name and replace that link; an unmatched name is a newly-added
            // hardlink, appended rather than clobbering a sibling.
            many => many
                .iter()
                .map(|&p| p as usize)
                .find(|&p| self.recs[p].parent == rec.parent && self.name(p) == raw.name.as_str()),
        };
        // folder totals: take the old entry's bytes off its old folders, put the new ones on its new folders. A
        // folder event carries no total, so it keeps the one it had.
        let old = target.map(|i| (self.recs[i].parent, self.recs[i].weight()));
        self.log(old.is_none_or(|o| o.0 != rec.parent), || Change::Upsert(raw.clone())); // added or moved: stays
        let slot = target.map_or(NONE, |i| i as u32);
        self.touch(rec.record, rec.parent, slot);
        if let Some((parent, _)) = old.filter(|o| o.0 != rec.parent) {
            self.touch(rec.record, parent, slot); // moved: the folder it left changed too
        }
        if rec.is_dir() {
            rec = rec.with_size(old.map_or(0, |o| o.1));
        }
        if old != Some((rec.parent, rec.weight())) {
            if let Some((parent, bytes)) = old {
                self.add_up(parent, -(bytes as i64));
            }
            self.add_up(rec.parent, rec.weight() as i64);
        }
        match target {
            // most journal events are content/metadata changes or moves: same name, so the slot (and its place in
            // the name order) stays
            Some(i) if self.name(i) == raw.name.as_str() => {
                rec.set_name((self.recs[i].name, self.recs[i].bits & NAME_BITS));
                self.recs[i] = rec;
            }
            // renamed: retire the old slot and add the new name to the tail, so the name order stays valid
            Some(i) => {
                let pos = self.push_rec(rec, &raw.name);
                self.link_move(rec.record, i as u32, pos);
                self.retire(i);
            }
            None => {
                let pos = self.push_rec(rec, &raw.name);
                self.link_add(rec.record, pos);
            }
        }
    }

    fn push_rec(&mut self, mut rec: Rec, name: &str) -> u32 {
        // past the room left by the last rebuild (a burst of long names): grow by that room again, not by doubling
        if self.recs.len() == self.recs.capacity() {
            self.recs.reserve_exact(REBUILD_AT);
        }
        if self.names.capacity() - self.names.len() < 3 * name.len() + 2 {
            self.names.reserve_exact(NAME_ROOM);
        }
        rec.set_name(push_name(&mut self.names, name));
        self.recs.push(rec);
        self.recs.len() as u32 - 1
    }

    /// Remove an entry (USN delete). Removes **every** link of the record (a
    /// hardlinked file frees its single MFT record on full deletion).
    pub fn apply_delete(&mut self, record_no: u64) {
        let Ok(record) = u32::try_from(record_no) else {
            return;
        };
        let positions = self.positions(record);
        if !positions.is_empty() {
            self.log(false, || Change::Delete(record_no));
        }
        for pos in positions {
            let r = self.recs[pos as usize];
            self.touch(record, r.parent, pos);
            self.add_up(r.parent, -(r.weight() as i64));
            self.retire(pos as usize);
        }
        if let Some(p) = self.first.get_mut(record as usize) {
            *p = NONE;
        }
        self.more.remove(&record);
    }

    /// Reconstruct an entry's absolute path by walking parents to the root, from original-case names or, with
    /// `lower`, straight from the lowercased arena (no per-part allocation).
    fn build_path(&self, i: usize, lower: bool) -> String {
        let mut parts: Vec<Cow<str>> = Vec::with_capacity(16);
        let part = |i: usize| if lower { Cow::Borrowed(self.lower(i)) } else { self.name(i) };
        parts.push(part(i));
        let mut parent = self.recs[i].parent;
        let mut depth = 0;
        while parent != ROOT_RECORD && depth < MAX_PATH_DEPTH {
            // Any link of the parent record yields the same directory name.
            match self.first.get(parent as usize).filter(|&&p| p != NONE) {
                Some(&p) => {
                    parts.push(part(p as usize));
                    parent = self.recs[p as usize].parent;
                }
                None => break,
            }
            depth += 1;
        }
        let mut path = String::with_capacity(64);
        path.push(if lower { self.drive.to_ascii_lowercase() } else { self.drive });
        path.push(':');
        for part in parts.iter().rev() {
            path.push('\\');
            path.push_str(part);
        }
        path
    }
}

/// Reused buffers for path matching: folder record -> (path, lowercased path), and the entry's own paths.
#[derive(Default)]
struct PathScratch {
    dirs: HashMap<u32, (String, String)>,
    path: String,
    path_lower: String,
}

/// Whether an entry with attributes `a` is listed: like Explorer, protected operating-system files (hidden + system:
/// NTUSER.DAT.LOG1, *.blf, ntuser.ini...) and hidden ones only on request.
fn shown(r: &Rec, opts: &SearchOptions) -> bool {
    let a = r.bits >> ATTR_SHIFT; // hidden (0x2) and system (0x4) keep their Windows bits in `ATTRS`
    if a & 0x6 == 0x6 { opts.show_system } else { opts.show_hidden || a & 0x2 == 0 }
}

/// Size as a sort/display key: a folder's total, `-1` for an unknown size.
fn size_key(r: &Rec) -> i64 {
    match r.size() {
        u64::MAX => -1,
        s => s as i64,
    }
}

/// A Shell Link header (MS-SHLLINK) whose stored target attributes include FILE_ATTRIBUTE_DIRECTORY.
fn lnk_header_is_folder(h: &[u8]) -> bool {
    h.len() >= 0x1C && h[..4] == [0x4C, 0, 0, 0] && u32::from_le_bytes([h[0x18], h[0x19], h[0x1A], h[0x1B]]) & 0x10 != 0
}

/// Extension slice of a lowercased name (after the last dot), or `""` if none.
/// A leading dot is a dotfile (`.gitignore`), not an extension â€” matching what
/// the UI's Ext column displays.
fn ext_of(name_lower: &str) -> &str {
    match name_lower.rfind('.') {
        Some(i) if i > 0 => &name_lower[i + 1..],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn orphans_are_dropped() {
        let r = |record, parent| Rec { record, parent, ..Rec::default() };
        // 10 sits in the root; 11 is under 10; 12 is under the missing record 99; 13 is under 12
        let mut recs = vec![r(10, ROOT_RECORD), r(11, 10), r(12, 99), r(13, 12)];
        drop_orphans(&mut recs);
        assert_eq!(recs.iter().map(|r| r.record).collect::<Vec<_>>(), vec![10, 11, NONE, NONE]);
    }

    use super::*;

    fn raw(record: u64, parent: u64, name: &str) -> RawRecord {
        RawRecord {
            record_no: record,
            parent_no: parent,
            seq: 0,
            parent_seq: 0,
            name: name.into(),
            is_dir: false,
            size: Some(1),
            modified_ft: 0,
            created_ft: 0,
            accessed_ft: 0,
            attributes: 0,
        }
    }

    /// An index as a scan builds it: every record appended (hardlinks included).
    fn index(raws: Vec<RawRecord>) -> SearchIndex {
        let (mut recs, mut names) = (Vec::new(), Vec::new());
        for r in raws {
            let mut rec = new_rec(&r).unwrap();
            rec.set_name(push_name(&mut names, &r.name));
            recs.push(rec);
        }
        SearchIndex::from_parts('C', recs, names)
    }

    /// Names of the live entries, in slot order.
    fn names(idx: &SearchIndex) -> Vec<String> {
        (0..idx.recs.len()).filter(|&i| idx.is_live(i)).map(|i| idx.name(i).into_owned()).collect()
    }

    /// Names in the default (name-sorted) view.
    fn by_name(idx: &SearchIndex) -> Vec<String> {
        let opts = SearchOptions::default();
        let m = Matcher::compile(&opts).unwrap();
        idx.ranked(&opts, &m).into_iter().map(|i| idx.name(i as usize).into_owned()).collect()
    }

    /// Every live entry is reachable through the record map, and no slot is stale.
    fn assert_links_consistent(idx: &SearchIndex) {
        for (i, r) in idx.recs.iter().enumerate().filter(|(_, r)| r.record != NONE) {
            assert!(idx.positions(r.record).contains(&(i as u32)), "entry {i} (record {}) not mapped", r.record);
        }
        for (rec, &p) in idx.first.iter().enumerate().filter(|(_, &p)| p != NONE) {
            assert_eq!(idx.recs[p as usize].record as usize, rec, "stale record slot");
        }
        for (rec, ps) in &idx.more {
            assert!(ps.iter().all(|&p| idx.recs[p as usize].record == *rec), "stale hardlink slot");
        }
    }

    #[test]
    fn hit_and_sort_read_created_accessed_from_the_datestore() {
        let _g = crate::datestore::test_lock(); // ENABLED is process-global; see its doc comment
        let idx = index(vec![raw(700001, 5, "a.txt"), raw(700002, 5, "b.txt")]);
        crate::datestore::set_enabled(true);
        crate::datestore::set('C', 700001, 100, 900);
        crate::datestore::set('C', 700002, 200, 800);

        assert_eq!(idx.hit(0).created, 100);
        assert_eq!(idx.hit(0).accessed, 900);
        assert_eq!(idx.hit(1).created, 200);
        assert_eq!(idx.hit(1).accessed, 800);
        assert!(idx.num_key(0, SortKey::Created) < idx.num_key(1, SortKey::Created)); // 100 < 200
        assert!(idx.num_key(0, SortKey::Accessed) > idx.num_key(1, SortKey::Accessed)); // 900 > 800

        crate::datestore::set_enabled(false);
        assert_eq!(idx.hit(0).created, 0); // gated off: reads nothing back even though it's still stored
    }

    #[test]
    fn rec_stays_24_bytes() {
        assert_eq!(std::mem::size_of::<Rec>(), 24);
        let mut r = Rec { record: 1, parent: 5, ..Rec::default() };
        r.set_name((9, 765 | CASE_FULL << CASE_SHIFT));
        let r = r.with_attrs(0x40_0000 | 0x4000 | 0x10 | 0x2 | 0x1);
        // exact below 256 GB, within half a KB above, up to 256 TB
        for s in [0, 1, u32::MAX as u64 + 7, (1 << 38) - 1, u64::MAX] {
            assert_eq!(r.with_size(s).size(), s);
        }
        for s in [1 << 38, (1 << 40) + 1000, (1 << 47) + 777] {
            assert!(r.with_size(s).size().abs_diff(s) <= 512, "{s}");
        }
        // the fields don't overlap
        let r = r.with_size((1 << 48) - 2);
        assert_eq!((r.name, r.lower_len(), r.case_kind()), (9, 765, CASE_FULL));
        assert_eq!(r.attrs(), 0x40_0000 | 0x4000 | 0x10 | 0x2 | 0x1);
        assert!(r.is_dir());
        assert_eq!(Rec::default().attrs(), 0x80); // nothing set reads as NORMAL
        assert!(!Rec::default().with_size(u64::MAX).with_attrs(0x10).with_attrs(0).is_dir());
    }

    #[test]
    fn names_round_trip_through_the_arena() {
        let all = ["readme.txt", "ReadMe.TXT", "ÃœnÃ¯cÃ¶dÃ© FÃ¯lÃ©.PDF", "Ã¼nÃ¯cÃ¶dÃ©", "a", "ABCDEFGHIJKLMNOPQRSTUVWXYZ.X", "Ä°stanbul"];
        let idx = index(all.iter().enumerate().map(|(k, n)| raw(10 + k as u64, 5, n)).collect());
        // laid out in name order: each looked up by name
        let got = names(&idx);
        let mut want = all.map(String::from);
        want.sort_by_key(|n| n.to_lowercase());
        assert_eq!(got, want);
        for (i, n) in got.iter().enumerate() {
            assert_eq!(idx.lower(i), n.to_lowercase());
        }
        let at = got.iter().position(|n| n == "ReadMe.TXT").unwrap();
        assert_eq!(idx.build_path(at, false), "C:\\ReadMe.TXT");
        assert_eq!(idx.build_path(at, true), "c:\\readme.txt");
    }

    #[test]
    fn path_of_resolves_a_record_number_and_nothing_for_an_unknown_one() {
        let idx = index(vec![raw(10, 5, "readme.txt")]);
        assert_eq!(idx.path_of(10), Some("C:\\readme.txt".into()));
        assert_eq!(idx.path_of(999), None);
    }

    #[test]
    fn folders_total_everything_under_them_through_live_changes() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        let file = |r, p, n: &str, s| RawRecord { size: Some(s), ..raw(r, p, n) };
        // a (10) holds b (11) and a.txt; b holds b.txt
        let mut idx = index(vec![dir(10, 5, "a"), dir(11, 10, "b"), file(20, 11, "b.txt", 100), file(21, 10, "a.txt", 10), file(22, 5, "r.txt", 5)]);
        let size = |idx: &SearchIndex, rec: u32| idx.hit(idx.first[rec as usize]).size;
        let both = |idx: &SearchIndex| (size(idx, 10), size(idx, 11));
        assert_eq!(both(&idx), (110, 100));
        idx.apply_upsert(file(20, 11, "b.txt", 300)); // grew
        assert_eq!(both(&idx), (310, 300));
        idx.apply_upsert(file(20, 5, "b.txt", 300)); // moved out to the root
        assert_eq!(both(&idx), (10, 0));
        idx.apply_upsert(file(23, 11, "new.txt", 7)); // created
        assert_eq!(both(&idx), (17, 7));
        idx.apply_upsert(dir(11, 5, "b")); // folder moved to the root: keeps its total
        assert_eq!(both(&idx), (10, 7));
        idx.apply_upsert(dir(11, 10, "c")); // renamed (new slot) and moved back
        assert_eq!(both(&idx), (17, 7));
        idx.apply_delete(21);
        idx.apply_delete(23);
        assert_eq!(both(&idx), (0, 0));
        idx.apply_upsert(file(24, 11, "x.txt", 42));
        let live = both(&idx);
        idx.rebuild(idx.rebuild_plan()); // recomputed from scratch: same answer
        assert_eq!(both(&idx), live);
        assert_eq!(live, (42, 42));
    }

    #[test]
    fn rebuild_stores_each_distinct_name_once_and_keeps_every_spelling() {
        let names = ["index.js", "README.md", "index.js", "readme.md", "README.md", "a.txt", "index.js", "Ünïcode.txt", "Ünïcode.txt"];
        let mut idx = index(names.iter().enumerate().map(|(k, n)| raw(10 + k as u64, 5, n)).collect());
        idx.rebuild(idx.rebuild_plan());
        let distinct = ["index.js", "README.md", "readme.md", "a.txt", "Ünïcode.txt"];
        let mut arena = Vec::new();
        for n in distinct {
            push_name(&mut arena, n);
        }
        assert_eq!(idx.names.len(), arena.len(), "one copy per distinct spelling");
        let mut shown: Vec<String> = (0..idx.recs.len()).map(|i| idx.name(i).into_owned()).collect();
        let mut want: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        shown.sort();
        want.sort();
        assert_eq!(shown, want, "every entry still shows its own spelling");
        assert_links_consistent(&idx);
    }

    #[test]
    fn live_changes_stay_sorted_without_a_rebuild_and_rebuild_drops_retired() {
        let mut idx = index(vec![raw(10, 5, "b.txt"), raw(11, 5, "d.txt"), raw(12, 5, "F.txt")]);
        idx.apply_upsert(RawRecord { size: Some(99), ..raw(10, 5, "b.txt") }); // metadata only: same slot
        assert_eq!(idx.recs.len(), 3);
        idx.apply_upsert(raw(11, 5, "a.txt")); // rename: retired slot + tail
        idx.apply_upsert(raw(13, 5, "C.txt")); // new file: tail
        idx.apply_upsert(raw(14, 5, "z.txt"));
        idx.apply_delete(12);
        assert_eq!(by_name(&idx), ["a.txt", "b.txt", "C.txt", "z.txt"]);
        assert_eq!(idx.len(), 4);
        assert_links_consistent(&idx);
        let epoch = idx.epoch();
        idx.rebuild(idx.rebuild_plan());
        assert_eq!(idx.recs.len(), 4, "retired slots are gone");
        assert_eq!(idx.names.len(), (0..4).map(|i| idx.stored_len(i)).sum::<usize>(), "and so are their names");
        assert_eq!(by_name(&idx), ["a.txt", "b.txt", "C.txt", "z.txt"]);
        assert_ne!(idx.epoch(), epoch);
        assert_links_consistent(&idx);
    }

    #[test]
    fn a_listing_hears_only_of_changes_in_or_below_its_folder() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        // 20 docs \ 21 deep, 30 music, under the root
        let mut idx = index(vec![dir(20, 5, "docs"), dir(21, 20, "deep"), dir(30, 5, "music"), raw(40, 30, "song.mp3")]);
        let s = idx.stamp();
        assert!(!idx.changed_under(&[20], s), "nothing happened yet");
        idx.apply_upsert(raw(41, 30, "other.mp3"));
        assert!(!idx.changed_under(&[20], s), "a file elsewhere");
        assert!(idx.changed_under(&[30], s));
        let s = idx.stamp();
        idx.apply_upsert(raw(42, 21, "note.txt"));
        assert!(idx.changed_under(&[20], s), "two levels down: docs' total size changed");
        let s = idx.stamp();
        idx.apply_upsert(raw(40, 20, "song.mp3")); // moved out of music into docs
        assert!(idx.changed_under(&[30], s) && idx.changed_under(&[20], s));
        let s = idx.stamp();
        idx.apply_delete(42);
        assert!(idx.changed_under(&[21], s) && !idx.changed_under(&[30], s));
        // too many changes since: can't tell, so it has changed
        let s = idx.stamp();
        for k in 0..TOUCHED as u64 + 1 {
            idx.apply_upsert(raw(41, 30, if k % 2 == 0 { "x" } else { "y" }));
        }
        assert!(idx.changed_under(&[20], s));
    }

    #[test]
    fn the_saved_index_plus_its_change_log_loads_back_as_the_live_index() {
        use crate::infrastructure::cache;
        use std::io::Write;
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        let every = |idx: &SearchIndex| {
            let mut v: Vec<(String, i64)> = (0..idx.recs.len() as u32).filter(|&i| idx.is_live(i as usize)).map(|i| idx.hit(i)).map(|h| (h.path, h.size)).collect();
            v.sort();
            v
        };
        let d = std::env::temp_dir().join(format!("mosaic-log-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("C.idx");
        // a snapshot big enough that two small batches stay in the log
        let mut idx = index((0..400).map(|r| raw(100 + r, 5, &format!("file{r}.txt"))).chain([dir(20, 5, "docs")]).collect());
        cache::save_to(&path, 'C', 1, 2, 10, idx.parts()).unwrap();
        idx.apply_upsert(raw(40, 20, "new.txt"));
        idx.apply_upsert(raw(100, 5, "renamed.txt"));
        idx.apply_delete(101);
        assert!(cache::append_to(&path, 20, &idx.take_unsaved().unwrap()).unwrap());
        idx.apply_upsert(raw(102, 20, "file2.txt")); // moved
        idx.apply_upsert(raw(40, 20, "newer.txt"));
        assert!(cache::append_to(&path, 30, &idx.take_unsaved().unwrap()).unwrap());
        let log = path.with_extension("log");
        let good = std::fs::metadata(&log).unwrap().len();
        std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(&[7, 0, 0]).unwrap(); // torn append
        let s = cache::load_from(&path, 'C').unwrap();
        assert_eq!(s.next_usn, 30, "the journal resumes past the log");
        assert_eq!(std::fs::metadata(&log).unwrap().len(), good, "the torn frame is cut off");
        let mut back = SearchIndex::from_parts('C', s.recs, s.names);
        s.log.iter().for_each(|f| back.replay(cache::changes(f)));
        assert_eq!(every(&back), every(&idx));
        assert!(back.take_unsaved().unwrap().is_empty(), "replayed changes are saved already");
        cache::save_to(&path, 'C', 1, 2, 40, back.parts()).unwrap();
        assert!(!log.exists(), "folded into the snapshot");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_coalesced_change_log_replays_to_the_live_index() {
        for k in 1..=20 {
            coalesced_replay((k as u64).wrapping_mul(0x9e3779b97f4a7c15));
        }
    }

    fn coalesced_replay(mut seed: u64) {
        use crate::infrastructure::cache;
        let every = |idx: &SearchIndex| {
            let mut v: Vec<(String, i64)> = (0..idx.recs.len() as u32).filter(|&i| idx.is_live(i as usize)).map(|i| idx.hit(i)).map(|h| (h.path, h.size)).collect();
            v.sort();
            v
        };
        let d = std::env::temp_dir().join(format!("mosaic-coalesce-{}-{seed}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("C.idx");
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        // the model: record -> (parent, name, is_dir, size); 5 is the root
        let mut live: std::collections::BTreeMap<u64, (u64, String, bool, u64)> = Default::default();
        let mut scan = vec![];
        for r in 0..40u64 {
            let (p, dir) = (if r < 8 { 5 } else { 100 + r % 8 }, r < 8);
            live.insert(100 + r, (p, format!("e{r}"), dir, if dir { 0 } else { r + 1 }));
            scan.push(RawRecord { is_dir: dir, size: (!dir).then_some(r + 1), ..raw(100 + r, p, &format!("e{r}")) });
        }
        // untouched: a snapshot big enough that the log stays beside it
        scan.extend((0..20_000u64).map(|k| RawRecord { size: Some(rnd(1 << 30) as u64), ..raw(10_000 + k, 5, &format!("f{}", rnd(1 << 20))) }));
        let mut idx = index(scan);
        cache::save_to(&path, 'C', 1, 2, 10, idx.parts()).unwrap();
        // a new folder, a file made in it, then the folder reported again (its time changed): the folder's first
        // event is what the file's bytes are added to, so it must stay in place
        let folder = RawRecord { is_dir: true, size: None, ..raw(150, 5, "new") };
        idx.apply_upsert(folder.clone());
        idx.apply_upsert(RawRecord { size: Some(77), ..raw(151, 150, "in.txt") });
        idx.apply_upsert(folder);
        live.insert(150, (5, "new".into(), true, 0));
        live.insert(151, (150, "in.txt".into(), false, 77));
        assert!(cache::append_to(&path, 11, &idx.take_unsaved().unwrap()).unwrap());
        let (mut next, mut gone, mut applied, mut logged) = (200u64, vec![], 0, 0);
        let under = |live: &std::collections::BTreeMap<u64, (u64, String, bool, u64)>, mut p: u64, r: u64| {
            while p != 5 {
                if p == r {
                    return true;
                }
                p = live[&p].0;
            }
            false
        };
        for usn in 0..30i64 {
            for _ in 0..200 {
                let keys: Vec<u64> = live.keys().copied().collect();
                let dirs: Vec<u64> = std::iter::once(5).chain(live.iter().filter(|e| e.1 .2).map(|e| *e.0)).collect();
                // mostly the newest few, as real churn is: the same temp files written again and deleted
                let r = if rnd(4) == 0 { keys[rnd(keys.len())] } else { keys[keys.len() - 1 - rnd(keys.len().min(6))] };
                let up = |r: u64, e: &(u64, String, bool, u64)| RawRecord { is_dir: e.2, size: (!e.2).then_some(e.3), ..raw(r, e.0, &e.1) };
                match rnd(8) {
                    // reported again unchanged, as a folder is each time an entry inside it changes
                    6 => {}
                    0 | 1 => {
                        // new entry, now and then in a record number freed earlier
                        let r = if !gone.is_empty() && rnd(3) == 0 {
                            gone.swap_remove(rnd(gone.len()))
                        } else {
                            next += 1;
                            next
                        };
                        let e = (dirs[rnd(dirs.len())], format!("n{}", rnd(50)), rnd(4) == 0, rnd(1000) as u64);
                        idx.apply_upsert(up(r, &e));
                        live.insert(r, e);
                        applied += 1;
                        continue;
                    }
                    2 | 3 if !live[&r].2 => live.get_mut(&r).unwrap().3 = rnd(1000) as u64,
                    4 => live.get_mut(&r).unwrap().1 = format!("m{}", rnd(50)),
                    5 => {
                        let p = dirs[rnd(dirs.len())];
                        if !under(&live, p, r) {
                            live.get_mut(&r).unwrap().0 = p;
                        }
                    }
                    _ if !live.values().any(|e| e.0 == r) => {
                        idx.apply_delete(r);
                        live.remove(&r);
                        gone.push(r);
                        applied += 1;
                        continue;
                    }
                    _ => continue,
                }
                if let Some(e) = live.get(&r) {
                    idx.apply_upsert(up(r, e));
                }
                applied += 1;
            }
            let c = idx.take_unsaved().unwrap();
            logged += c.len();
            assert!(cache::append_to(&path, usn + 12, &c).unwrap());
        }
        assert!(logged < applied * 9 / 10, "{logged} of {applied} changes logged");
        let s = cache::load_from(&path, 'C').unwrap();
        let mut back = SearchIndex::from_parts('C', s.recs, s.names);
        s.log.iter().for_each(|f| back.replay(cache::changes(f)));
        let (a, b) = (every(&back), every(&idx));
        if a != b {
            let diff: Vec<_> = a.iter().filter(|x| !b.contains(x)).chain(b.iter().filter(|x| !a.contains(x))).collect();
            panic!("replayed vs live: {diff:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_everything_view_takes_in_changes_as_a_full_rank_would() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        for (ff, asc) in [(true, true), (true, false), (false, true), (false, false)] {
            let opts = SearchOptions { folders_first: ff, ascending: asc, ..SearchOptions::default() };
            let m = Matcher::compile(&opts).unwrap();
            let mut idx = index(vec![dir(20, 5, "m"), raw(30, 20, "c.txt"), raw(31, 5, "k.txt"), raw(32, 5, "t.txt"), raw(33, 20, "q.txt")]);
            let (mut order, s) = (idx.ranked(&opts, &m), idx.stamp());
            idx.apply_upsert(raw(40, 5, "a.txt")); // new
            idx.apply_upsert(dir(41, 5, "z")); // new folder
            idx.apply_upsert(raw(31, 5, "x.txt")); // renamed
            idx.apply_upsert(raw(32, 20, "t.txt")); // moved
            idx.apply_delete(33);
            idx.apply_upsert(RawRecord { attributes: 0x2, ..raw(30, 20, "c.txt") }); // hidden now
            idx.apply_upsert(raw(42, 5, "b.txt"));
            idx.apply_upsert(raw(42, 5, "y.txt")); // created, then renamed
            assert!(idx.patch_all(&mut order, s, &opts));
            assert_eq!(order, idx.ranked(&opts, &m), "folders first {ff}, ascending {asc}");
            let s = idx.stamp();
            for k in 0..TOUCHED as u64 + 1 {
                idx.apply_upsert(raw(31, 5, if k % 2 == 0 { "x.txt" } else { "w.txt" }));
            }
            assert!(!idx.patch_all(&mut order, s, &opts), "too many changes ago to tell");
        }
    }

    #[test]
    fn folders_first_keeps_column_order_within_groups() {
        let dir = |r, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, 5, n) };
        // offline, so the test never opens a real file; such a link counts as a file
        let lnk = RawRecord { attributes: FILE_ATTRIBUTE_OFFLINE, ..raw(70, 5, "a.lnk") };
        let idx = index(vec![raw(60, 5, "b.txt"), dir(50, "zdir"), lnk, dir(51, "adir")]);
        let order = |asc| {
            let opts = SearchOptions { folders_first: true, ascending: asc, ..SearchOptions::default() };
            let m = Matcher::compile(&opts).unwrap();
            idx.ranked(&opts, &m).into_iter().map(|i| idx.name(i as usize).into_owned()).collect::<Vec<_>>()
        };
        assert_eq!(order(true), ["adir", "zdir", "a.lnk", "b.txt"]);
        assert_eq!(order(false), ["zdir", "adir", "b.txt", "a.lnk"]);
        let mut h = [0u8; 0x1C];
        h[0] = 0x4C;
        assert!(!lnk_header_is_folder(&h));
        h[0x18] = 0x10;
        assert!(lnk_header_is_folder(&h));
    }

    #[test]
    fn child_listing_uses_parent_numbers() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        let idx = index(vec![dir(50, 5, "Users"), raw(60, 50, "a.txt"), dir(51, 50, "sub"), raw(62, 51, "c.txt"), raw(61, 5, "b.txt")]);
        let list = |q: &str| {
            let opts = SearchOptions { query: q.into(), ..SearchOptions::default() };
            let m = Matcher::compile(&opts).unwrap();
            let mut v: Vec<String> = idx.ranked(&opts, &m).into_iter().map(|i| idx.name(i as usize).into_owned()).collect();
            v.sort();
            v
        };
        assert_eq!(list(r#"child:"C:\users""#), ["a.txt", "sub"]);
        assert_eq!(list(r#"child:"c:\Users\SUB""#), ["c.txt"]);
        assert_eq!(list(r#"child:"C:""#), ["Users", "b.txt"]);
        assert!(list(r#"child:"C:\nope""#).is_empty());
        assert!(list(r#"child:"D:\users""#).is_empty());
        // Home: the listed folders themselves, found without building paths
        assert_eq!(list(r#"parent:"C:\Users\x" | parent:"C:\users\sub\x" | parent:"C:\nope\x""#), ["Users", "sub"]);
    }

    #[test]
    fn path_search_builds_the_same_paths_from_cached_folders() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        // 63's parent (99) isn't indexed: like `build_path`, its path is then the drive root
        let idx = index(vec![dir(50, 5, "Users"), dir(51, 50, "Sub"), raw(62, 51, "c.txt"), raw(60, 50, "a.txt"), raw(61, 5, "b.txt")]);
        let find = |q: &str, case: bool| {
            let opts = SearchOptions { query: q.into(), match_case: case, ..SearchOptions::default() };
            let m = Matcher::compile(&opts).unwrap();
            let mut v: Vec<String> = idx.ranked(&opts, &m).into_iter().map(|i| idx.build_path(i as usize, false)).collect();
            v.sort();
            v
        };
        assert_eq!(find(r"path:users\sub\", false), [r"C:\Users\Sub\c.txt"]);
        assert_eq!(find(r"path:c:\users\", false), [r"C:\Users\Sub", r"C:\Users\Sub\c.txt", r"C:\Users\a.txt"]);
        assert_eq!(find(r"path:Users\Sub", true), [r"C:\Users\Sub", r"C:\Users\Sub\c.txt"]);
        assert!(find(r"path:users\sub", true).is_empty());
        assert_eq!(find(r"path:c:\b.txt", false), [r"C:\b.txt"]);
    }

    #[test]
    fn folder_lookup_cache_follows_renames() {
        let dir = |r, p, n: &str| RawRecord { is_dir: true, size: None, ..raw(r, p, n) };
        let mut idx = index(vec![dir(50, 5, "Users"), dir(51, 50, "sub"), raw(62, 51, "c.txt")]);
        assert_eq!(idx.dir_record(r"C:\Users\sub", false), Some(51));
        assert_eq!(idx.dir_record(r"c:\users\SUB", false), Some(51)); // cached
        idx.apply_upsert(dir(51, 50, "renamed"));
        assert_eq!(idx.dir_record(r"C:\Users\sub", false), None);
        assert_eq!(idx.dir_record(r"C:\Users\renamed", false), Some(51));
    }

    #[test]
    fn delete_removes_all_hardlinks_of_a_record() {
        // Record 50 is hardlinked (two names); record 60 is unrelated.
        let mut idx = index(vec![raw(50, 5, "a.dll"), raw(50, 6, "b.dll"), raw(60, 5, "x.txt")]);
        assert_eq!(idx.len(), 3);
        idx.apply_delete(50);
        assert_eq!(idx.len(), 1, "both links of record 50 must be gone");
        assert_eq!(names(&idx), ["x.txt"]);
        assert!(idx.positions(50).is_empty());
        assert_links_consistent(&idx);
    }

    #[test]
    fn upsert_targets_the_named_link_and_appends_new_ones() {
        let mut idx = index(vec![raw(50, 5, "a.dll"), raw(50, 6, "b.dll")]);
        // Updating an existing link (record + parent + name) replaces it, never a sibling.
        idx.apply_upsert(raw(50, 6, "b.dll"));
        assert_eq!(idx.len(), 2);
        // A new hardlink to the same record appends a row rather than clobbering one.
        idx.apply_upsert(raw(50, 7, "c.dll"));
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.positions(50).len(), 3);
        let all = names(&idx);
        assert!(["a.dll", "b.dll", "c.dll"].iter().all(|n| all.iter().any(|x| x == n)));
        assert_links_consistent(&idx);
    }

    #[test]
    fn upsert_renames_single_link_in_place() {
        // A single-link file's rename / move updates its row, never a duplicate.
        let mut idx = index(vec![raw(50, 5, "foo.txt"), raw(60, 5, "other.txt")]);
        idx.apply_upsert(raw(50, 5, "bar.txt")); // rename in place
        assert_eq!(idx.len(), 2, "rename must not leave a ghost row");
        let r50 = idx.positions(50);
        assert_eq!(r50.len(), 1);
        assert_eq!(idx.name(r50[0] as usize), "bar.txt");
        idx.apply_upsert(raw(50, 7, "bar.txt")); // move to a new parent
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.positions(50).len(), 1);
        assert_eq!(idx.recs[idx.positions(50)[0] as usize].parent, 7);
        assert_links_consistent(&idx);
    }

    #[test]
    fn live_additions_grow_by_the_rebuild_room_not_by_doubling() {
        let mut idx = index(vec![raw(10, 5, "a.txt"), raw(11, 5, "b.txt")]);
        assert_eq!(idx.recs.capacity(), 2 + REBUILD_AT);
        for r in 0..REBUILD_AT as u64 + 1 {
            idx.apply_upsert(raw(100 + r, 5, "new.txt"));
        }
        assert_eq!(idx.recs.capacity(), 2 + 2 * REBUILD_AT, "one more room, not twice the index");
        assert!(idx.names.capacity() <= idx.names.len() + NAME_ROOM + 16);
    }

    /// `MOSAIC_IDX=<path to a real C.idx> cargo test --release bench_folder_sizes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_folder_sizes() {
        let snap = crate::infrastructure::cache::load_from(std::path::Path::new(&std::env::var("MOSAIC_IDX").unwrap()), 'C').unwrap();
        let mut idx = SearchIndex::from_parts('C', snap.recs, snap.names);
        let t = std::time::Instant::now();
        idx.sum_folders();
        println!("BENCH sum_folders_ms={:.1} entries={}", t.elapsed().as_secs_f64() * 1000.0, idx.len());
        for p in [r"c:\windows", r"c:\program files", r"c:\users"] {
            let r = idx.dir_record(p, false).unwrap();
            println!("BENCH {p} = {:.2} GB", idx.hit(idx.first[r as usize]).size as f64 / 1e9);
        }
    }

    /// `MOSAIC_BENCH_IDX=<path to a real {drive}.idx> cargo test --release bench_real_index -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real_index() {
        use crate::application::Catalog;
        use std::time::Instant;
        let path = std::env::var("MOSAIC_BENCH_IDX").expect("MOSAIC_BENCH_IDX");
        // the v4 (bincode) cache: loaded the old way, timed, then saved and reloaded in the current format
        #[derive(Deserialize, Serialize)]
        struct OldRec { size_lo: u32, size_hi: u16, record: u32, parent: u32, name: u32, mtime: u32, attrs: u32, len: u16 }
        #[derive(Deserialize, Serialize)]
        struct Snap { _v: u32, drive: char, _s: u32, _j: u64, _u: i64, recs: Vec<OldRec>, names: Vec<u8> }
        let t = Instant::now();
        let s: Snap = bincode::deserialize_from(std::io::BufReader::new(std::fs::File::open(&path).unwrap())).unwrap();
        let old_load = t.elapsed();
        let t = Instant::now();
        let old_out = std::path::Path::new(&path).with_extension("v4.idx");
        bincode::serialize_into(std::io::BufWriter::new(std::fs::File::create(&old_out).unwrap()), &s).unwrap();
        println!("BENCH cache old_bincode_save_ms={}", t.elapsed().as_millis());
        let recs: Vec<Rec> = s.recs.iter().map(|o| { let mut r = Rec { record: o.record, parent: o.parent, mtime: o.mtime, ..Rec::default() }.with_attrs(o.attrs).with_size(match (o.size_hi as u64) << 32 | o.size_lo as u64 { 0xFFFF_FFFF_FFFF => u64::MAX, s => s }); r.set_name((o.name, (o.len & 0x3FFF) as u32 | ((o.len >> 14) as u32) << CASE_SHIFT)); r }).collect();
        let new_path = std::path::Path::new(&path).with_extension("v5.idx");
        let t = Instant::now();
        crate::infrastructure::cache::save_to(&new_path, s.drive, 1, 2, 3, (&recs, &s.names)).unwrap();
        let save = t.elapsed();
        let t = Instant::now();
        let snap = crate::infrastructure::cache::load_from(&new_path, s.drive).unwrap();
        let load = t.elapsed();
        println!("BENCH cache old_bincode_load_ms={} raw_save_ms={} raw_load_ms={}", old_load.as_millis(), save.as_millis(), load.as_millis());
        drop(recs);
        let t = Instant::now();
        let mut recs = snap.recs;
        drop_orphans(&mut recs);
        let idx = SearchIndex::from_parts(s.drive, recs, snap.names);
        let build = t.elapsed();
        let heap = idx.recs.capacity() * size_of::<Rec>() + idx.names.capacity() + idx.first.capacity() * 4;
        println!("BENCH load_ms={} from_parts_ms={} entries={} index_heap_mb={:.1}", load.as_millis(), build.as_millis(), idx.len(), heap as f64 / 1048576.0);
        let mut cat = Catalog::default();
        cat.upsert_volume(idx);
        let cases: &[(&str, &str, SortKey)] = &[
            ("empty", "", SortKey::Name),
            ("word", "win", SortKey::Name),
            ("rare", "zzqxj", SortKey::Name),
            ("two_words", "micro soft", SortKey::Name),
            ("ext", "ext:jpg;png;gif", SortKey::Name),
            ("glob", "*.dll", SortKey::Name),
            ("path", r"path:windows\system32", SortKey::Name),
            ("bare_path", r"users\", SortKey::Name),
            ("size_sort", "", SortKey::Size),
            ("mtime_sort", "ext:txt", SortKey::Modified),
            ("path_sort", "ext:txt", SortKey::Path),
            ("ext_sort", "", SortKey::Ext),
            ("child", r#"child:"C:\Windows\System32""#, SortKey::Name),
            ("child_new", r#"child:"C:\Program Files""#, SortKey::Name),
        ];
        for &(label, q, sort) in cases {
            let mut times = Vec::new();
            let mut total = 0;
            for run in 0..6 {
                cat.volume_mut(s.drive); // bumps the generation: every run is a cold search, not a view-cache hit
                let opts = SearchOptions { query: q.into(), sort, limit: 100, folders_first: true, match_path: q.contains('\\'), ascending: run % 2 == 0, ..SearchOptions::default() };
                let t = Instant::now();
                let r = cat.search(&opts);
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                total = r.total;
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!("BENCH {label:<11} median_ms={:8.2} min_ms={:8.2} total={total}", times[3], times[0]);
        }
        // paging the cached view (scrolling)
        let opts = SearchOptions { query: "".into(), limit: 100, folders_first: true, ..SearchOptions::default() };
        cat.search(&opts);
        let t = Instant::now();
        for p in 1..=200 {
            cat.search(&SearchOptions { offset: p * 100, ..opts.clone() });
        }
        println!("BENCH page_scroll  per_page_ms={:.3}", t.elapsed().as_secs_f64() * 1000.0 / 200.0);
    }

    /// Private bytes after each step of bringing a real index up and searching it, as the service does:
    /// `MOSAIC_BENCH_IDX=<path to a real {drive}.idx> cargo test --release bench_memory -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_memory() {
        use crate::application::Catalog;
        #[repr(C)]
        #[derive(Default)]
        struct Pmc { cb: u32, faults: u32, peak_ws: usize, ws: usize, q: [usize; 4], pagefile: usize, peak_pf: usize, private: usize }
        #[link(name = "kernel32")]
        extern "system" {
            fn K32GetProcessMemoryInfo(h: isize, p: *mut Pmc, cb: u32) -> i32;
        }
        let mb = |label: &str| {
            let mut p = Pmc { cb: size_of::<Pmc>() as u32, ..Default::default() };
            // SAFETY: -1 is our own process; `p` is a PROCESS_MEMORY_COUNTERS_EX of the size passed.
            unsafe { K32GetProcessMemoryInfo(-1, &mut p, p.cb) };
            println!("BENCH mem {label:<14} private_mb={:6.1} ws_mb={:6.1}", p.private as f64 / 1048576.0, p.ws as f64 / 1048576.0);
        };
        mb("start");
        let snap = crate::infrastructure::cache::load_from(std::path::Path::new(&std::env::var("MOSAIC_BENCH_IDX").unwrap()), 'C').unwrap();
        mb("loaded");
        let mut recs = snap.recs;
        drop_orphans(&mut recs);
        let mut idx = SearchIndex::from_parts('C', recs, snap.names);
        let t = std::time::Instant::now();
        let plan = idx.rebuild_plan();
        let planned = t.elapsed();
        let t = std::time::Instant::now();
        idx.rebuild(plan);
        println!("BENCH rebuild plan_ms={} (read lock) install_ms={} (write lock)", planned.as_millis(), t.elapsed().as_millis());
        let n = |c: usize, s: usize| c as f64 * s as f64 / 1048576.0;
        println!("BENCH parts entries={} recs_mb={:.1} names_mb={:.1} first_mb={:.1}", idx.len(), n(idx.recs.capacity(), size_of::<Rec>()), n(idx.names.capacity(), 1), n(idx.first.capacity(), 4));
        mb("indexed");
        let mut seen = std::collections::HashSet::new();
        let unique: usize = (0..idx.recs.len()).filter(|&i| idx.is_live(i)).map(|i| { let s = idx.recs[i].name as usize; &idx.names[s..s + idx.stored_len(i)] }).filter(|b| seen.insert(*b)).map(|b| b.len()).sum();
        println!("BENCH names unique_mb={:.1} of {:.1}", unique as f64 / 1048576.0, idx.names.len() as f64 / 1048576.0);
        drop(seen);
        let mut cat = Catalog::default();
        cat.upsert_volume(idx);
        for (q, sort) in [("", SortKey::Name), ("win", SortKey::Name), ("", SortKey::Size), ("ext:txt", SortKey::Modified), ("ext:txt", SortKey::Path), ("", SortKey::Ext)] {
            cat.volume_mut('C');
            cat.search(&SearchOptions { query: q.into(), sort, limit: 100, folders_first: true, ..SearchOptions::default() });
        }
        mb("searched");
    }

    #[test]
    fn deletes_keep_links_consistent() {
        let mut idx = index((10..20).map(|r| raw(r, 5, &format!("f{r}.txt"))).collect());
        idx.apply_delete(12);
        idx.apply_delete(18);
        idx.apply_delete(10);
        assert_eq!(idx.len(), 7);
        assert_links_consistent(&idx);
    }
}
