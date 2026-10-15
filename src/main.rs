#![windows_subsystem = "windows"]
//! Mosaic: dark UI on an NTFS MFT/USN search engine. The engine (application/, domain/, infrastructure/) is
//! adapted from AllTheThings (MIT, Swatto); the window is drawn natively (native*.rs).
mod application;
mod backend;
mod contentindex;
mod datestore;
mod direct;
mod extract;
mod hashcache;
mod i18n;
mod infrastructure;
mod kvlog;
mod metadata;
mod recent;
mod native;
mod shell;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;

use application::search::SortKey;
use application::SearchOptions;
use backend::SearchBackend;
use infrastructure::icons;
use infrastructure::service::{host, scm};

const SVC_DIR: &str = r"C:\Program Files\Mosaic";

// ---- categories and type words ------------------------------------------------------------------------------
const CATS: &[(&str, &str, &str)] = &[
    ("images", "jpg jpeg png gif bmp webp svg ico tif tiff heic psd avif raw", "images image pictures"),
    ("video", "mp4 mkv avi mov wmv flv webm m4v mpg mpeg", "video videos movies"),
    ("audio", "mp3 wav flac aac ogg m4a wma opus", "audio music sound"),
    ("documents", "pdf doc docx xls xlsx ppt pptx txt rtf odt ods odp md csv epub", "documents document"),
    ("archives", "zip rar 7z tar gz bz2 xz iso cab", "archives archive compressed"),
    ("executables", "exe msi bat cmd com scr", "executables executable programs"),
    ("code", "py js ts tsx jsx c h cpp hpp cs java go rs rb php html css json xml yml yaml sh asm", "code source script"),
];
const FOLDER_WORDS: &str = "folder folders directory file folder";

/// Every extension registered with Windows, with the words a type search matches: ext + type name + category words.
fn ext_info() -> &'static Vec<(String, String)> {
    static INFO: OnceLock<Vec<(String, String)>> = OnceLock::new();
    INFO.get_or_init(|| {
        let mut exts: Vec<String> = registered_exts();
        for (_, list, _) in CATS {
            exts.extend(list.split(' ').map(String::from));
        }
        exts.sort();
        exts.dedup();
        exts.into_iter()
            .map(|e| {
                let cat = CATS.iter().find(|c| c.1.split(' ').any(|x| x == e)).map(|c| c.2).unwrap_or("");
                let tn = icons::type_name(Some(&e), false).unwrap_or_default().to_lowercase();
                let info = format!("{e} {tn} {cat}");
                (e, info)
            })
            .collect()
    })
}

fn registered_exts() -> Vec<String> {
    use windows_sys::Win32::System::Registry::{RegEnumKeyExW, HKEY_CLASSES_ROOT};
    let mut out = Vec::new();
    for i in 0.. {
        let mut buf = [0u16; 128];
        let mut len = buf.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(HKEY_CLASSES_ROOT, i, buf.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        if rc == 259 {
            break; // ERROR_NO_MORE_ITEMS
        }
        if rc == 0 && buf[0] == b'.' as u16 {
            let name = String::from_utf16_lossy(&buf[1..len as usize]).to_lowercase();
            if !name.is_empty() && name.len() <= 12 && name.chars().all(|c| c.is_ascii_alphanumeric()) {
                out.push(name);
            }
        }
    }
    out
}

const OPS: &[&str] = &["ext:", "size:", "dm:", "dc:", "da:", "date:", "path:", "file:", "folder:", "attrib:", "content:"];

/// Turn the UI's query + chip + metadata mode into an engine query.
fn build_query(q: &str, cat: &str, meta: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#"[^\s"]*"[^"]*"|\S+"#).unwrap());
    let re = &*RE;
    let mut parts: Vec<String> = Vec::new();
    for m in re.find_iter(q) {
        let t = m.as_str();
        let lower = t.to_lowercase();
        let plain = !t.starts_with('"') && !t.contains(['|', '!', '(', ')', '*', '?', ':']) && !OPS.iter().any(|o| lower.starts_with(o));
        if !meta.is_empty() && plain {
            let mut exts: Vec<&str> = ext_info().iter().filter(|(_, info)| info.contains(&lower)).map(|(e, _)| e.as_str()).collect();
            exts.truncate(400);
            let mut alts: Vec<String> = Vec::new();
            if meta == "both" {
                alts.push(t.to_string());
            }
            if !exts.is_empty() {
                alts.push(format!("ext:{}", exts.join(";")));
            }
            if FOLDER_WORDS.contains(lower.as_str()) {
                alts.push("folder:".into());
            }
            if alts.is_empty() {
                alts.push("ext:zzqzzq".into()); // nothing has this type: match nothing
            }
            parts.push(format!("({})", alts.join(" | ")));
        } else {
            parts.push(t.to_string());
        }
    }
    let mut out = parts.join(" ");
    let filter = match cat {
        "" => None,
        "folders" => Some("folder:".to_string()),
        c => CATS.iter().find(|x| x.0 == c).map(|x| format!("ext:{}", x.1.replace(' ', ";"))),
    };
    if let Some(f) = filter {
        out = if out.trim().is_empty() { f } else { format!("({out}) {f}") };
    }
    out
}

// ---- search command -----------------------------------------------------------------------------------------
#[derive(Deserialize, Clone, PartialEq)]
struct SearchArgs {
    q: String,
    cat: String,
    sort: String,
    dir: String,
    meta: String,
    /// Metadata / Content toggles (native window): search the content index instead of names
    #[serde(default)]
    tags: bool,
    #[serde(default)]
    content: bool,
    #[serde(default)]
    match_case: bool,
    #[serde(default)]
    whole_word: bool,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    diacritics: bool,
    #[serde(default)]
    hidden: bool,
    system: bool,
}

fn sort_key(s: &str) -> SortKey {
    match s {
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        "mtime" => SortKey::Modified,
        "type" => SortKey::Ext,
        "attributes" => SortKey::Attributes,
        "created" => SortKey::Created, // populated once Phase 4's opt-in side-store is on; Equal until then
        "accessed" => SortKey::Accessed,
        _ => SortKey::Name,
    }
}

fn options(a: &SearchArgs, offset: usize, limit: usize) -> SearchOptions {
    SearchOptions {
        query: build_query(&a.q, &a.cat, &a.meta),
        limit,
        offset,
        sort: sort_key(&a.sort),
        ascending: a.dir != "desc",
        match_path: a.q.contains('\\'),
        folders_first: true,  // folders and folder shortcuts above files, like Explorer
        match_case: a.match_case,
        whole_word: a.whole_word,
        regex: a.regex,
        diacritics: a.diacritics,
        show_hidden: a.hidden,
        show_system: a.system,
        ..SearchOptions::default()
    }
}

// ---- Content: the in-process Tantivy index (contentindex.rs); Metadata: metadata.rs ----------------------------
/// ponytail: hits kept per query; raise it if someone needs more
const CONTENT_MAX: usize = 5000;

/// Files matching `query` in the content index that still exist, as index hits.
fn content_query(query: &str, h: &backend::SearchHandle) -> Result<Vec<application::index::Hit>, String> {
    use rayon::prelude::*;
    // one stat per hit, in parallel: thousands of them one after another took a good part of a second
    Ok(h.content(query, CONTENT_MAX)?.into_par_iter().filter_map(|p| {
        let m = std::fs::metadata(&p).ok()?; // gone since it was indexed
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        Some(application::index::Hit {
            name: p.rsplit('\\').next().unwrap_or(&p).to_string(),
            size: if m.is_dir() { -1 } else { m.len() as i64 },
            modified,
            created: 0,
            accessed: 0,
            attributes: 0,
            is_dir: m.is_dir(),
            path: p,
        })
    }).collect())
}

/// Shared by [`content_progress`] and [`metadata_progress`]: turns a raw (done, total) reading into
/// one with an ETA, estimated from the rate over the last ~20s of samples (not since the run started),
/// so it tracks the current pace (big files slowing things down, a fast run of small ones) instead of
/// a stale average. `samples` is the caller's own static — each progress source needs its own history.
fn eta_progress(current: Option<(usize, usize)>, samples: &Mutex<VecDeque<(Instant, usize)>>) -> Option<(usize, usize, Option<Duration>)> {
    const WINDOW: Duration = Duration::from_secs(20);

    let Some((done, total)) = current else {
        samples.lock().clear();
        return None;
    };
    let mut s = samples.lock();
    if s.back().is_some_and(|&(_, last)| done < last) {
        s.clear(); // a new run started over
    }
    let now = Instant::now();
    if s.back().is_none_or(|&(_, last)| done != last) {
        s.push_back((now, done));
    }
    while s.len() > 1 && now.duration_since(s[0].0) > WINDOW {
        s.pop_front();
    }
    let eta = s.front().filter(|&&(at, first)| now.duration_since(at).as_secs_f64() > 2.0 && done > first).map(|&(at, first)| {
        let rate = (done - first) as f64 / now.duration_since(at).as_secs_f64(); // items/sec, recent pace only
        Duration::from_secs_f64((total - done) as f64 / rate)
    });
    Some((done, total, eta))
}

/// (files scanned, files to scan, ETA) while the content index's own background indexer
/// (`contentindex::bulk_index`) is still on its first pass over the corpus. `(done, total)` comes
/// from whichever process actually runs that scan — read straight from `contentindex::progress()`
/// when this process is the one indexing (no service installed), or relayed over IPC in
/// `IndexStatus::content_done`/`content_total` when a background service does it instead; either
/// way the caller already has the latest `IndexStatus` from its own poll, so it's passed in rather
/// than re-read here. `None` once it's done (or before it starts) — nothing to report.
pub fn content_progress(done: usize, total: usize) -> Option<(usize, usize, Option<Duration>)> {
    static SAMPLES: Mutex<VecDeque<(Instant, usize)>> = Mutex::new(VecDeque::new());
    eta_progress((total > 0 && done < total).then_some((done, total)), &SAMPLES)
}

/// (files read, files to read, ETA) while `metadata::sync` is running ExifTool over the candidate
/// set. Once it's done there's nothing to report.
pub fn metadata_progress() -> Option<(usize, usize, Option<Duration>)> {
    static SAMPLES: Mutex<VecDeque<(Instant, usize)>> = Mutex::new(VecDeque::new());
    eta_progress(metadata::progress(), &SAMPLES)
}

/// A search as the toggles ask: names from the NTFS index, or with Metadata / Content on, files whose metadata or
/// text (content index) matches, filtered by the category and sorted like the list. The last such result is kept: the list
/// pages through it.
/// Explorer's Home: the Quick access folders (pinned, then frequent), cached for a minute.
/// Matched as `parent:"<folder>\x"`, which selects the item whose path is exactly that folder.
static HOME: Mutex<Option<(Instant, String)>> = Mutex::new(None);
fn home_query() -> String {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    // Asking the shell costs a PowerShell start (about a second), so the last answer lives on disk and in memory
    // and is returned at once; once over a minute old, a background thread refreshes it for the next visit.
    // ponytail: a Quick access change shows one visit late.
    let file = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("home.txt");
    let known = HOME.lock().clone().or_else(|| std::fs::read_to_string(&file).ok().filter(|q| !q.is_empty()).map(|q| (Instant::now().checked_sub(Duration::from_secs(61)).unwrap_or_else(Instant::now), q)));
    if let Some((t, q)) = known {
        *HOME.lock() = Some((t, q.clone()));
        if t.elapsed() > Duration::from_secs(60) && !BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::thread::spawn(|| {
                home_refresh();
                BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        }
        return q;
    }
    home_refresh()
}

fn home_refresh() -> String {
    use std::os::windows::process::CommandExt;
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; (New-Object -ComObject Shell.Application).Namespace('shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}').Items() | % { $_.Path }";
    let out = std::process::Command::new("powershell").args(["-NoProfile", "-Command", script]).creation_flags(0x0800_0000).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    // drive roots (`G:\`) are not index entries, so they can't be listed
    let mut parts: Vec<String> = out.lines().map(str::trim).filter(|p| p.len() > 3 && p.as_bytes()[1] == b':').map(|p| format!("parent:\"{p}\\x\"")).collect();
    if parts.is_empty() {
        parts.push(format!("child:\"{}\"", std::env::var("USERPROFILE").unwrap_or_default()));
    } else {
        // then the 20 most recently opened files, as Explorer's Home does
        parts.extend(recent::list(false, 20).into_iter().filter(|p| p.as_bytes().get(1) == Some(&b':')).map(|p| format!("parent:\"{p}\\x\"")));
    }
    let q = parts.join(" | ");
    let file = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("home.txt");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(q.as_str()) {
        let _ = std::fs::write(file, &q); // refreshed every visit: only written when Quick access actually changed
    }
    *HOME.lock() = Some((Instant::now(), q.clone()));
    q
}

/// What every file directly inside `path` is: 1 pictures, 2 videos, 3 music, 4 documents; 0 when mixed, unknown or empty.
/// Samples the first 400 entries, so a huge folder costs no more than a small one.
fn folder_kind(h: &backend::SearchHandle, path: &str) -> u8 {
    let kind_of = |name: &str| match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "svg" | "ico") => 1,
        Some("mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" | "m4v" | "mpg" | "mpeg") => 2,
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma" | "opus") => 3,
        Some("txt" | "md" | "rtf" | "doc" | "docx" | "odt" | "ppt" | "pptx" | "odp" | "xls" | "xlsx" | "ods" | "pdf" | "csv") => 4,
        _ => 0,
    };
    let r = h.search(&SearchOptions { query: format!("child:\"{path}\""), limit: 400, ..SearchOptions::default() });
    let mut kind = 0;
    for hit in r.hits.iter().filter(|x| !x.is_dir && !x.name.eq_ignore_ascii_case("desktop.ini") && !x.name.eq_ignore_ascii_case("thumbs.db")) {
        let k = kind_of(&hit.name);
        if k == 0 || (kind != 0 && k != kind) {
            return 0;
        }
        kind = k;
    }
    kind
}

fn find(a: &SearchArgs, offset: usize, limit: usize, h: &backend::SearchHandle) -> application::SearchResult {
    let q = match a.q.trim() {
        "home:" => Some(home_query()),
        _ => None,
    };
    if let Some(q) = q {
        return h.search(&options(&SearchArgs { q, ..a.clone() }, offset, limit));
    }
    if let Some(r) = direct::find(&a.q, &options(a, offset, limit)) {
        return r; // a share, Linux, a stick: the index doesn't hold it
    }
    type Hits = std::sync::Arc<Vec<application::index::Hit>>;
    static LAST: Mutex<Option<(String, Hits, Option<String>)>> = Mutex::new(None);
    let (clean_q, hashq) = hashcache::extract_hash(&a.q);
    let (clean_q, fieldq) = metadata::extract_field(&clean_q);
    // a folder listing (Browse, or typed child:/descendant:) is a name search: the content index has no such field
    let q = a.q.trim_start().to_ascii_lowercase();
    let folder_listing = q.starts_with("child:") || q.starts_with("descendant:");
    if folder_listing || (!a.tags && !a.content && hashq.is_none() && fieldq.is_none()) {
        return h.search(&options(a, offset, limit));
    }
    if a.tags || fieldq.is_some() {
        metadata::sync(h.clone());
    }
    if hashq.is_some() {
        hashcache::sync(h.clone());
    }
    let hq_key = hashq.as_ref().map_or(String::new(), |q| format!("{q:?}"));
    let fq_key = fieldq.as_ref().map_or(String::new(), |(t, f)| format!("{t}{f:?}"));
    let key = format!("{}{}{}{}\0{}\0{}\0{}\0{}\0{}\0{}", a.tags, a.content, metadata::generation(), hashcache::generation(), clean_q, a.cat, a.sort, a.dir, hq_key, fq_key);
    let cached = LAST.lock().as_ref().filter(|c| c.0 == key).map(|c| (c.1.clone(), c.2.clone()));
    let (hits, error) = cached.unwrap_or_else(|| {
        let (mut v, mut error) = (Vec::new(), None);
        if a.content && !clean_q.trim().is_empty() {
            match content_query(&clean_q, h) {
                Ok(r) => v = r,
                Err(e) => error = Some(e),
            }
        }
        if a.tags {
            let mut seen: std::collections::HashSet<String> = v.iter().map(|h| h.path.clone()).collect();
            v.extend(metadata::search(&clean_q).into_iter().filter(|h| seen.insert(h.path.clone())));
            error = error.or_else(metadata::error);
        }
        if !a.tags && !a.content {
            // A hash/dupe/field-only query (no Metadata/Content toggle on): source candidates from the
            // name index itself, over a bounded pool (matches `CONTENT_MAX`'s spirit).
            let mut a2 = a.clone();
            a2.q = clean_q.clone();
            let r = h.search(&options(&a2, 0, 200_000));
            v = r.hits;
            error = r.error;
        }
        match a.cat.as_str() {
            "" => {}
            "folders" => v.retain(|h| h.is_dir),
            c => {
                let exts = CATS.iter().find(|x| x.0 == c).map_or("", |x| x.1);
                v.retain(|h| ext_of(&h.name).is_some_and(|e| exts.split(' ').any(|x| x == e)));
            }
        }
        if let Some(hq) = &hashq {
            v.retain(|h| hashcache::matches(&h.path, hq));
        }
        if let Some((tag, filter)) = &fieldq {
            v.retain(|h| metadata::field_matches(&h.path, tag, filter));
        }
        match a.sort.as_str() {
            "path" => v.sort_by_cached_key(|h| h.path.to_lowercase()),
            "size" => v.sort_by_key(|h| h.size),
            "mtime" => v.sort_by_key(|h| h.modified),
            "type" => v.sort_by_cached_key(|h| (ext_of(&h.name), h.name.to_lowercase())),
            "attributes" => v.sort_by_key(|h| h.attributes),
            "created" => v.sort_by_key(|h| h.created),
            "accessed" => v.sort_by_key(|h| h.accessed),
            _ => v.sort_by_cached_key(|h| h.name.to_lowercase()),
        }
        if a.dir == "desc" {
            v.reverse();
        }
        v.sort_by_key(|h| !h.is_dir); // stable: folders first, as the name index does
        let hits: Hits = v.into();
        *LAST.lock() = Some((key, hits.clone(), error.clone()));
        (hits, error)
    });
    application::SearchResult { total: hits.len(), took_ms: 0, hits: hits.iter().skip(offset).take(limit).cloned().collect(), error, capped: false, stamp: None }
}

fn ext_of(name: &str) -> Option<String> {
    name.rfind('.').filter(|&i| i > 0).map(|i| name[i + 1..].to_lowercase())
}

fn type_cache(ext: &Option<String>, dir: bool) -> String {
    static C: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let key = if dir { "\0dir".to_string() } else { ext.clone().unwrap_or_default() };
    let mut m = C.get_or_init(Default::default).lock();
    m.entry(key).or_insert_with(|| icons::type_name(ext.as_deref(), dir).unwrap_or_else(|| "File".into())).clone()
}

// ---- entry point --------------------------------------------------------------------------------------------
/// Elevated one-shots: the exe is copied under Program Files (admin-writable only) before it is registered as the
/// SYSTEM service, so a normal user can't swap the binary the service runs.
fn admin(arg: &str) -> i32 {
    let r = match arg {
        "--svc-setup" => (|| {
            let _ = scm::stop();
            std::fs::create_dir_all(SVC_DIR).map_err(|e| e.to_string())?;
            let dst = PathBuf::from(SVC_DIR).join("mosaic-index.exe");
            let me = std::env::current_exe().map_err(|e| e.to_string())?;
            // its own name and icon in Task Manager
            let copy = || native::stamp(&me, &dst, "Mosaic Filename Index", native::index_icons(std::path::Path::new(SVC_DIR)));
            let mut copied = copy();
            for _ in 0..40 {
                // the stopped service's process can hold the exe open for a moment after it reports Stopped
                if copied.is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
                copied = copy();
            }
            copied.map_err(|e| e.to_string())?;
            scm::install(dst)?;
            scm::start()
        })(),
        "--svc-uninstall" => scm::uninstall(),
        "--svc-start" => scm::start(),
        "--svc-stop" => scm::stop(),
        _ => return 2,
    };
    i32::from(r.is_err())
}

/// `mosaic.exe --cli <query...>`: a thin, script-friendly client sharing `SearchBackend` with the
/// GUI (service pipe if the background service is up, else the same in-process index the GUI would build).
/// A GUI-subsystem exe has no console by default, so this attaches the caller's (or allocates a fresh one)
/// and writes straight to CONOUT$ — println!/stdout stay disconnected even after AttachConsole.
/// ponytail: one flat flag set (--status/--limit/--case/--word/--regex/--path), no subcommands; the query
/// syntax itself (ext:/size:/dm:/regex/etc., see application::search) already covers everything else.
mod cli {
    use std::io::Write;

    unsafe fn console_out() -> Option<std::fs::File> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::CreateFileW;
        use windows_sys::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const OPEN_EXISTING: u32 = 3;
        let h = CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        (h != INVALID_HANDLE_VALUE).then(|| std::fs::File::from_raw_handle(h as _))
    }

    /// Pure so it's testable without a console or a search backend.
    fn parse_args(args: &[String]) -> (crate::SearchOptions, bool) {
        let mut opts = crate::SearchOptions { limit: 100, ..Default::default() };
        let mut status_only = false;
        let mut words = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--status" => status_only = true,
                "--limit" => opts.limit = it.next().and_then(|s| s.parse().ok()).unwrap_or(opts.limit),
                "--case" => opts.match_case = true,
                "--word" => opts.whole_word = true,
                "--regex" => opts.regex = true,
                "--path" => opts.match_path = true,
                _ => words.push(a.as_str()),
            }
        }
        opts.query = words.join(" ");
        (opts, status_only)
    }

    pub fn run(args: &[String]) -> i32 {
        let Some(mut out) = (unsafe { console_out() }) else { return 1 };
        let (opts, status_only) = parse_args(args);
        let backend = crate::SearchBackend::detect();
        if status_only {
            let s = backend.status();
            let _ = writeln!(out, "{}\t{}\t{}", s.state, s.count, s.message);
            return 0;
        }
        let res = backend.search(&opts);
        for h in &res.hits {
            let _ = writeln!(out, "{}", h.path);
        }
        if let Some(e) = res.error {
            let _ = writeln!(out, "error: {e}");
            return 1;
        }
        0
    }

    #[cfg(test)]
    mod tests {
        use super::parse_args;

        fn a(words: &[&str]) -> Vec<String> {
            words.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn flags_set_options_and_bare_words_join_into_the_query() {
            let (opts, status) = parse_args(&a(&["--case", "foo", "--word", "bar", "baz"]));
            assert!(!status);
            assert!(opts.match_case);
            assert!(opts.whole_word);
            assert!(!opts.regex);
            assert_eq!(opts.query, "foo bar baz");
            assert_eq!(opts.limit, 100); // default, no --limit given
        }

        #[test]
        fn limit_and_status_parse() {
            let (opts, status) = parse_args(&a(&["--status", "--limit", "5", "q"]));
            assert!(status);
            assert_eq!(opts.limit, 5);
            assert_eq!(opts.query, "q");
        }

        #[test]
        fn a_malformed_limit_keeps_the_default_instead_of_panicking() {
            let (opts, _) = parse_args(&a(&["--limit", "not-a-number"]));
            assert_eq!(opts.limit, 100);
        }
    }
}

fn main() {
    // out-of-process content extraction: a misbehaving IFilter DLL takes down this throwaway
    // child, not the indexer — same crash-isolation reasoning as IPreviewHandler in native_preview.rs
    if let Some(k) = std::env::args().position(|a| a == "--extract") {
        let path = std::env::args().nth(k + 1).unwrap_or_default();
        extract::confine_worker();
        if let Some(text) = extract::full_text(&path) {
            print!("{text}");
        }
        std::process::exit(0);
    }
    if let Some(k) = std::env::args().position(|a| a == "--debug-content") {
        let q = std::env::args().nth(k + 1).unwrap_or_default();
        // the same route the GUI takes: through the service when one is running, else in-process
        let hits = match infrastructure::service::client::ServiceClient::probe() {
            Some(c) => backend::SearchHandle::Service(c).content(&q, 20),
            None => contentindex::search(&q, 20).map(|v| v.into_iter().map(|(p, _)| p).collect()),
        };
        let hits = hits.unwrap_or_else(|e| {
            eprintln!("DEBUG_CONTENT_ERROR={e}");
            std::process::exit(1);
        });
        eprintln!("DEBUG_CONTENT_HITS={}", hits.len());
        for p in &hits {
            eprintln!("  {p}");
        }
        std::process::exit(0);
    }
    if let Some(k) = std::env::args().position(|a| a == "--cli") {
        let args: Vec<String> = std::env::args().skip(k + 1).collect();
        std::process::exit(cli::run(&args));
    }
    if let Some(k) = std::env::args().position(|a| a == "--dist") {
        let dir = std::env::args().nth(k + 1).unwrap_or_else(|| "dist".into());
        let choice: Vec<(String, String)> = std::env::args().skip(k + 2).filter_map(|a| a.split_once('=').map(|(r, n)| (r.to_string(), n.to_string()))).collect();
        std::process::exit(i32::from(native::dist(std::path::Path::new(&dir), &choice).is_err()));
    }
    if std::env::args().any(|a| a == "--dialogs") {
        native::save::watch_main();
        return;
    }
    if std::env::args().any(|a| a == "--shell-menu") {
        shell::menu_main();
        return;
    }
    if std::env::args().any(|a| a == "--shell-helper") {
        shell::helper_main();
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--service") {
        let _ = host::run();
        return;
    }
    if let Some(a) = args.get(1).filter(|a| a.starts_with("--svc-")) {
        std::process::exit(admin(a));
    }
    // the window's type search reads this; the service and the one-shots above never do
    std::thread::spawn(|| {
        ext_info();
    });
    let named_native = std::env::current_exe().is_ok_and(|p| p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("mosaic")));
    if named_native || args.iter().any(|a| a == "--native") {
        native::app_id("local.mosaic.native");
        native::run();
        return;
    }
    // started under another name (a dev build): run as mosaic.exe, a copy with its own name and icon
    {
        match native::native_exe().and_then(|p| std::process::Command::new(p).args(&args[1..]).spawn()) {
            Ok(_) => return,
            Err(_) => {
                // exe copy failed (read-only folder, or the copy is running and locked): run here instead
                native::app_id("local.mosaic.native");
                native::run();
                return;
            }
        }
    }
}
