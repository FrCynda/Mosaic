//! The background indexing lifecycle, independent of any UI. Owns the shared
//! [`Catalog`], scans/loads each fixed NTFS volume, keeps it live via USN
//! watchers, and persists the on-disk cache. Used both by the in-process GUI
//! ([`crate::presentation::state::AppState`]) and the background service, so it
//! lives in `infrastructure` with no presentation dependency.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use crate::application::{Catalog, IndexStatus, SearchIndex};
use crate::infrastructure::cache;
use crate::infrastructure::ntfs::{
    catch_up, journal_state, ntfs_fixed_drives, volume_serial, CatchUp, MftReader, UsnWatcher,
};

/// How often the cache saver appends what changed to the cache's log (KBs: see `cache`). Keeps the saved journal
/// position minutes old, so a crash costs a short replay, never the rescan a wrapped journal (~8 h of a busy
/// disk) forces.
const CACHE_SAVE_INTERVAL: Duration = Duration::from_secs(300);

/// What [`save_caches`] saves, set once the volumes are online.
static SAVER: OnceLock<(Arc<RwLock<Catalog>>, Vec<IndexedVolume>)> = OnceLock::new();
/// `CHANGES` as of the last save; the lock also keeps the saver and a stop from writing the same file at once.
static SAVED_AT: Mutex<u64> = Mutex::new(0);

/// Where indexing is in its lifecycle.
enum Phase {
    Indexing,
    Ready,
    Error(String),
}

/// A volume that finished indexing, plus what the cache saver needs.
struct IndexedVolume {
    drive: char,
    serial: u32,
}

/// Drives the index: builds it, keeps it live, and exposes it + a status
/// snapshot. Cheap to clone the shared [`Catalog`] handle out for searching.
pub struct Indexer {
    catalog: Arc<RwLock<Catalog>>,
    phase: Arc<RwLock<Phase>>,
    /// Entries committed from volumes already finished.
    committed: Arc<AtomicUsize>,
    /// Running entry count of the volume currently being scanned.
    progress: Arc<AtomicUsize>,
    /// Label of the volume currently being scanned.
    scanning: Arc<RwLock<String>>,
    drives: Arc<RwLock<Vec<char>>>,
    watchers: Arc<Mutex<Vec<UsnWatcher>>>,
}

impl Default for Indexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Indexer {
    pub fn new() -> Self {
        Self {
            catalog: Arc::new(RwLock::new(Catalog::default())),
            phase: Arc::new(RwLock::new(Phase::Indexing)),
            committed: Arc::new(AtomicUsize::new(0)),
            progress: Arc::new(AtomicUsize::new(0)),
            scanning: Arc::new(RwLock::new(String::new())),
            drives: Arc::new(RwLock::new(Vec::new())),
            watchers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The shared catalog handle, for running searches.
    pub fn catalog(&self) -> Arc<RwLock<Catalog>> {
        self.catalog.clone()
    }

    /// Bring every fixed NTFS volume online on a background thread: load its
    /// cached index and replay journal changes where possible, otherwise scan
    /// the MFT. Each volume starts its USN watcher as it completes.
    pub fn start(&self) {
        let catalog = self.catalog.clone();
        let phase = self.phase.clone();
        let committed = self.committed.clone();
        let progress = self.progress.clone();
        let scanning = self.scanning.clone();
        let drives_slot = self.drives.clone();
        let watchers = self.watchers.clone();

        let (content_tx, content_rx) = std::sync::mpsc::channel();
        crate::contentindex::spawn_consumer(content_rx);

        thread::spawn(move || {
            let drives = ntfs_fixed_drives();
            *drives_slot.write() = drives.clone();

            if drives.is_empty() {
                *phase.write() = Phase::Error("no fixed NTFS volumes found".into());
                return;
            }

            let mut indexed: Vec<IndexedVolume> = Vec::new();
            let mut content_current = crate::contentindex::take_done();
            let mut last_error = String::new();

            for drive in drives {
                *scanning.write() = format!("{drive}:");
                progress.store(0, Ordering::Relaxed);
                let serial = volume_serial(drive);

                let outcome = bring_volume_online(drive, serial, &progress, &content_tx);
                match outcome {
                    Ok(volume) => {
                        content_current &= volume.replayed;
                        let count = volume.index.len();
                        // A fresh scan is saved before publishing so a cache exists immediately. A loaded one isn't
                        // rewritten: it plus the journal still gets the next start here.
                        if let (false, Some(next_usn)) = (volume.replayed, volume.resume_usn) {
                            let _ = cache::save(
                                drive,
                                serial,
                                volume.journal_id,
                                next_usn,
                                volume.index.parts(),
                            );
                            let _ = crate::datestore::save();
                        }
                        catalog.write().upsert_volume(volume.index);
                        watchers.lock().push(UsnWatcher::spawn(
                            drive,
                            catalog.clone(),
                            volume.resume_usn,
                            Some(content_tx.clone()),
                        ));
                        committed.fetch_add(count, Ordering::Relaxed);
                        // Clear `progress` immediately so a concurrent status()
                        // (committed + progress) can't briefly count this volume
                        // twice while progress still holds its just-committed count.
                        progress.store(0, Ordering::Relaxed);
                        indexed.push(IndexedVolume { drive, serial });
                    }
                    Err(e) => last_error = e,
                }
            }

            *scanning.write() = String::new();
            progress.store(0, Ordering::Relaxed);
            *phase.write() = if indexed.is_empty() {
                // The usual cause now is an unelevated GUI with no service to
                // read the volumes for it — point at both ways out.
                Phase::Error(format!(
                    "couldn't read the NTFS volumes ({last_error}) — install the background \
                     service in Settings, or launch Mosaic as administrator"
                ))
            } else {
                Phase::Ready
            };

            if !indexed.is_empty() {
                // the full content scan only when something may have been missed: a volume scanned from scratch (its
                // changes while we were off are unknown) or an earlier scan that never finished. Otherwise the
                // journal replay above already sent every change since the last run to the content index.
                if content_current {
                    crate::contentindex::mark_complete();
                } else {
                    let scan_catalog = catalog.clone();
                    thread::spawn(move || {
                        // before the file list: it takes a while, and the list goes stale if the index re-sorts meanwhile
                        crate::contentindex::trim();
                        // a re-sort of the name index (every REBUILD_AT changes) ends a pass unfinished: go again on a
                        // fresh list, the files done so far cost a lookup each
                        loop {
                            let (refs, epochs) = scan_catalog.read().file_positions();
                            if crate::contentindex::bulk_index(refs.len(), |from, n| scan_catalog.read().files_at(&refs[from..(from + n).min(refs.len())], &epochs)) {
                                break;
                            }
                        }
                    });
                }
                spawn_cache_saver(catalog, indexed);
            }
        });
    }

    /// Current status snapshot for the UI / IPC.
    pub fn status(&self) -> IndexStatus {
        let count = self.committed.load(Ordering::Relaxed) + self.progress.load(Ordering::Relaxed);
        let mut s = match &*self.phase.read() {
            Phase::Indexing => {
                let scanning = self.scanning.read();
                let volume = if scanning.is_empty() {
                    "volumes".to_string()
                } else {
                    scanning.clone()
                };
                IndexStatus::indexing(volume, count)
            }
            Phase::Ready => IndexStatus::ready(self.volume_label(), count),
            Phase::Error(message) => IndexStatus::error(self.volume_label(), message.clone()),
        };
        s.changes = super::ntfs::CHANGES.load(Ordering::Relaxed);
        if let Some((done, total)) = crate::contentindex::progress() {
            (s.content_done, s.content_total) = (done, total);
        }
        s
    }

    fn volume_label(&self) -> String {
        let drives = self.drives.read();
        if drives.is_empty() {
            return "—".into();
        }
        drives
            .iter()
            .map(|d| format!("{d}:"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A volume brought online, with the journal position to resume the watcher.
struct OnlineVolume {
    /// Loaded from the cache and caught up from the journal (every change since reported to the content index),
    /// rather than scanned from scratch.
    replayed: bool,
    index: SearchIndex,
    journal_id: u64,
    resume_usn: Option<i64>,
}

/// Load a volume from cache and catch it up, or scan its MFT from scratch.
fn bring_volume_online(
    drive: char,
    serial: u32,
    progress: &AtomicUsize,
    sink: &std::sync::mpsc::Sender<super::ntfs::ChangeEvent>,
) -> Result<OnlineVolume, String> {
    // Fast path: a valid cached index we can replay forward.
    if let Some(snapshot) = cache::load(drive) {
        if snapshot.volume_serial == serial {
            let journal_id = snapshot.journal_id;
            let mut recs = snapshot.recs;
            crate::application::index::drop_orphans(&mut recs);
            let mut index = SearchIndex::from_parts(drive, recs, snapshot.names);
            for frame in &snapshot.log {
                index.replay(cache::changes(frame)); // a frame at a time: the whole log decoded is 100+ MB
            }
            progress.store(index.len(), Ordering::Relaxed);
            if let CatchUp::Caught { next_usn } =
                catch_up(drive, journal_id, snapshot.next_usn, &mut index, Some(sink))
            {
                return Ok(OnlineVolume {
                    replayed: true,
                    index,
                    journal_id,
                    resume_usn: Some(next_usn),
                });
            }
        }
    }

    // Full scan. Capture the journal position first so any change during the
    // scan is replayed (idempotently) on the next load rather than lost.
    let journal = journal_state(drive);
    let mut reader = MftReader::open(drive).map_err(|e| e.to_string())?;
    let index = SearchIndex::build_from(&mut reader, progress).map_err(|e| e.to_string())?;

    Ok(OnlineVolume {
        replayed: false,
        index,
        journal_id: journal.map(|j| j.journal_id).unwrap_or(0),
        resume_usn: journal.map(|j| j.next_usn),
    })
}

/// Save what changed in each volume's index since the last save: appended to its cache's log, or the whole cache
/// when that log is due for folding in. Called every few minutes and on service stop, so the next start loads fast
/// and replays only a small journal tail.
pub fn save_caches() {
    let Some((catalog, indexed)) = SAVER.get() else { return };
    let mut saved_at = SAVED_AT.lock();
    let now = super::ntfs::CHANGES.load(Ordering::Relaxed);
    if now == *saved_at {
        return;
    }
    *saved_at = now;
    for vol in indexed {
        let Some(journal) = journal_state(vol.drive) else {
            continue;
        };
        // written straight from the live index under a read lock: searches go on, no copy of the index. Saved
        // with the position the watcher has applied, not the journal's end: the watcher trails the end, and
        // changes in between (a delete) would never be replayed.
        let catalog = catalog.read();
        if let Some((index, usn)) = catalog.volume(vol.drive).and_then(|i| Some((i, i.usn?))) {
            // under the read lock: the changes taken are exactly those up to `usn` (the watcher applies under the write lock)
            let appended = index.take_unsaved().is_some_and(|c| matches!(cache::append(vol.drive, usn, &c), Ok(true)));
            if !appended {
                index.saved_whole(cache::save(vol.drive, vol.serial, journal.journal_id, usn, index.parts()).is_ok());
                let _ = crate::datestore::save(); // its changes are replayed from the log meanwhile
            }
        }
    }
}

fn spawn_cache_saver(catalog: Arc<RwLock<Catalog>>, indexed: Vec<IndexedVolume>) {
    *SAVED_AT.lock() = super::ntfs::CHANGES.load(Ordering::Relaxed);
    if SAVER.set((catalog, indexed)).is_err() {
        return; // one indexer per process
    }
    thread::spawn(|| loop {
        thread::sleep(CACHE_SAVE_INTERVAL);
        save_caches();
    });
}
