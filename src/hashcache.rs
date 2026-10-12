//! Content-hash cache backing `md5:`/`sha1:`/`sha256:`/`crc32:` and `dupe:`. Kept per path and
//! mtime in %LOCALAPPDATA%\Mosaic\hashcache.bin (same shape as `metadata.rs`); a sync only reads
//! new or changed files, hashing all four algorithms together in one read pass.
//! ponytail: files over HASH_MAX_BYTES are skipped by the background sync — not worth hashing a
//! multi-GB file on every drive just to enable dupe/hash search on it. Raise the cap if needed.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use md5::Md5;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::application::SearchOptions;
use crate::backend::SearchHandle;

const SKIP: &str = r"!path:\appdata\ !path:c:\windows\ !path:$recycle.bin";
const HASH_MAX_BYTES: i64 = 200 << 20;
const BATCH: usize = 64;
fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(2))
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Hashes {
    pub md5: [u8; 16],
    pub sha1: [u8; 20],
    pub sha256: [u8; 32],
    pub crc32: u32,
}

/// path (lowercased) → (mtime ms, size, hashes)
type Files = HashMap<String, (i64, i64, Hashes)>;

static FILES: RwLock<Option<Files>> = RwLock::new(None);
static GEN: AtomicUsize = AtomicUsize::new(0);
static PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static SYNCED: Mutex<Option<Instant>> = Mutex::new(None);
static ERROR: Mutex<Option<String>> = Mutex::new(None);

fn store() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("hashcache.bin")
}

pub fn generation() -> usize {
    GEN.load(Ordering::Relaxed)
}
/// Bring the cache up to date in the background: at most once a minute, never twice at a time.
/// Only called when a query actually uses a hash/dupe function.
pub fn sync(h: SearchHandle) {
    {
        let mut s = SYNCED.lock();
        if PROGRESS.lock().is_some() || s.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *s = Some(Instant::now());
        *PROGRESS.lock() = Some((0, 0));
    }
    std::thread::spawn(move || {
        let r = run(&h);
        *ERROR.lock() = r.err();
        *PROGRESS.lock() = None;
        GEN.fetch_add(1, Ordering::Relaxed);
    });
}

fn run(h: &SearchHandle) -> Result<(), String> {
    if FILES.read().is_none() {
        let saved: Files = std::fs::read(store()).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default();
        *FILES.write() = Some(saved);
        GEN.fetch_add(1, Ordering::Relaxed);
    }
    let mut now: HashMap<String, (i64, i64)> = HashMap::new();
    for offset in (0..).step_by(100_000) {
        let opts = SearchOptions { query: format!("size:<{HASH_MAX_BYTES} {SKIP}"), limit: 100_000, offset, ..SearchOptions::default() };
        let r = h.search(&opts);
        if let Some(e) = r.error {
            return Err(e);
        }
        let n = r.hits.len();
        now.extend(r.hits.into_iter().filter(|x| !x.is_dir).map(|x| (x.path.to_lowercase(), (x.modified, x.size))));
        if n < opts.limit {
            break;
        }
    }
    let todo: Vec<String> = {
        let mut g = FILES.write();
        let files = g.as_mut().unwrap();
        files.retain(|p, _| now.contains_key(p));
        now.iter().filter(|(p, (m, _))| files.get(*p).is_none_or(|e| e.0 != *m)).map(|(p, _)| p.clone()).collect()
    };
    *PROGRESS.lock() = Some((0, todo.len()));
    let queue = Mutex::new(todo.chunks(BATCH).collect::<Vec<_>>());
    let saved = Mutex::new(Instant::now());
    std::thread::scope(|s| {
        for _ in 0..workers() {
            let (queue, now, saved) = (&queue, &now, &saved);
            s.spawn(move || loop {
                let next = queue.lock().pop();
                let Some(chunk) = next else { break };
                // hashed before taking the lock: reading 64 files held every hash/dupe search (and the other
                // workers) behind it
                let done: Vec<(&String, Hashes)> = chunk.iter().filter_map(|p| Some((p, hash_file(p)?))).collect();
                let mut g = FILES.write();
                let files = g.as_mut().unwrap();
                for (p, hashes) in done {
                    let (m, size) = now[p];
                    files.insert(p.clone(), (m, size, hashes));
                }
                drop(g);
                if let Some(pr) = PROGRESS.lock().as_mut() {
                    pr.0 += chunk.len();
                }
                GEN.fetch_add(1, Ordering::Relaxed);
                let mut s = saved.lock();
                if s.elapsed() > Duration::from_secs(30) {
                    *s = Instant::now();
                    let _ = save();
                }
            });
        }
    });
    save()
}

fn save() -> Result<(), String> {
    let bytes = bincode::serialize(FILES.read().as_ref().unwrap()).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(store().parent().unwrap());
    let tmp = store().with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, store()).map_err(|e| e.to_string())
}

/// Read `path` once, feeding all four hashers at the same time.
fn hash_file(path: &str) -> Option<Hashes> {
    let mut f = std::fs::File::open(path).ok()?;
    let (mut md5, mut sha1, mut sha256, mut crc) = (Md5::new(), Sha1::new(), Sha256::new(), crc32fast::Hasher::new());
    let mut buf = [0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
        sha256.update(&buf[..n]);
        crc.update(&buf[..n]);
    }
    Some(Hashes { md5: md5.finalize().into(), sha1: sha1.finalize().into(), sha256: sha256.finalize().into(), crc32: crc.finalize() })
}

pub fn get(path: &str) -> Option<Hashes> {
    FILES.read().as_ref()?.get(&path.to_lowercase()).map(|(_, _, h)| h.clone())
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Which of the four algorithms a `md5:`/`sha1:`/`sha256:`/`crc32:` token asked for, and the
/// lowercased hex value to match.
#[derive(Clone, PartialEq, Debug)]
pub enum HashQuery {
    Md5(String),
    Sha1(String),
    Sha256(String),
    Crc32(String),
    /// `dupe:` — no value, just "only files with an identical twin".
    Dupe,
}

/// Pull a `md5:`/`sha1:`/`sha256:`/`crc32:`/`dupe:` token out of `query`, returning the remaining
/// query and the parsed function (the last one wins if more than one appears — combining two hash
/// functions or hash+dupe in one query isn't supported, matching `distinct:`'s single-field rule).
pub fn extract_hash(query: &str) -> (String, Option<HashQuery>) {
    let mut found = None;
    let mut rest = Vec::new();
    for word in query.split_whitespace() {
        let hq = [("md5:", HashQuery::Md5 as fn(String) -> HashQuery), ("sha1:", HashQuery::Sha1), ("sha256:", HashQuery::Sha256), ("crc32:", HashQuery::Crc32)]
            .into_iter()
            .find_map(|(prefix, wrap)| word.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| wrap(word[prefix.len()..].to_lowercase())));
        if let Some(hq) = hq {
            if !matches!(&hq, HashQuery::Md5(v) | HashQuery::Sha1(v) | HashQuery::Sha256(v) | HashQuery::Crc32(v) if v.is_empty()) {
                found = Some(hq);
            }
            continue;
        }
        if word.eq_ignore_ascii_case("dupe:") {
            found = Some(HashQuery::Dupe);
            continue;
        }
        rest.push(word);
    }
    (rest.join(" "), found)
}

/// Whether `path`'s cached hash matches a `md5:`/`sha1:`/`sha256:`/`crc32:` query. A path not yet
/// hashed by the background sync never matches (same "empty until synced" behavior as `metadata.rs`).
fn hash_matches(path: &str, hq: &HashQuery) -> bool {
    let Some(h) = get(path) else { return false };
    match hq {
        HashQuery::Md5(v) => &to_hex(&h.md5) == v,
        HashQuery::Sha1(v) => &to_hex(&h.sha1) == v,
        HashQuery::Sha256(v) => &to_hex(&h.sha256) == v,
        HashQuery::Crc32(v) => format!("{:08x}", h.crc32) == *v,
        HashQuery::Dupe => false, // handled separately by `dupe_paths`, not a per-path hash compare
    }
}

/// Every path that's part of a same-size, same-sha256 group of two or more, among already-hashed
/// files. `dupe:` filters candidates down to this set.
pub fn dupe_paths() -> std::collections::HashSet<String> {
    let g = FILES.read();
    let Some(files) = g.as_ref() else { return Default::default() };
    let mut by_size: HashMap<i64, Vec<(&String, &Hashes)>> = HashMap::new();
    for (p, (_, size, h)) in files.iter() {
        by_size.entry(*size).or_default().push((p, h));
    }
    let mut out = std::collections::HashSet::new();
    for members in by_size.into_values() {
        if members.len() < 2 {
            continue;
        }
        let mut by_hash: HashMap<[u8; 32], Vec<&String>> = HashMap::new();
        for (p, h) in members {
            by_hash.entry(h.sha256).or_default().push(p);
        }
        for group in by_hash.into_values().filter(|g| g.len() > 1) {
            out.extend(group.into_iter().cloned());
        }
    }
    out
}

/// Whether `path` (any case) should be kept for a hash/dupe query.
pub fn matches(path: &str, hq: &HashQuery) -> bool {
    match hq {
        HashQuery::Dupe => {
            // grouped once per cache generation, not once per candidate (that was every hashed file, per row)
            static DUPES: Mutex<Option<(usize, std::sync::Arc<std::collections::HashSet<String>>)>> = Mutex::new(None);
            let gen = generation();
            let set = {
                let mut d = DUPES.lock();
                match d.as_ref().filter(|d| d.0 == gen) {
                    Some(d) => d.1.clone(),
                    None => d.insert((gen, std::sync::Arc::new(dupe_paths()))).1.clone(),
                }
            };
            set.contains(&path.to_lowercase())
        }
        _ => hash_matches(path, hq),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_hash_pulls_the_function_and_cleans_the_query() {
        let (rest, hq) = extract_hash("report crc32:deadbeef memo");
        assert_eq!(rest, "report memo");
        assert_eq!(hq, Some(HashQuery::Crc32("deadbeef".into())));

        let (rest, hq) = extract_hash("dupe: ext:jpg");
        assert_eq!(rest, "ext:jpg");
        assert_eq!(hq, Some(HashQuery::Dupe));

        let (rest, hq) = extract_hash("just a name search");
        assert_eq!(rest, "just a name search");
        assert_eq!(hq, None);

        // A bare `md5:` (no value) is dropped, like an empty `content:`.
        let (rest, hq) = extract_hash("md5: report");
        assert_eq!(rest, "report");
        assert_eq!(hq, None);
    }

    #[test]
    fn hash_file_matches_hand_verified_crc32_of_known_bytes() {
        let dir = std::env::temp_dir().join(format!("rn-hashcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let h = hash_file(path.to_str().unwrap()).unwrap();
        // crc32("hello world") is a well-known test vector.
        assert_eq!(h.crc32, 0x0d4a1185);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dupe_paths_groups_by_size_then_hash() {
        *FILES.write() = Some(HashMap::from([
            ("c:\\a.txt".into(), (0, 100, Hashes { sha256: [1; 32], ..Default::default() })),
            ("c:\\b.txt".into(), (0, 100, Hashes { sha256: [1; 32], ..Default::default() })), // same size+hash as a
            ("c:\\c.txt".into(), (0, 100, Hashes { sha256: [2; 32], ..Default::default() })), // same size, different hash
            ("c:\\d.txt".into(), (0, 200, Hashes { sha256: [1; 32], ..Default::default() })), // different size
        ]));
        let dupes = dupe_paths();
        assert!(dupes.contains("c:\\a.txt"));
        assert!(dupes.contains("c:\\b.txt"));
        assert!(!dupes.contains("c:\\c.txt"));
        assert!(!dupes.contains("c:\\d.txt"));
        *FILES.write() = None;
    }
}
