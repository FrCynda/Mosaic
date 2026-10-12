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
/// only ever fires on a corrupt parent cycle — not on a genuinely deep path,
/// which would otherwise be silently truncated to a wrong-but-absolute path.
const MAX_PATH_DEPTH: usize = 4096;
/// "No entry" in the record map.
const NONE: u32 = u32::MAX;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// `Rec::len`: lowercased name length in the low 14 bits, case kind in the top 2 (0: the name is already all
/// lowercase, no case data).
const LEN_MASK: u16 = 0x3FFF;
/// ASCII name: one bit per byte (set = uppercase) follows the lowercased bytes.
const CASE_MASK: u16 = 1;
/// Anything else: a u16 length and the original UTF-8 follow the lowercased bytes.
const CASE_FULL: u16 = 2;

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

/// One file or folder, 28 bytes (no 8-byte field, so no padding: 10 MB less over 2.5M entries). Created/accessed times aren't kept (Everything doesn't index them by default
/// either); `dc:`/`da:` filters therefore match nothing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rec {
    /// Bytes, 48 bits (256 TB) split in two; all ones for folders and unknown sizes. See [`Rec::size`].
    size_lo: u32,
    size_hi: u16,
    /// MFT record number; `NONE` marks a retired slot (deleted, or renamed into a new slot).
    record: u32,
    /// MFT record number of the containing folder.
    parent: u32,
    /// Offset of the lowercased name (then its case data) in the name arena.
    name: u32,
    /// Last modified, Unix seconds; 0 = unknown.
    mtime: u32,
    /// `FILE_ATTRIBUTE_*`; DIRECTORY marks folders.
    attrs: u32,
    /// See [`LEN_MASK`].
    len: u16,
}

impl Rec {
    const NO_SIZE: u64 = (1 << 48) - 1;
    /// Bytes; `u64::MAX` for folders and unknown sizes.
    fn size(&self) -> u64 {
        match (self.size_hi as u64) << 32 | self.size_lo as u64 {
            Self::NO_SIZE => u64::MAX,
            s => s,
        }
    }
    fn with_size(self, size: u64) -> Rec {
        let s = size.min(Self::NO_SIZE);
        Rec { size_lo: s as u32, size_hi: (s >> 32) as u16, ..self }
    }
    fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    fn lower_len(&self) -> usize {
        (self.len & LEN_MASK) as usize
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
    /// Size in bytes; `-1` for directories or unknown.
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
    /// `recs[..sorted]` is covered by `by_name`; later slots are the unsorted tail of new names.
    sorted: usize,
    /// Bumped by every rebuild: positions from before it point at other entries.
    epoch: u64,
    /// MFT record number -> its (first) position in `recs`, or `NONE`.
    first: Vec<u32>,
    /// Further positions of hardlinked records: one MFT record with several names appears once per name, and
    /// the live update/delete paths must touch every link. Rare, hence a side map.
    more: HashMap<u32, Vec<u32>>,
    /// Positions of `recs[..sorted]` ordered by lowercased name (retired ones included, skipped when read).
    by_name: Vec<u32>,
    drive: char,
    /// The journal position this index has applied every change up to (set by the live watcher). The cache is
    /// saved with it, so the next start replays exactly what the saved copy is missing.
    pub usn: Option<i64>,
    /// Folder path (lowercased) -> its MFT record, for `dir_record`: each folder walked to once, not per listing.
    /// Checked on use (the record must still be that folder), so renames and deletes never serve a stale answer.
    dirs: parking_lot::Mutex<HashMap<String, u32>>,
}

/// Append `name` to the arena: lowercased bytes, then case data. Returns the `Rec::name`/`Rec::len` pair.
fn push_name(names: &mut Vec<u8>, name: &str) -> (u32, u16) {
    let off = names.len() as u32;
    if name.is_ascii() {
        names.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        if !name.bytes().any(|b| b.is_ascii_uppercase()) {
            return (off, name.len() as u16);
        }
        for chunk in name.as_bytes().chunks(8) {
            names.push(chunk.iter().enumerate().fold(0u8, |m, (k, b)| m | ((b.is_ascii_uppercase() as u8) << k)));
        }
        return (off, name.len() as u16 | CASE_MASK << 14);
    }
    let lower = name.to_lowercase();
    names.extend_from_slice(lower.as_bytes());
    if lower == name {
        return (off, lower.len() as u16);
    }
    names.extend_from_slice(&(name.len() as u16).to_le_bytes());
    names.extend_from_slice(name.as_bytes());
    (off, lower.len() as u16 | CASE_FULL << 14)
}

/// A record for `r`, minus its name; `None` for records that are never search hits or can't be stored.
fn new_rec(r: &RawRecord) -> Option<Rec> {
    if r.name.is_empty() || r.record_no == ROOT_RECORD as u64 || r.record_no >= NONE as u64 || r.parent_no >= NONE as u64 {
        return None;
    }
    let size = if r.is_dir { u64::MAX } else { r.size.unwrap_or(u64::MAX) };
    Some(Rec {
        size_lo: 0,
        size_hi: 0,
        record: r.record_no as u32,
        parent: r.parent_no as u32,
        name: 0,
        mtime: filetime_to_unix_s(r.modified_ft),
        attrs: if r.is_dir { r.attributes | FILE_ATTRIBUTE_DIRECTORY } else { r.attributes & !FILE_ATTRIBUTE_DIRECTORY },
        len: 0,
    }
    .with_size(size))
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
                (rec.name, rec.len) = push_name(&mut names, &r.name);
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
            by_name: Vec::new(),
            drive,
            usn: None,
            dirs: Default::default(),
        };
        let order = idx.live_name_order();
        idx.rebuild(order);
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

    /// Drop retired slots and their names, and install `order` from [`Self::live_name_order`] as the name view.
    /// O(n) copying; the caller guarantees no mutation happened since `order` was taken.
    pub fn rebuild(&mut self, order: Vec<u32>) {
        let live = self.len();
        let mut new_pos = vec![NONE; self.recs.len()];
        // room for the entries that arrive until the next rebuild: a Vec growing by doubling would reserve a
        // second copy of the whole index (~100 MB for 2M files) on the first file created after startup
        let mut recs = Vec::with_capacity(live + REBUILD_AT);
        let mut names = Vec::with_capacity(self.names.len() + NAME_ROOM);
        for i in 0..self.recs.len() {
            if self.is_live(i) {
                new_pos[i] = recs.len() as u32;
                let (s, n) = (self.recs[i].name as usize, self.stored_len(i));
                recs.push(Rec { name: names.len() as u32, ..self.recs[i] });
                names.extend_from_slice(&self.names[s..s + n]);
            }
        }
        names.shrink_to(names.len() + NAME_ROOM);
        self.by_name = order.into_iter().map(|i| new_pos[i as usize]).collect();
        debug_assert_eq!(self.by_name.len(), live);
        drop(new_pos);
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
        match r.len >> 14 {
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
        n + match r.len >> 14 {
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
    /// record number back into a real path — e.g. for the content indexer to open and re-read.
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
        }
    }

    /// Every match as entry positions in final display order (sort, grouping and
    /// direction applied). Cheap to page through: see `Catalog`'s view cache.
    pub fn ranked(&self, opts: &SearchOptions, matcher: &Matcher) -> Vec<u32> {
        let all = matcher.matches_all();
        // like Explorer, hide protected operating-system files (hidden + system: NTUSER.DAT.LOG1, *.blf, ntuser.ini...)
        let shown = |a: u32| if a & 0x6 == 0x6 { opts.show_system } else { opts.show_hidden || a & 0x2 == 0 }; // protected (hidden + system) and hidden files only on request
        // a plain folder listing: compare parent record numbers instead of rebuilding every path
        let child_of = matcher.only_child().map(|t| self.dir_record(t, false));
        let folders: Option<Vec<u32>> = matcher.only_parents().map(|ts| ts.into_iter().filter_map(|t| self.dir_record(t, true)).collect());
        let keep = |i: u32| {
            self.is_live(i as usize)
                && shown(self.recs[i as usize].attrs)
                && match (&folders, child_of) {
                    (Some(set), _) => set.contains(&self.recs[i as usize].record),
                    (None, Some(dir)) => dir == Some(self.recs[i as usize].parent),
                    (None, None) => all || self.entry_matches(i as usize, matcher),
                }
        };
        let (sort, asc) = (opts.sort, opts.ascending);
        let mut m: Vec<u32>;
        if matches!(sort, SortKey::Name) {
            // Name order without a full sort: filter the presorted view, then slot in the (small) sorted tail.
            m = self.by_name.par_iter().copied().filter(|&i| keep(i)).collect();
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
        } else {
            m.par_sort_unstable_by(|&a, &b| self.col_cmp(a as usize, b as usize, sort));
        }
        if !asc {
            m.reverse();
        }
        if opts.folders_first {
            m = self.folders_first(m);
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

    /// Stable split: folders and shortcuts to folders first, each group keeping the column order. O(n), so the
    /// presorted name order still serves folders-first searches without a full sort.
    fn folders_first(&self, m: Vec<u32>) -> Vec<u32> {
        // flags read in entry order into a compact table: the split below then hops through 2 MB, not the records
        let mut folder: Vec<bool> = self.recs.par_iter().map(Rec::is_dir).collect();
        let lnk_dirs: Vec<usize> =
            (0..self.recs.len()).into_par_iter().filter(|&i| self.is_live(i) && self.is_lnk(i) && self.lnk_to_folder(i)).collect();
        for i in lnk_dirs {
            folder[i] = true;
        }
        let mut dirs: Vec<u32> = m.par_iter().copied().filter(|&i| folder[i as usize]).collect();
        dirs.par_extend(m.par_iter().copied().filter(|&i| !folder[i as usize]));
        dirs
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
        let v = r.attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) == 0 && {
            use std::io::Read;
            let mut head = [0u8; 0x1C];
            std::fs::File::open(self.build_path(i, false)).and_then(|mut f| f.read_exact(&mut head)).is_ok()
                && lnk_header_is_folder(&head)
        };
        cache.lock().insert(key, v);
        v
    }

    /// Compare two entries by a sort column, ascending. `Path` is ranked separately.
    fn col_cmp(&self, a: usize, b: usize, sort: SortKey) -> std::cmp::Ordering {
        let (ra, rb) = (&self.recs[a], &self.recs[b]);
        match sort {
            SortKey::Name => self.lower(a).cmp(self.lower(b)),
            SortKey::Size => size_key(ra).cmp(&size_key(rb)),
            SortKey::Modified => ra.mtime.cmp(&rb.mtime),
            SortKey::Attributes => ra.attrs.cmp(&rb.attrs),
            // Directories show a blank Ext cell in the UI, so sort them as such.
            SortKey::Ext => self.ext(a).cmp(self.ext(b)),
            SortKey::Created => self.created_accessed(a).0.cmp(&self.created_accessed(b).0),
            SortKey::Accessed => self.created_accessed(a).1.cmp(&self.created_accessed(b).1),
            SortKey::Path => std::cmp::Ordering::Equal,
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
            attributes: r.attrs,
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

    fn entry_matches(&self, i: usize, matcher: &Matcher) -> bool {
        let r = &self.recs[i];
        let lower = self.lower(i);
        // original-case strings only for match-case searches: everything else reads the lowercased arena
        let case = matcher.match_case();
        let name = if case { self.name(i) } else { Cow::Borrowed(lower) };
        let (path, path_lower) = if matcher.needs_path() {
            (if case { self.build_path(i, false) } else { String::new() }, self.build_path(i, true))
        } else {
            (String::new(), String::new())
        };
        let (created, accessed) = self.created_accessed(i);
        matcher.eval(&EntryView {
            name: &name,
            name_lower: lower,
            path: &path,
            path_lower: &path_lower,
            is_dir: r.is_dir(),
            size: (r.size() != u64::MAX).then_some(r.size()),
            modified_ms: r.modified_ms(),
            created_ms: (created != 0).then_some(created as i64 * 1000),
            accessed_ms: (accessed != 0).then_some(accessed as i64 * 1000),
            attributes: r.attrs,
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
            // Unknown record — a new file or the first link.
            [] => None,
            // Single-link file: replace that one row in place. A USN event can't
            // say which old (parent, name) it replaced, but for a sole link a
            // rename, a move, and a metadata change all update the same row — so
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
        match target {
            // most journal events are content/metadata changes or moves: same name, so the slot (and its place in
            // the name order) stays
            Some(i) if self.name(i) == raw.name.as_str() => {
                (rec.name, rec.len) = (self.recs[i].name, self.recs[i].len);
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
        (rec.name, rec.len) = push_name(&mut self.names, name);
        self.recs.push(rec);
        self.recs.len() as u32 - 1
    }

    /// Remove an entry (USN delete). Removes **every** link of the record (a
    /// hardlinked file frees its single MFT record on full deletion).
    pub fn apply_delete(&mut self, record_no: u64) {
        let Ok(record) = u32::try_from(record_no) else {
            return;
        };
        for pos in self.positions(record) {
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

/// Size as a sort/display key: `-1` for directories or unknown size.
fn size_key(r: &Rec) -> i64 {
    if r.is_dir() || r.size() == u64::MAX {
        -1
    } else {
        r.size() as i64
    }
}

/// A Shell Link header (MS-SHLLINK) whose stored target attributes include FILE_ATTRIBUTE_DIRECTORY.
fn lnk_header_is_folder(h: &[u8]) -> bool {
    h.len() >= 0x1C && h[..4] == [0x4C, 0, 0, 0] && u32::from_le_bytes([h[0x18], h[0x19], h[0x1A], h[0x1B]]) & 0x10 != 0
}

/// Extension slice of a lowercased name (after the last dot), or `""` if none.
/// A leading dot is a dotfile (`.gitignore`), not an extension — matching what
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
        let r = |record, parent| Rec { size_lo: 0, size_hi: 0, record, parent, name: 0, mtime: 0, attrs: 0, len: 0 };
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
            (rec.name, rec.len) = push_name(&mut names, &r.name);
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
        assert_eq!(idx.col_cmp(0, 1, SortKey::Created), std::cmp::Ordering::Less); // 100 < 200
        assert_eq!(idx.col_cmp(0, 1, SortKey::Accessed), std::cmp::Ordering::Greater); // 900 > 800

        crate::datestore::set_enabled(false);
        assert_eq!(idx.hit(0).created, 0); // gated off: reads nothing back even though it's still stored
    }

    #[test]
    fn rec_stays_28_bytes() {
        assert_eq!(std::mem::size_of::<Rec>(), 28);
        let r = Rec { size_lo: 0, size_hi: 0, record: 1, parent: 5, name: 0, mtime: 0, attrs: 0, len: 0 };
        for s in [0, 1, u32::MAX as u64 + 7, (1 << 48) - 2, u64::MAX] {
            assert_eq!(r.with_size(s).size(), s);
        }
    }

    #[test]
    fn names_round_trip_through_the_arena() {
        let all = ["readme.txt", "ReadMe.TXT", "Ünïcödé Fïlé.PDF", "ünïcödé", "a", "ABCDEFGHIJKLMNOPQRSTUVWXYZ.X", "İstanbul"];
        let idx = index(all.iter().enumerate().map(|(k, n)| raw(10 + k as u64, 5, n)).collect());
        assert_eq!(names(&idx), all);
        for (i, n) in all.iter().enumerate() {
            assert_eq!(idx.lower(i), n.to_lowercase());
        }
        assert_eq!(idx.build_path(1, false), "C:\\ReadMe.TXT");
        assert_eq!(idx.build_path(1, true), "c:\\readme.txt");
    }

    #[test]
    fn path_of_resolves_a_record_number_and_nothing_for_an_unknown_one() {
        let idx = index(vec![raw(10, 5, "readme.txt")]);
        assert_eq!(idx.path_of(10), Some("C:\\readme.txt".into()));
        assert_eq!(idx.path_of(999), None);
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
        idx.rebuild(idx.live_name_order());
        assert_eq!(idx.recs.len(), 4, "retired slots are gone");
        assert_eq!(idx.names.len(), (0..4).map(|i| idx.stored_len(i)).sum::<usize>(), "and so are their names");
        assert_eq!(by_name(&idx), ["a.txt", "b.txt", "C.txt", "z.txt"]);
        assert_ne!(idx.epoch(), epoch);
        assert_links_consistent(&idx);
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
