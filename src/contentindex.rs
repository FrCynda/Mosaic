//! In-process full-text content index (Tantivy — MIT/Apache-2.0), replacing the external,
//! GPL-licensed Recoll/Xapian engine this app used to shell out to via `recollq.exe`/`recollindex.exe`.
//!
//! `main.rs`'s `content_query()` now searches this index directly; extraction runs out-of-process (see
//! `extract::full_text`, driven through the `--extract` worker mode) and content flows in from two
//! places: `bulk_index` walks the whole known corpus once per session (catching anything indexed before
//! this session, or on a fresh index), and `spawn_consumer` keeps it live off the USN watcher's
//! [`crate::infrastructure::ntfs::ChangeEvent`] stream after that.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use crate::infrastructure::ntfs::ChangeEvent;

use parking_lot::Mutex;
use tantivy::collector::TopDocs;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::MmapDirectory;
use tantivy::query::{QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, OwnedValue, Schema, FAST, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// 3 indexing threads at tantivy's 15MB-per-thread minimum: one thread capped the bulk scan at a single core, and the
/// default (50MB over up to 8 threads) just held more RAM.
const WRITER_BUDGET: usize = 45_000_000;
/// Segments past this many docs are left alone by merging: otherwise every few commits rewrite the whole
/// multi-GB index (the 30MB/s of disk writes and a core of CPU with nothing changing).
const MAX_MERGE_DOCS: usize = 100_000;
const CHUNK: usize = 20_000; // files whose paths the bulk scan holds at once
/// Live changes are applied at most this often: a file rewritten constantly is extracted once per window.
const LIVE_BATCH: std::time::Duration = std::time::Duration::from_secs(60);
/// A file re-indexed this recently waits for a later batch: a log or transcript appended to all day was re-extracted
/// whole every minute, each time a new segment (MBs of SSD writes) for the same text plus a bit.
const HOT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(600);

/// Fixed, not per-account: the indexer (the background service, running as SYSTEM when one's
/// installed — see `backend.rs`) and a search-only GUI process (running as the interactive user)
/// need to land on the same directory. `%LOCALAPPDATA%` resolves to two different profiles for
/// those two accounts; `%ProgramData%` is one shared location both can reach (SYSTEM gets full
/// control everywhere under it; a plain user gets create-in-root plus read/execute of what's
/// already there, so whichever account creates this folder first, the other can still use it).
fn dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("Mosaic").join("contentindex")
}

#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    mtime: Field,
    size: Field,
    body: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    // untokenized (STRING, not TEXT): a full path is the doc's identity, not something to word-search
    let path = b.add_text_field("path", STRING | STORED);
    let mtime = b.add_i64_field("mtime", STORED | FAST);
    let size = b.add_i64_field("size", STORED | FAST);
    // not STORED: the point is to search it, not redisplay it — the same space-saving choice Recoll makes
    let body = b.add_text_field("body", TEXT);
    (b.build(), Fields { path, mtime, size, body })
}

/// Everything needed to write to and search one index — kept together since both need the field
/// handles, and a `Schema`'s `Field`s are only meaningful paired with the schema that made them.
struct Engine {
    index: Index,
    fields: Fields,
    // lazy, and guarded by the same lock used to create it: tantivy allows only one writer per index
    // directory, and a search-only process (the GUI, when a background service does the actual
    // indexing — see `backend.rs`) must never try to grab it, or opening it would contend with the
    // process that actually holds it. The mutex being held across the "does it exist yet" check is
    // what keeps two racing threads from each trying to open a second writer.
    writer: Mutex<Option<IndexWriter>>,
    reader: IndexReader,
}

impl Engine {
    fn open(index: Index, fields: Fields) -> tantivy::Result<Self> {
        // manual, reloaded synchronously in `commit()`: `OnCommitWithDelay` reloads on its own schedule,
        // so a search right after committing could still see the pre-commit snapshot
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(Engine { index, fields, writer: Mutex::new(None), reader })
    }

    fn with_writer<R>(&self, f: impl FnOnce(&mut IndexWriter) -> tantivy::Result<R>) -> tantivy::Result<R> {
        let mut w = self.writer.lock();
        if w.is_none() {
            let writer = self.index.writer_with_num_threads(3, WRITER_BUDGET)?;
            let mut policy = tantivy::merge_policy::LogMergePolicy::default();
            policy.set_max_docs_before_merge(MAX_MERGE_DOCS);
            writer.set_merge_policy(Box::new(policy));
            *w = Some(writer);
        }
        f(w.as_mut().unwrap())
    }

    /// Re-indexes `path`: tantivy has no update, only delete-then-add (see its own
    /// `deleting_updating_documents` example), keyed on the untokenized `path` field.
    fn upsert(&self, path: &str, mtime: i64, size: i64, body: &str) -> tantivy::Result<()> {
        let f = &self.fields;
        self.with_writer(|w| {
            w.delete_term(Term::from_field_text(f.path, path));
            w.add_document(doc!(f.path => path, f.mtime => mtime, f.size => size, f.body => body))?;
            Ok(())
        })
    }

    fn remove(&self, path: &str) -> tantivy::Result<()> {
        self.with_writer(|w| {
            w.delete_term(Term::from_field_text(self.fields.path, path));
            Ok(())
        })
    }

    fn commit(&self) -> tantivy::Result<()> {
        // nothing to flush if this process never wrote anything (a search-only reader): reloading
        // the reader is still correct (and cheap) in case another process committed meanwhile
        if let Some(w) = self.writer.lock().as_mut() {
            w.commit()?;
        }
        self.reader.reload()
    }

    /// Commit and close the writer: its per-thread indexing buffers are tens of MB, held for nothing between live
    /// batches (one a minute at most). Merges finish first (under the lock: a second writer can't open while this
    /// one still holds the directory). Reopening costs milliseconds.
    fn release(&self) -> tantivy::Result<()> {
        let mut w = self.writer.lock();
        if let Some(mut writer) = w.take() {
            writer.commit()?;
            writer.wait_merging_threads()?;
        }
        drop(w);
        self.reader.reload()
    }

    /// The stored `(mtime, size)` of `path`'s current document, if indexed — lets a caller skip
    /// re-extracting a file whose content hasn't changed since it was last indexed.
    fn get(&self, path: &str) -> Option<(i64, i64)> {
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.path, path);
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        let (_, addr) = searcher.search(&q, &TopDocs::with_limit(1)).ok()?.into_iter().next()?;
        let d: TantivyDocument = searcher.doc(addr).ok()?;
        let (Some(OwnedValue::I64(mtime)), Some(OwnedValue::I64(size))) = (d.get_first(self.fields.mtime), d.get_first(self.fields.size)) else {
            return None;
        };
        Some((*mtime, *size))
    }

    /// Paths of the best `limit` matches for `query` against `body`, best first.
    fn search(&self, query: &str, limit: usize) -> tantivy::Result<Vec<(String, f32)>> {
        // no reload needed: searches run in the process that indexes (the service, or a local
        // indexer), and its own `commit()` reloads the reader
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.fields.body]);
        let q = parser.parse_query(query)?;
        let top = searcher.search(&q, &TopDocs::with_limit(limit))?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            if let Some(OwnedValue::Str(p)) = d.get_first(self.fields.path) {
                out.push((p.clone(), score));
            }
        }
        Ok(out)
    }
}

/// Opened on first use. A failure isn't cached: the next call tries again, rather than leaving
/// content search (or, in the indexing process, all indexing) dead for the rest of the session.
///
/// Only the process that owns the index should open it: tantivy takes a write lock on
/// `.tantivy-meta.lock` even to read, and files the service creates under `%ProgramData%` are
/// read-only to plain users — an unelevated GUI searches through the service instead
/// (`backend::SearchHandle::content`).
fn engine() -> Result<&'static Engine, String> {
    static E: OnceLock<Engine> = OnceLock::new();
    static OPENING: Mutex<()> = Mutex::new(());
    if let Some(e) = E.get() {
        return Ok(e);
    }
    let _g = OPENING.lock(); // one opener at a time: bulk_index calls this from every pool thread
    if let Some(e) = E.get() {
        return Ok(e);
    }
    let (schema, fields) = schema();
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    let engine = open_index(&d, schema).and_then(|i| Engine::open(i, fields)).map_err(|e| format!("content index unavailable: {e}"))?;
    Ok(E.get_or_init(|| engine))
}

fn open_index(dir: &Path, schema: Schema) -> tantivy::Result<Index> {
    Index::open_or_create(Dir { inner: MmapDirectory::open(dir)?, root: dir.to_path_buf() }, schema)
}

/// `MmapDirectory`, except for how `meta.json` (and `.managed.json`) gets replaced. Tantivy's own
/// `atomic_write` renames with plain `MoveFileExW`, which fails with "access denied" whenever
/// anything else has the target open at that instant — the reader reloading on another thread,
/// a merge, an antivirus scan. That fails the whole commit, and its documents are lost.
/// `std::fs::rename` retries such a failure as a POSIX-semantics rename, which can replace a file
/// that's still open (for reading, with delete sharing — how std and tantivy open it).
#[derive(Clone, Debug)]
struct Dir {
    inner: MmapDirectory,
    root: PathBuf,
}

impl tantivy::Directory for Dir {
    fn get_file_handle(&self, p: &Path) -> Result<std::sync::Arc<dyn tantivy::directory::FileHandle>, OpenReadError> {
        self.inner.get_file_handle(p)
    }
    fn delete(&self, p: &Path) -> Result<(), DeleteError> {
        self.inner.delete(p)
    }
    fn exists(&self, p: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(p)
    }
    fn open_write(&self, p: &Path) -> Result<tantivy::directory::WritePtr, OpenWriteError> {
        self.inner.open_write(p)
    }
    fn atomic_read(&self, p: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(p)
    }
    fn atomic_write(&self, p: &Path, data: &[u8]) -> std::io::Result<()> {
        let path = self.root.join(p);
        let tmp = path.with_extension("tmp-write");
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, data)?;
        f.sync_data()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    }
    fn acquire_lock(&self, lock: &tantivy::directory::Lock) -> Result<tantivy::directory::DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }
    fn watch(&self, cb: tantivy::directory::WatchCallback) -> tantivy::Result<tantivy::directory::WatchHandle> {
        self.inner.watch(cb)
    }
    fn sync_directory(&self) -> std::io::Result<()> {
        self.inner.sync_directory()
    }
}

/// Re-indexes one file's already-extracted text. A no-op call still costs a delete+add (tantivy
/// has no cheaper update path) — callers should skip files whose `mtime`/`size` haven't changed.
pub fn upsert(path: &str, mtime: i64, size: i64, body: &str) {
    if let Ok(e) = engine() {
        let _ = e.upsert(path, mtime, size, body);
    }
}

pub fn remove(path: &str) {
    if let Ok(e) = engine() {
        let _ = e.remove(path);
    }
}

/// Close the writer when no bulk scan is writing (see `Engine::release`).
pub fn release_writer() {
    if BULK_TOTAL.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        if let Ok(e) = engine() {
            let _ = e.release();
        }
    }
}

pub fn commit() {
    if let Ok(e) = engine() {
        let _ = e.commit();
    }
}

pub fn search(query: &str, limit: usize) -> Result<Vec<(String, f32)>, String> {
    engine()?.search(query, limit).map_err(|e| format!("content search failed: {e}"))
}

/// Bulk-scan progress, for the status line's "building index" message — `None` once the initial
/// scan is done (or before one has started), matching the shape `main.rs::content_progress` expects.
static BULK_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static BULK_TOTAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn progress() -> Option<(usize, usize)> {
    use std::sync::atomic::Ordering::Relaxed;
    let (done, total) = (BULK_DONE.load(Relaxed), BULK_TOTAL.load(Relaxed));
    (total > 0 && done < total).then_some((done, total))
}

/// Extracts `path`'s text and indexes it. `extract::local_text` — plain text/code and
/// Office/OpenDocument zips — is pure Rust and runs right here, no subprocess: that alone covers
/// most of a typical corpus. Only a file with a registered IFilter (`extract::has_ifilter`) pays
/// for a subprocess at all, and only then for the reason that one exists — a misbehaving filter
/// DLL takes down that throwaway child, never this process.
fn index_file(path: &str, mtime: i64, size: i64) {
    if let Some(body) = crate::extract::local_text(path) {
        upsert(path, mtime, size, &body);
        return;
    }
    if !crate::extract::has_ifilter(path) {
        skip_mark(path, mtime, size); // no text in it: don't open it again until it changes
        return;
    }
    let Ok(exe) = crate::native::content_exe().or_else(|_| std::env::current_exe()) else { return };
    let Ok(out) = std::process::Command::new(exe).arg("--extract").arg(path).output() else { return };
    let body = String::from_utf8_lossy(&out.stdout);
    // an empty result is stored too (an empty body): the stored mtime/size is what lets the next session's scan
    // skip this file instead of starting another subprocess for it, every time
    upsert(path, mtime, size, body.trim());
}

/// Files looked at and found to hold no text (binaries, media...), as hashes of (path, mtime, size). Only files
/// with text get a document, so without this every session's scan reopened every other file on the disk, millions
/// of opens and a few cores for as long as that takes, at each service start.
struct Skip {
    sorted: Vec<u64>,
    added: std::collections::HashSet<u64>,
}

static SKIP: Mutex<Option<Skip>> = Mutex::new(None);

fn skip_path() -> PathBuf {
    dir().with_file_name("contentskip.bin")
}

fn skip_key(path: &str, mtime: i64, size: i64) -> u64 {
    let mut h = 0xcbf29ce484222325u64; // FNV-1a
    for b in path.to_lowercase().bytes().chain(mtime.to_le_bytes()).chain(size.to_le_bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

fn with_skip<T>(f: impl FnOnce(&mut Skip) -> T) -> T {
    let mut g = SKIP.lock();
    let s = g.get_or_insert_with(|| {
        let sorted = std::fs::read(skip_path()).unwrap_or_default().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        Skip { sorted, added: Default::default() }
    });
    f(s)
}

fn skip_mark(path: &str, mtime: i64, size: i64) {
    let k = skip_key(path, mtime, size);
    with_skip(|s| {
        s.added.insert(k);
    });
}

fn skipped(path: &str, mtime: i64, size: i64) -> bool {
    let k = skip_key(path, mtime, size);
    with_skip(|s| s.sorted.binary_search(&k).is_ok() || s.added.contains(&k))
}

/// Left by a clean stop ([`shutdown`]) of a session whose index was complete: a full pass stood behind it and every
/// change it heard of had landed. The next start then trusts the journal replay instead of walking every file.
fn done_path() -> PathBuf {
    dir().with_file_name("contentdone")
}

/// Whether the last session left the index complete, consuming the marker: a crash this session leaves none behind.
pub fn take_done() -> bool {
    let done = done_path().exists() && dir().join("meta.json").exists();
    let _ = std::fs::remove_file(done_path());
    done
}

/// A full pass stands behind this session's index (finished now, or vouched for by [`take_done`]).
static COMPLETE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_complete() {
    COMPLETE.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn skip_save() {
    with_skip(|s| {
        s.sorted.extend(s.added.drain());
        s.sorted.sort_unstable();
        s.sorted.dedup();
        let bytes: Vec<u8> = s.sorted.iter().flat_map(|k| k.to_le_bytes()).collect();
        let _ = std::fs::write(skip_path(), bytes);
    });
}

/// One-time (per session) walk of every already-known file, indexing anything new or changed since the
/// last time this ran — skipped via the stored `mtime`/`size` (`Engine::get`). Keeps the content index
/// synced with files that predate this session's USN events (the whole corpus, on a fresh index).
/// Spread across the thread pool: extraction (the slow part — file reads, zip parsing, the rare
/// subprocess) runs concurrently per file; only the final write is serialized, by `Engine`'s own lock.
pub fn bulk_index(total: usize, chunk: impl Fn(usize, usize) -> Option<Vec<(String, i64, i64)>> + Sync) {
    use rayon::prelude::*;
    use std::sync::atomic::Ordering::Relaxed;
    BULK_TOTAL.store(total, Relaxed);
    BULK_DONE.store(0, Relaxed);
    // `chunk(from, n)` builds the paths of files `from..from + n` only when their turn comes: memory stays flat
    // however many files there are. `None` (the index was rebuilt meanwhile) ends the pass; the next start redoes it.
    let scan = || {
        for (n, from) in (0..total).step_by(CHUNK).enumerate() {
            if n % 20 == 19 {
                skip_save();
            }
            let Some(files) = chunk(from, CHUNK) else { return false };
            files.par_iter().for_each(|(path, mtime, size)| {
                if !excluded(path) && !skipped(path, *mtime, *size) && engine().ok().and_then(|e| e.get(path)) != Some((*mtime, *size)) {
                    index_file(path, *mtime, *size);
                }
                let n = BULK_DONE.fetch_add(1, Relaxed) + 1;
                if n.is_multiple_of(2000) {
                    commit();
                }
            });
        }
        true
    };
    // its own pool, not rayon's global one: this keeps every worker busy with blocking file reads for
    // minutes (hours on a fresh index), and rayon only serves a job from outside its pool once a worker
    // runs out of work — so a name search, or the USN watcher's re-sort, would wait out the whole scan
    // (holding the catalog lock, which stalls the watcher and with it every live update)
    let complete = match rayon::ThreadPoolBuilder::new().num_threads(std::thread::available_parallelism().map_or(4, |n| n.get() / 2).max(2)).thread_name(|i| format!("content-scan-{i}")).build() {
        Ok(pool) => pool.install(scan),
        Err(_) => scan(),
    };
    commit();
    skip_save();
    if complete {
        mark_complete();
    }
    BULK_TOTAL.store(0, Relaxed); // done: progress() reports nothing more
    release_writer();
}

/// Drives the content index from live file-change events (see [`crate::infrastructure::ntfs::usn::ChangeEvent`]).
/// Runs on its own thread so extraction (a subprocess spawn per file) never blocks the USN watcher.
/// Changes heard but not yet applied: latest event per path, applied once per LIVE_BATCH window (a file saved 1000x
/// costs one extraction). Locked for the whole of a batch, so [`shutdown`] waits for one in flight.
static PENDING: LazyLock<Mutex<std::collections::HashMap<String, ChangeEvent>>> = LazyLock::new(Default::default);

/// Whether `ev` touched the index at all (anything but the index's own files).
fn apply(ev: ChangeEvent) -> bool {
    match ev {
        ChangeEvent::Upserted { ref path, .. } | ChangeEvent::Deleted { ref path } if excluded(path) => false,
        ChangeEvent::Upserted { path, mtime, size } => {
            index_file(&path, mtime, size);
            true
        }
        ChangeEvent::Deleted { path } => {
            remove(&path);
            true
        }
    }
}

fn flush(any: bool) {
    if any {
        release_writer(); // commits and closes it, unless the bulk scan is still using it (then commit() below)
        commit();
    }
}

pub fn spawn_consumer(rx: std::sync::mpsc::Receiver<ChangeEvent>) {
    std::thread::spawn(move || {
        let mut first = std::time::Instant::now();
        let mut recent: std::collections::HashMap<String, std::time::Instant> = Default::default();
        loop {
            let waiting = !PENDING.lock().is_empty();
            let wait = if waiting { LIVE_BATCH.saturating_sub(first.elapsed()) } else { std::time::Duration::from_secs(3600) };
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    let path = match &ev {
                        ChangeEvent::Upserted { path, .. } | ChangeEvent::Deleted { path } => path.clone(),
                    };
                    if !excluded(&path) {
                        let mut pending = PENDING.lock();
                        if pending.is_empty() {
                            first = std::time::Instant::now();
                        }
                        pending.insert(path, ev);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !waiting => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            let mut pending = PENDING.lock();
            let now = std::time::Instant::now();
            recent.retain(|_, t| now - *t < HOT_COOLDOWN);
            let ready: Vec<String> = pending.keys().filter(|p| !recent.contains_key(*p)).cloned().collect();
            let mut any = false;
            for p in ready {
                let ev = pending.remove(&p).unwrap();
                if matches!(ev, ChangeEvent::Upserted { .. }) {
                    recent.insert(p, now);
                }
                any |= apply(ev);
            }
            flush(any);
            first = now; // what's still cooling down is looked at again next batch
        }
    });
}

/// Service stop: land the waiting batch, for at most `within`, and if nothing was left unapplied and a full pass
/// stands behind the index, leave the marker that lets the next start skip its full pass. Keeps the batch lock
/// so no new batch starts before the process exits.
pub fn shutdown(within: std::time::Duration) {
    let deadline = std::time::Instant::now() + within;
    let Some(pending) = PENDING.try_lock_for(within) else { return };
    let pending = parking_lot::MutexGuard::leak(pending); // never unlocked
    let mut any = false;
    for (_, ev) in pending.drain() {
        if std::time::Instant::now() > deadline {
            return; // out of time: changes left unapplied, so no marker; the next start's full pass picks them up
        }
        any |= apply(ev);
    }
    flush(any);
    if COMPLETE.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::write(done_path(), b"");
    }
}

/// Never content-indexed: the index's own files ([`is_own`]), and system/app churn, left out as `hashcache::SKIP`
/// leaves it out. Windows, ProgramData and AppData rewrite files around the clock (logs, telemetry, browser and app
/// state): each change cost an extraction, a commit and later segment merges, i.e. constant SSD writes, for text
/// nobody content-searches.
fn excluded(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let rest = p.get(2..).unwrap_or_default(); // past the drive letter
    is_own(path)
        || rest.starts_with(r"\windows\")
        || rest.starts_with(r"\programdata\")
        || rest.starts_with(r"\system volume information\")
        || rest.starts_with(r"\$recycle.bin\")
        || p.contains(r"\appdata\")
}

/// Marks [`purge_excluded`] done.
fn scope_path() -> PathBuf {
    dir().with_file_name("contentscope1")
}

/// One-time: drops what [`excluded`] keeps out but an index built before it already holds. Walks the `path`
/// term dictionary (every indexed path, without decompressing stored docs). The space comes back as the
/// segments holding them are merged.
pub fn purge_excluded() {
    if scope_path().exists() {
        return;
    }
    let Ok(e) = engine() else { return };
    let mut gone = Vec::new();
    for seg in e.reader.searcher().segment_readers() {
        let Ok(inv) = seg.inverted_index(e.fields.path) else { return };
        let Ok(mut terms) = inv.terms().stream() else { return };
        while terms.advance() {
            if let Ok(p) = std::str::from_utf8(terms.key()) {
                if excluded(p) {
                    gone.push(p.to_string());
                }
            }
        }
    }
    gone.sort_unstable();
    gone.dedup();
    let deleted = e.with_writer(|w| {
        for p in &gone {
            w.delete_term(Term::from_field_text(e.fields.path, p));
        }
        Ok(())
    });
    release_writer();
    commit();
    if deleted.is_ok() {
        let _ = std::fs::write(scope_path(), b"");
    }
}

/// A file inside the index's own directory. Never indexed: opening one while tantivy renames a new
/// `meta.json` over it (Windows can't replace a file someone has open) fails that whole commit and
/// drops its documents — and each commit's own writes would queue another commit, forever.
fn is_own(path: &str) -> bool {
    static OWN: OnceLock<String> = OnceLock::new();
    // the whole Mosaic folder: the index, and beside it the skip list and done marker
    let own = OWN.get_or_init(|| format!("{}\\", dir().parent().unwrap_or(&dir()).display()));
    path.get(..own.len()).is_some_and(|p| p.eq_ignore_ascii_case(own))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A from-scratch in-RAM index (never touches `engine()`'s on-disk singleton, so tests can't
    /// collide with each other or with a real running app's index) exercising the full
    /// upsert → commit → search → re-upsert (the delete-then-add "update") → search round trip.
    fn ram() -> Engine {
        let (schema, fields) = schema();
        Engine::open(Index::create_in_ram(schema), fields).unwrap()
    }

    #[test]
    fn upsert_commit_search_finds_the_document() {
        let e = ram();
        e.upsert("C:/notes/todo.txt", 100, 12, "buy milk and eggs").unwrap();
        e.upsert("C:/notes/other.txt", 100, 5, "unrelated content").unwrap();
        e.commit().unwrap();
        let hits = e.search("milk", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "C:/notes/todo.txt");
    }

    #[test]
    fn upsert_again_replaces_rather_than_duplicates() {
        let e = ram();
        e.upsert("C:/notes/todo.txt", 100, 12, "buy milk and eggs").unwrap();
        e.commit().unwrap();
        assert_eq!(e.search("milk", 10).unwrap().len(), 1);

        e.upsert("C:/notes/todo.txt", 200, 20, "buy bread instead").unwrap();
        e.commit().unwrap();
        assert_eq!(e.search("milk", 10).unwrap().len(), 0);
        let hits = e.search("bread", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "C:/notes/todo.txt");
    }

    #[test]
    fn meta_json_can_be_replaced_while_a_reader_has_it_open() {
        use tantivy::Directory;
        let root = std::env::temp_dir().join(format!("contentindex_test_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let d = Dir { inner: MmapDirectory::open(&root).unwrap(), root: root.clone() };
        let meta = Path::new("meta.json");
        d.atomic_write(meta, b"one").unwrap();
        let held = std::fs::File::open(root.join(meta)).unwrap(); // a reader mid-reload
        assert!(d.inner.atomic_write(meta, b"x").is_err(), "tantivy's own write is what fails here");
        d.atomic_write(meta, b"two").unwrap();
        drop(held);
        assert_eq!(d.atomic_read(meta).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn own_files_are_recognized_case_insensitively() {
        let d = dir().display().to_string();
        assert!(is_own(&format!(r"{d}\meta.json")));
        assert!(is_own(&format!(r"{}\.tmpAB12", d.to_uppercase())));
        assert!(is_own(&done_path().display().to_string()));
        assert!(!is_own(&format!(r"{}x\meta.json", dir().parent().unwrap().display())));
        assert!(!is_own(r"C:\Users\me\notes.txt"));
    }

    #[test]
    fn system_and_app_churn_is_excluded() {
        assert!(excluded(r"C:\Windows\Prefetch\X.pf"));
        assert!(excluded(r"D:\ProgramData\Microsoft\Diagnosis\a.json"));
        assert!(excluded(r"C:\Users\me\AppData\Roaming\Code\state.json"));
        assert!(excluded(r"C:\$Recycle.Bin\S-1\$R1.txt"));
        assert!(!excluded(r"C:\Users\me\Documents\Windows\notes.txt"));
        assert!(!excluded(r"C:\Users\me\appdata.txt"));
        assert!(!excluded(r"C:\Program Files\App\readme.txt"));
    }

    #[test]
    fn remove_drops_it_from_search() {
        let e = ram();
        e.upsert("C:/notes/todo.txt", 100, 12, "buy milk and eggs").unwrap();
        e.commit().unwrap();
        assert_eq!(e.search("milk", 10).unwrap().len(), 1);

        e.remove("C:/notes/todo.txt").unwrap();
        e.commit().unwrap();
        assert_eq!(e.search("milk", 10).unwrap().len(), 0);
    }
}
