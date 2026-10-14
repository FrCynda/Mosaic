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
/// Segments smaller than this many docs count as this size when merging picks like-sized ones: none. Everything under
/// the floor is one class, merged whole each time 8 gather, so a segment just under it (tantivy's 10,000: a ~70 MB
/// one, ~11 GB a day; 100: tens of docs but MBs of logs) was rewritten every few live batches to take in a few KB.
/// Without a floor, segments merge only with ones their own size, so a document is rewritten once per class it climbs.
const MIN_MERGE_LAYER: u32 = 1;
const CHUNK: usize = 20_000; // files whose paths the bulk scan holds at once
/// Live changes are applied at most this often: a file rewritten constantly is extracted once per window.
const LIVE_BATCH: std::time::Duration = std::time::Duration::from_secs(60);
/// A file re-indexed this recently waits for a later batch: a log or transcript appended to all day was re-extracted
/// whole every minute, each time a new segment (MBs of SSD writes) for the same text plus a bit. Searches stay
/// current meanwhile: [`search`] reads what's waiting as it is now.
const HOT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

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
    /// Something was added or deleted since the last commit. A commit with nothing in it still rewrites `meta.json`
    /// and `.managed.json`: the bulk pass, skipping thousands of unchanged files a second, did that hundreds of times.
    dirty: std::sync::atomic::AtomicBool,
    reader: IndexReader,
}

impl Engine {
    fn open(index: Index, fields: Fields) -> tantivy::Result<Self> {
        // manual, reloaded synchronously in `commit()`: `OnCommitWithDelay` reloads on its own schedule,
        // so a search right after committing could still see the pre-commit snapshot
        // no decompressed-block cache: tantivy keeps 100 blocks (~1.6 MB) per segment, tens of MB over the index's
        // segments, for lookups (`get`, a hit's path) that decompress a block in microseconds anyway
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).doc_store_cache_num_blocks(0).try_into()?;
        Ok(Engine { index, fields, writer: Mutex::new(None), dirty: Default::default(), reader })
    }

    fn with_writer<R>(&self, f: impl FnOnce(&mut IndexWriter) -> tantivy::Result<R>) -> tantivy::Result<R> {
        let mut w = self.writer.lock();
        if w.is_none() {
            let writer = self.index.writer_with_num_threads(3, WRITER_BUDGET)?;
            let mut policy = tantivy::merge_policy::LogMergePolicy::default();
            policy.set_max_docs_before_merge(MAX_MERGE_DOCS);
            policy.set_min_layer_size(MIN_MERGE_LAYER);
            writer.set_merge_policy(Box::new(policy));
            *w = Some(writer);
        }
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(w) = self.writer.lock().as_mut().filter(|_| self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed)) {
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
            if self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                writer.commit()?;
            }
            writer.wait_merging_threads()?;
        }
        drop(w);
        self.reader.reload()
    }

    /// Drop every document over `words` words, then rewrite the segments they were in: a delete only marks a
    /// document, and the merge policy goes by the share of documents deleted, tiny here though they hold most of the
    /// bytes. The bulk pass then indexes those files again, through [`worth_indexing`].
    fn drop_long(&self, words: u32) -> tantivy::Result<()> {
        let searcher = self.reader.searcher();
        self.with_writer(|w| {
            let mut segments = Vec::new();
            for sr in searcher.segment_readers() {
                let norms = sr.get_fieldnorms_reader(self.fields.body)?;
                let store = sr.get_store_reader(0)?;
                let before = segments.len();
                for d in sr.doc_ids_alive().filter(|&d| norms.fieldnorm(d) > words) {
                    if let Some(OwnedValue::Str(p)) = store.get::<TantivyDocument>(d)?.get_first(self.fields.path) {
                        w.delete_term(Term::from_field_text(self.fields.path, p));
                        if segments.len() == before {
                            segments.push(sr.segment_id());
                        }
                    }
                }
            }
            w.commit()?;
            for id in segments {
                let _ = w.merge(&[id]).wait(); // one at a time; fails only if a background merge took it already
            }
            Ok(())
        })?;
        self.release()
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
    let mut hits = engine()?.search(query, limit).map_err(|e| format!("content search failed: {e}"))?;
    // files changed but not yet indexed (the batch minute, HOT_COOLDOWN, or the daily batch of app state and logs):
    // searched as they are now, newest first, from a throwaway in-RAM index, so results are current without writing
    // anything. Not system churn (thousands of temp files an hour): that's searched as of its daily batch. `try_lock`:
    // a batch in flight holds the lock while it indexes these very files.
    let fresh: Vec<String> = PENDING.try_lock().map_or_else(Vec::new, |p| {
        let mut v: Vec<(&str, i64)> = p
            .iter()
            .filter(|(p, _)| !system_churn(&p.to_ascii_lowercase()))
            .map(|(p, w)| (&**p, w.map_or(i64::MAX, |w| w.0)))
            .collect();
        v.sort_unstable_by_key(|e| std::cmp::Reverse(e.1));
        v.into_iter().map(|(p, _)| p.to_string()).collect()
    });
    if fresh.is_empty() {
        return Ok(hits);
    }
    let (fresh_hits, read) = search_fresh(&fresh, query, limit).map_err(|e| format!("content search failed: {e}"))?;
    hits.retain(|(p, _)| !read.contains(p)); // their indexed text is stale
    hits.extend(fresh_hits);
    hits.sort_by(|a, b| b.1.total_cmp(&a.1));
    hits.truncate(limit);
    Ok(hits)
}

// ponytail: plain-text/Office files only (IFilter types like PDF would need a subprocess per query), and capped at
// 200 files / 32 MB of text / 2000 looked at: past that (a build, an unzip) the rest is searched from the index, as of
// its last batch
/// The hits among `paths` as they are now, and the paths that answer covers (read, or gone).
fn search_fresh(paths: &[String], query: &str, limit: usize) -> tantivy::Result<(Vec<(String, f32)>, Vec<String>)> {
    let (schema, fields) = schema();
    let index = Index::create_in_ram(schema);
    let mut w: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    let (mut read, mut budget) = (Vec::new(), 32usize << 20);
    for path in paths.iter().take(2000) {
        if read.len() == 200 || budget == 0 {
            break;
        }
        if !Path::new(path).exists() {
            read.push(path.clone());
        } else if let Some(body) = crate::extract::local_text(path) {
            budget = budget.saturating_sub(body.len());
            w.add_document(doc!(fields.path => path.as_str(), fields.mtime => 0i64, fields.size => 0i64, fields.body => body))?;
            read.push(path.clone());
        }
    }
    w.commit()?;
    Ok((Engine::open(index, fields)?.search(query, limit)?, read))
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
/// Whether it wrote anything to the index.
fn index_file(path: &str, mtime: i64, size: i64) -> bool {
    if let Some(body) = crate::extract::local_text(path) {
        let Some(body) = worth_indexing(path, &body) else {
            // empty (a lock, a placeholder: a document per such file was a new segment a minute for nothing), or
            // machine data rather than text
            skip_mark(path, mtime, size);
            return forget(path);
        };
        upsert(path, mtime, size, body);
        return true;
    }
    if !crate::extract::has_ifilter(path) {
        skip_mark(path, mtime, size); // no text in it: don't open it again until it changes
        return false;
    }
    let Ok(exe) = crate::native::content_exe().or_else(|_| std::env::current_exe()) else { return false };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--extract").arg(path);
    let Some(out) = run_capped(cmd, EXTRACT_TIMEOUT, EXTRACT_MAX) else { return false };
    let body = String::from_utf8_lossy(&out);
    // an empty result is stored too (an empty body): the stored mtime/size is what lets the next session's scan
    // skip this file instead of starting another subprocess for it, every time
    upsert(path, mtime, size, first_words(body.trim()));
    true
}

/// Words of one file indexed, about a 300-page book. Past that it's machine output: measured, 27% of every word in
/// the index sat past this point in its file (minified bundles, game and database files, fonts).
const MAX_WORDS: usize = 100_000;

/// What of a text file's `body` to index: nothing when it's empty or machine data, else its first [`MAX_WORDS`].
fn worth_indexing<'a>(path: &str, body: &'a str) -> Option<&'a str> {
    let body = body.trim();
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default();
    let office = crate::extract::OFFICE.iter().any(|e| e.eq_ignore_ascii_case(ext));
    (!body.is_empty() && !binary(body) && (office || !minified(body))).then(|| first_words(body))
}

/// Binary data with a text extension (a database's `.log`): over 1% control characters, which text never has. C1
/// controls don't count: a Windows-1252 file decoded byte for byte puts its curly quotes and euro signs there.
fn binary(body: &str) -> bool {
    let (mut n, mut bad) = (0, 0);
    for c in body.chars().take(65536) {
        n += 1;
        bad += ((c < ' ' && !matches!(c, '\t' | '\n' | '\r' | '\x0c')) || c == '\u{FFFD}') as usize;
    }
    bad * 100 > n
}

/// Minified code or one-line data dumps: over 64 KB with lines averaging past 2000 characters. Not applied to Office
/// documents, whose extracted paragraphs can run long.
fn minified(body: &str) -> bool {
    body.len() > 64 << 10 && body.len() / (body.matches('\n').count() + 1) > 2000
}

/// `body` up to its [`MAX_WORDS`]-th word, words counted as the index's tokenizer splits them (letter/digit runs).
fn first_words(body: &str) -> &str {
    let (mut words, mut in_word) = (0, false);
    for (i, c) in body.char_indices() {
        let w = c.is_alphanumeric();
        if w && !in_word {
            words += 1;
            if words > MAX_WORDS {
                return &body[..i];
            }
        }
        in_word = w;
    }
    body
}

/// Drops `path`'s document, if it has one. Whether it did.
fn forget(path: &str) -> bool {
    let indexed = engine().ok().and_then(|e| e.get(path)).is_some();
    if indexed {
        remove(path);
    }
    indexed
}

/// A worker still running after this is killed (a filter stuck on a malformed file); what it printed so far is kept.
const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Output read from a worker: its own caps (`IFILTER_MAX_CHARS`, `CONTENT_TEXT_MAX`) with room for UTF-8.
const EXTRACT_MAX: u64 = 16 << 20;

/// Run `cmd` and return its stdout, at most `max` bytes; killed once `timeout` passes. A worker that keeps printing
/// past the cap finds its pipe closed and dies on the next write.
fn run_capped(mut cmd: std::process::Command, timeout: std::time::Duration, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.take(max).read_to_end(&mut b);
        let _ = tx.send(b); // dropping `out` here closes the pipe
    });
    let got = rx.recv_timeout(timeout);
    let _ = child.kill(); // no-op once it exited; a stuck or flooding worker ends here
    let _ = child.wait();
    // after a kill the pipe closes at once, unless something the worker started still holds it: don't wait on that
    got.or_else(|_| rx.recv_timeout(std::time::Duration::from_secs(1))).ok()
}

/// Files looked at and found to hold no text (binaries, media...), as hashes of (path, mtime, size). Only files
/// with text get a document, so without this every session's scan reopened every other file on the disk, millions
/// of opens and a few cores for as long as that takes, at each service start.
struct Skip {
    sorted: Vec<u64>,
    added: std::collections::HashSet<u64>,
}

/// Loaded only while [`bulk_index`] runs, the one reader of it and the one place it's saved: ~8 MB per million
/// files that a live change has no use for.
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

fn skip_load() {
    let sorted = std::fs::read(skip_path()).unwrap_or_default().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    *SKIP.lock() = Some(Skip { sorted, added: Default::default() });
}

/// Outside a bulk pass this drops the mark, as it always did: only a bulk pass saved them.
fn skip_mark(path: &str, mtime: i64, size: i64) {
    let k = skip_key(path, mtime, size);
    if let Some(s) = SKIP.lock().as_mut() {
        s.added.insert(k);
    }
}

fn skipped(path: &str, mtime: i64, size: i64) -> bool {
    let k = skip_key(path, mtime, size);
    SKIP.lock().as_ref().is_some_and(|s| s.sorted.binary_search(&k).is_ok() || s.added.contains(&k))
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
    // left by a build that purged Windows/ProgramData/AppData from the index: one full pass brings them back
    let purged = std::fs::remove_file(dir().with_file_name("contentscope1")).is_ok();
    done && !purged && trimmed_path().exists()
}

/// Written once the index went through [`trim`]: until then the next bulk pass starts with it.
fn trimmed_path() -> PathBuf {
    dir().with_file_name("contenttrim1")
}

/// Once per index: re-check its long documents under [`worth_indexing`] (they were indexed whole, junk included) and
/// have Windows compress the index folder's new files. Runs at the start of a bulk pass, which then re-indexes them.
pub fn trim() {
    if trimmed_path().exists() {
        return;
    }
    compress_dir(&dir());
    if engine().and_then(|e| e.drop_long(10_000).map_err(|e| e.to_string())).is_ok() {
        let _ = std::fs::write(trimmed_path(), b"");
    }
}

/// NTFS compression on `d`: files created in it from then on are compressed (existing ones aren't rewritten). Measured
/// ~10% off the positions files, decompressed on first read only (the file cache holds them decompressed).
fn compress_dir(d: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;
    let w: Vec<u16> = d.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated path; the handle is closed below and the in-buffer outlives the call
    unsafe {
        let h = CreateFileW(w.as_ptr(), FILE_GENERIC_READ | FILE_GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        let format = COMPRESSION_FORMAT_DEFAULT;
        let mut n = 0u32;
        windows_sys::Win32::System::IO::DeviceIoControl(h, FSCTL_SET_COMPRESSION, &format as *const u16 as _, 2, std::ptr::null_mut(), 0, &mut n, std::ptr::null_mut());
        CloseHandle(h);
    }
}

/// A full pass stands behind this session's index (finished now, or vouched for by [`take_done`]).
static COMPLETE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_complete() {
    COMPLETE.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn skip_save() {
    if let Some(s) = SKIP.lock().as_mut() {
        s.sorted.extend(s.added.drain());
        s.sorted.sort_unstable();
        s.sorted.dedup();
        let bytes: Vec<u8> = s.sorted.iter().flat_map(|k| k.to_le_bytes()).collect();
        let _ = std::fs::write(skip_path(), bytes);
    }
}

/// One-time (per session) walk of every already-known file, indexing anything new or changed since the
/// last time this ran — skipped via the stored `mtime`/`size` (`Engine::get`). Keeps the content index
/// synced with files that predate this session's USN events (the whole corpus, on a fresh index).
/// Spread across the thread pool: extraction (the slow part — file reads, zip parsing, the rare
/// subprocess) runs concurrently per file; only the final write is serialized, by `Engine`'s own lock.
pub fn bulk_index(total: usize, chunk: impl Fn(usize, usize) -> Option<Vec<(String, i64, i64)>> + Sync) -> bool {
    use rayon::prelude::*;
    use std::sync::atomic::Ordering::Relaxed;
    BULK_TOTAL.store(total, Relaxed);
    BULK_DONE.store(0, Relaxed);
    skip_load();
    // `chunk(from, n)` builds the paths of files `from..from + n` only when their turn comes: memory stays flat
    // however many files there are. `None` (the index was rebuilt meanwhile) ends the pass unfinished: the caller starts another on a fresh list.
    let scan = || {
        for (n, from) in (0..total).step_by(CHUNK).enumerate() {
            if n % 20 == 19 {
                skip_save();
            }
            let Some(files) = chunk(from, CHUNK) else { return false };
            files.par_iter().for_each(|(path, mtime, size)| {
                if !is_own(path) && !skipped(path, *mtime, *size) && engine().ok().and_then(|e| e.get(path)) != Some((*mtime, *size)) {
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
    *SKIP.lock() = None;
    if complete {
        mark_complete();
    }
    BULK_TOTAL.store(0, Relaxed); // done: progress() reports nothing more
    release_writer();
    complete
}

/// Drives the content index from live file-change events (see [`crate::infrastructure::ntfs::usn::ChangeEvent`]).
/// Runs on its own thread so extraction (a subprocess spawn per file) never blocks the USN watcher.
/// Changes heard but not yet applied: latest event per path, applied once per LIVE_BATCH window (a file saved 1000x
/// costs one extraction). Locked for the whole of a batch, so [`shutdown`] waits for one in flight.
/// A day of system churn is hundreds of thousands of paths: each is kept once, as a `Box<str>` (no spare capacity).
static PENDING: LazyLock<Mutex<std::collections::HashMap<Box<str>, Waiting>>> = LazyLock::new(Default::default);
/// A waiting change: the file's (mtime, size) as last heard, `None` once deleted.
type Waiting = Option<(i64, i64)>;

/// Whether a change still has work to do in a batch. A deleted file without a document has none (most churn: temp
/// files made and deleted within the day): dropped when heard, not held in memory until its batch.
fn worth_keeping(path: &str, w: Waiting) -> bool {
    w.is_some() || engine().map_or(true, |e| e.get(path).is_some())
}

/// Whether the change to `path` changed the index. Most don't (an attribute change, a file with no text, a deleted
/// temp file never indexed), and then nothing is written: every commit is a new meta.json and fsyncs, even an empty one.
fn apply(path: &str, w: Waiting) -> bool {
    match w {
        _ if is_own(path) => false,
        Some((mtime, size)) => engine().ok().and_then(|e| e.get(path)) != Some((mtime, size)) && index_file(path, mtime, size),
        None => forget(path),
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
        load_pending();
        let mut first = std::time::Instant::now();
        let mut recent: std::collections::HashMap<Box<str>, std::time::Instant> = Default::default();
        loop {
            let waiting = !PENDING.lock().is_empty();
            let wait = if waiting { LIVE_BATCH.saturating_sub(first.elapsed()) } else { std::time::Duration::from_secs(3600) };
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    let (path, w) = match ev {
                        ChangeEvent::Upserted { path, mtime, size } => (path, Some((mtime, size))),
                        ChangeEvent::Deleted { path } => (path, None),
                    };
                    if !is_own(&path) {
                        let mut pending = PENDING.lock();
                        if !worth_keeping(&path, w) {
                            pending.remove(path.as_str());
                            continue;
                        }
                        if pending.is_empty() {
                            first = std::time::Instant::now();
                        }
                        pending.insert(path.into_boxed_str(), w);
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
            use std::sync::atomic::Ordering::Relaxed;
            let churn_due = unix_now().saturating_sub(CHURN_SINCE.load(Relaxed)) >= CHURN_BATCH.as_secs();
            let ready: Vec<Box<str>> = pending.keys().filter(|p| if churny(p) { churn_due } else { !recent.contains_key(*p) }).cloned().collect();
            let mut any = false;
            for p in ready {
                let w = pending.remove(&p).unwrap();
                any |= apply(&p, w);
                if w.is_some() && !churny(&p) {
                    recent.insert(p, now);
                }
            }
            flush(any);
            if churn_due {
                CHURN_SINCE.store(unix_now(), Relaxed);
                pending.shrink_to_fit(); // the day's churn is gone: its table needn't stay that big
            }
            first = now; // what's still cooling down is looked at again next batch
        }
    });
}

/// Service stop: what's still waiting goes to [`pending_path`] for the next start (indexing it here, hours of hot
/// files and a day of churn, could take far longer than a stop allows), and if a full pass stands behind the index,
/// the marker that lets the next start skip its full pass. Keeps the batch lock so no new batch starts before exit.
pub fn shutdown(within: std::time::Duration) {
    let Some(pending) = PENDING.try_lock_for(within) else { return };
    let pending = parking_lot::MutexGuard::leak(pending); // never unlocked
    if !pending.is_empty() {
        let since = CHURN_SINCE.load(std::sync::atomic::Ordering::Relaxed);
        let text: String = std::iter::once(since.to_string()).chain(pending.drain().map(|(p, _)| p.into_string())).map(|l| l + "\n").collect();
        if std::fs::write(pending_path(), text).is_err() {
            return; // not kept: no marker, so the next start's full pass picks these up
        }
    }
    if COMPLETE.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::write(done_path(), b"");
    }
}

/// System/app churn: Windows, ProgramData and AppData rewrite files around the clock (logs, telemetry, temp, caches).
/// Applied per minute, each change cost an extraction, a commit and later segment merges (~30 GB/day of SSD writes).
/// So its changes wait for the daily [`CHURN_BATCH`] instead: temp and cache files created and deleted within the
/// day cancel out unseen, and a file rewritten all day is indexed once, as it ended up.
/// Same for app state outside AppData and for logs: dot-folders (`.git`, `.cache`, tools' session files), installed
/// packages, `.log` files and `logs` folders. Appended to or recreated all day, they were most of the live segments
/// and the merges after them. Unlike system churn, a search still reads them as they are now ([`search`]).
fn churny(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let dir = p.rsplit_once('\\').map_or("", |(d, _)| d);
    system_churn(&p) || dir.contains(r"\.") || p.contains(r"\node_modules\") || p.contains(r"\logs\") || p.ends_with(".log")
}

/// Windows, ProgramData and AppData, of a lowercased path.
fn system_churn(p: &str) -> bool {
    let rest = p.get(2..).unwrap_or_default(); // past the drive letter
    rest.starts_with(r"\windows\") || rest.starts_with(r"\programdata\") || p.contains(r"\appdata\")
}

const CHURN_BATCH: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// Unix seconds the current churn window opened (carried over a restart by [`pending_path`]).
static CHURN_SINCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Changes still waiting at a clean stop: the churn window's start, then one path per line.
fn pending_path() -> PathBuf {
    dir().with_file_name("contentchurn.txt")
}

/// Puts back what the last clean stop left waiting, as it stands now (its events weren't kept). User files land in
/// the first batch (cooldowns don't survive a restart); churn keeps its window.
fn load_pending() {
    let text = std::fs::read_to_string(pending_path()).unwrap_or_default();
    let _ = std::fs::remove_file(pending_path());
    let mut lines = text.lines();
    CHURN_SINCE.store(lines.next().and_then(|l| l.parse().ok()).unwrap_or_else(unix_now), std::sync::atomic::Ordering::Relaxed);
    let mut pending = PENDING.lock();
    for path in lines {
        let w = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
                Some((mtime, m.len() as i64))
            }
            _ => None,
        };
        if worth_keeping(path, w) {
            pending.insert(path.into(), w);
        }
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

    /// Read-only, elevated: for each path in `MOSAIC_PATHS` (`|`-separated), whether it's indexed, skip-listed, and
    /// what the filters make of it.
    #[test]
    #[ignore]
    fn bench_why() {
        skip_load();
        for p in std::env::var("MOSAIC_PATHS").unwrap().split('|') {
            let m = std::fs::metadata(p).unwrap();
            let mtime = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            let text = crate::extract::local_text(p);
            println!(
                "BENCH {p}\n  indexed={:?} now=({mtime}, {}) skipped={} local_text={:?} binary={:?} minified={:?} kept={:?}",
                engine().unwrap().get(p),
                m.len(),
                skipped(p, mtime, m.len() as i64),
                text.as_ref().map(|t| t.len()),
                text.as_deref().map(binary),
                text.as_deref().map(minified),
                text.as_deref().map(|t| worth_indexing(p, t).map(str::len)),
            );
        }
    }

    #[test]
    fn machine_data_is_not_indexed_and_long_text_is_cut() {
        let prose = "Le café coûte 3 €, l’addition s’il vous plaît.\n".repeat(2000);
        assert_eq!(worth_indexing(r"C:\a.txt", &prose), Some(prose.trim()));
        assert_eq!(worth_indexing(r"C:\a.txt", "  \n "), None);
        let db: String = (0..5000u32).map(|i| char::from_u32(i % 64).unwrap()).collect();
        assert_eq!(worth_indexing(r"C:\000004.log", &db), None);
        let bundle = "var a=1;function b(){return a}".repeat(5000);
        assert_eq!(worth_indexing(r"C:\vendor.js", &bundle), None);
        assert!(worth_indexing(r"C:\report.docx", &bundle).is_some()); // Office text: long paragraphs are fine
        let book = "word ".repeat(MAX_WORDS + 50);
        assert_eq!(first_words(&book).split_whitespace().count(), MAX_WORDS);
    }

    /// Read-only: what checking phrases by reading files (instead of stored positions) would cost. Per phrase: the
    /// files holding all its words (the candidates), how many really hold the phrase, and the wall time to read every
    /// candidate in parallel (installed `mosaic-content.exe` for IFilter types). Run elevated, like the one below.
    #[test]
    #[ignore]
    fn bench_phrase_check() {
        use rayon::prelude::*;
        use tantivy::collector::{Count, DocSetCollector};
        use tantivy::query::{BooleanQuery, PhraseQuery};
        use tantivy::tokenizer::TokenStream;
        let (schema, f) = schema();
        let index = open_index(&dir(), schema).unwrap();
        let searcher = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().map(|r: IndexReader| r.searcher()).unwrap();
        let mut tok = index.tokenizer_for_field(f.body).unwrap();
        let exe = r"C:\Program Files\Mosaic\mosaic-content.exe";
        let phrases = ["kestrel invoice", "error message", "thank you", "the project", "public static void", "merci beaucoup", "la facture", "of the"];
        for phrase in phrases {
            let mut words = Vec::new();
            let mut ts = tok.token_stream(phrase);
            while let Some(t) = ts.next() {
                words.push(t.text.clone());
            }
            let terms: Vec<Term> = words.iter().map(|w| Term::from_field_text(f.body, w)).collect();
            let loose = BooleanQuery::new(terms.iter().map(|t| (tantivy::query::Occur::Must, Box::new(TermQuery::new(t.clone(), IndexRecordOption::Basic)) as Box<dyn tantivy::query::Query>)).collect());
            let exact = searcher.search(&PhraseQuery::new(terms), &Count).unwrap();
            let docs = searcher.search(&loose, &DocSetCollector).unwrap();
            let paths: Vec<String> = docs
                .iter()
                .filter_map(|&a| match searcher.doc::<TantivyDocument>(a).ok()?.get_first(f.path) {
                    Some(OwnedValue::Str(p)) => Some(p.clone()),
                    _ => None,
                })
                .collect();
            let t = std::time::Instant::now();
            let (n_ifilter, n_local, n_all) = paths
                .par_iter()
                .map(|p| {
                    let (text, ifilter) = match crate::extract::local_text(p) {
                        Some(s) => (Some(s), false),
                        None if crate::extract::has_ifilter(p) => {
                            let mut cmd = std::process::Command::new(exe);
                            cmd.arg("--extract").arg(p);
                            (run_capped(cmd, EXTRACT_TIMEOUT, EXTRACT_MAX).map(|b| String::from_utf8_lossy(&b).into_owned()), true)
                        }
                        None => (None, false),
                    };
                    let all = text.is_some_and(|s| {
                        let l = s.to_lowercase();
                        words.iter().all(|w| l.contains(w.as_str()))
                    });
                    (ifilter as usize, !ifilter as usize, all as usize)
                })
                .reduce(|| (0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2));
            println!(
                "BENCH phrase={phrase:22} any_word={} candidates~={n_all} exact={exact} read_ms={} (ifilter={n_ifilter} local={n_local})",
                paths.len(),
                t.elapsed().as_millis()
            );
        }
    }

    /// Read-only: documents, words and stored path bytes per top folder of the live index (run elevated, the
    /// service's files are SYSTEM's). Words stand in for each folder's share of the postings on disk.
    #[test]
    #[ignore]
    fn bench_content_breakdown() {
        let (schema, f) = schema();
        let index = open_index(&dir(), schema).unwrap();
        let searcher = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().map(|r: IndexReader| r.searcher()).unwrap();
        let mut groups: std::collections::BTreeMap<&str, (u64, u64, u64)> = Default::default();
        let (mut over, mut big) = ([0u64; 3], Vec::new());
        for sr in searcher.segment_readers() {
            let norms = sr.get_fieldnorms_reader(f.body).unwrap();
            let store = sr.get_store_reader(0).unwrap();
            for d in sr.doc_ids_alive() {
                let doc: TantivyDocument = store.get(d).unwrap();
                let Some(OwnedValue::Str(p)) = doc.get_first(f.path) else { continue };
                let l = p.to_ascii_lowercase();
                let g = match &l.get(2..).unwrap_or_default() {
                    r if r.starts_with(r"\windows\") => "Windows",
                    r if r.starts_with(r"\programdata\") => "ProgramData",
                    _ if l.contains(r"\appdata\") => "AppData",
                    _ => "everything else",
                };
                let w = norms.fieldnorm(d) as u64;
                let e = groups.entry(g).or_default();
                *e = (e.0 + 1, e.1 + w, e.2 + p.len() as u64);
                // words past each cap, over all files
                for (k, cap) in [10_000u64, 100_000, 1_000_000].into_iter().enumerate() {
                    over[k] += w.saturating_sub(cap);
                }
                if w > 1_000_000 {
                    big.push((w, p.clone()));
                }
            }
        }
        for (g, (docs, words, path)) in groups {
            println!("BENCH {g:16} docs={docs} words={words} path_bytes={path}");
        }
        println!("BENCH words_past_cap 10k={} 100k={} 1M={}", over[0], over[1], over[2]);
        big.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        for (w, p) in big.iter().take(25) {
            println!("BENCH big words={w} {p}");
        }
    }

    #[test]
    fn a_stuck_worker_is_killed_and_a_flooding_one_cut_off() {
        let t = std::time::Instant::now();
        let mut hang = std::process::Command::new("powershell");
        hang.args(["-NoProfile", "-Command", "Start-Sleep 30"]);
        assert_eq!(run_capped(hang, std::time::Duration::from_secs(2), 1000), Some(Vec::new()));
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "took {:?}", t.elapsed());

        let big = std::env::temp_dir().join(format!("flood_{}.txt", std::process::id()));
        std::fs::write(&big, vec![b'x'; 4 << 20]).unwrap();
        let mut flood = std::process::Command::new("cmd");
        flood.arg("/c").arg("type").arg(&big);
        assert_eq!(run_capped(flood, std::time::Duration::from_secs(30), 1000).map(|b| b.len()), Some(1000));
        let _ = std::fs::remove_file(&big);
    }

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
    fn fresh_files_are_searched_as_they_are_now() {
        let f = std::env::temp_dir().join(format!("fresh_test_{}.txt", std::process::id()));
        std::fs::write(&f, "the quokka smiles").unwrap();
        let p = f.display().to_string();
        let (hits, read) = search_fresh(std::slice::from_ref(&p), "quokka", 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(), [p.as_str()]);
        assert_eq!(read, [p.clone()]);
        assert!(search_fresh(std::slice::from_ref(&p), "wombat", 10).unwrap().0.is_empty());
        let _ = std::fs::remove_file(&f);
        let (hits, read) = search_fresh(std::slice::from_ref(&p), "quokka", 10).unwrap();
        assert!(hits.is_empty() && read == [p], "a deleted file answers too: its indexed text must go");
    }

    #[test]
    fn system_and_app_churn_is_recognized() {
        assert!(churny(r"C:\Windows\Prefetch\X.pf"));
        assert!(churny(r"D:\ProgramData\Microsoft\Diagnosis\a.json"));
        assert!(churny(r"C:\Users\me\AppData\Roaming\Code\state.json"));
        assert!(!churny(r"C:\Users\me\Documents\Windows\notes.txt"));
        assert!(!churny(r"C:\Users\me\appdata.txt"));
        assert!(!churny(r"C:\Program Files\App\readme.txt"));
        assert!(churny(r"C:\Users\me\.tool\projects\session.jsonl"));
        assert!(churny(r"C:\Users\me\code\app\.git\COMMIT_EDITMSG"));
        assert!(churny(r"C:\Users\me\code\app\node_modules\x\index.js"));
        assert!(churny(r"C:\Server\logs\today.txt"));
        assert!(churny(r"C:\Users\me\code\app\build.log"));
        assert!(!churny(r"C:\Users\me\code\app\.env"), "a dot-file, not a dot-folder");
        assert!(!churny(r"C:\Users\me\Documents\v1.2\notes.txt"));
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
