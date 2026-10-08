#![windows_subsystem = "windows"]
//! Mosaic: dark UI on an NTFS MFT/USN search engine. The engine (application/, domain/, infrastructure/) is
//! adapted from AllTheThings (MIT, Swatto); the UI is ../static, served by Tauri, talking to it over IPC.
mod application;
mod backend;
mod contentindex;
mod datestore;
mod extract;
mod hashcache;
mod i18n;
mod infrastructure;
mod metadata;
mod native;
mod shell;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::http::{Response, StatusCode};
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder};

use application::search::SortKey;
use application::SearchOptions;
use backend::SearchBackend;
use infrastructure::icons;
use infrastructure::service::{host, scm};

const SVC_DIR: &str = r"C:\Program Files\Mosaic";

struct App {
    backend: RwLock<SearchBackend>,
}

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
    let re = regex::Regex::new(r#"[^\s"]*"[^"]*"|\S+"#).unwrap();
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
#[derive(Deserialize, Clone)]
struct SearchArgs {
    q: String,
    cat: String,
    sort: String,
    dir: String,
    meta: String,
    offset: usize,
    limit: usize,
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
}

#[derive(Serialize)]
struct Row {
    path: String,
    size: i64,
    mtime: i64,
    dir: bool,
    icon: String,
    #[serde(rename = "type")]
    ty: String,
    thumb: String,
}

#[derive(Serialize)]
struct Page {
    rows: Vec<Row>,
    matches: usize,
    total: usize,
    capped: bool,
    scanning: bool,
    scanned: usize,
    error: Option<String>,
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
        ..SearchOptions::default()
    }
}

// ---- Content: the in-process Tantivy index (contentindex.rs); Metadata: metadata.rs ----------------------------
/// ponytail: hits kept per query; raise it if someone needs more
const CONTENT_MAX: usize = 5000;

/// Files matching `query` in the content index that still exist, as index hits.
fn content_query(query: &str, h: &backend::SearchHandle) -> Result<Vec<application::index::Hit>, String> {
    let mut hits = Vec::new();
    for p in h.content(query, CONTENT_MAX)? {
        let Ok(m) = std::fs::metadata(&p) else { continue }; // gone since it was indexed
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        hits.push(application::index::Hit {
            name: p.rsplit('\\').next().unwrap_or(&p).to_string(),
            size: if m.is_dir() { -1 } else { m.len() as i64 },
            modified,
            created: 0,
            accessed: 0,
            attributes: 0,
            is_dir: m.is_dir(),
            path: p,
        });
    }
    Ok(hits)
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
fn find(a: &SearchArgs, offset: usize, limit: usize, h: &backend::SearchHandle) -> application::SearchResult {
    type Hits = std::sync::Arc<Vec<application::index::Hit>>;
    static LAST: Mutex<Option<(String, Hits, Option<String>)>> = Mutex::new(None);
    let (clean_q, hashq) = hashcache::extract_hash(&a.q);
    let (clean_q, fieldq) = metadata::extract_field(&clean_q);
    if !a.tags && !a.content && hashq.is_none() && fieldq.is_none() {
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
    application::SearchResult { total: hits.len(), took_ms: 0, hits: hits.iter().skip(offset).take(limit).cloned().collect(), error, capped: false }
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

#[tauri::command]
async fn search(app: State<'_, App>, args: SearchArgs) -> Result<Page, String> {
    let opts = options(&args, args.offset, args.limit.min(500));
    let (res, status) = {
        let b = app.backend.read();
        (b.search(&opts), b.status())
    };
    let rows = res
        .hits
        .into_iter()
        .map(|h| {
            let ext = if h.is_dir { None } else { ext_of(&h.name) };
            let icon = if h.is_dir {
                "http://res.localhost/icon?d=1".to_string()
            } else if matches!(ext.as_deref(), Some("exe" | "lnk" | "ico" | "msi" | "scr")) {
                format!("http://res.localhost/icon?p={}", enc(&h.path))
            } else {
                format!("http://res.localhost/icon?e={}", enc(ext.as_deref().unwrap_or("")))
            };
            Row {
                thumb: format!("http://res.localhost/thumb?p={}&m={}", enc(&h.path), h.modified),
                icon,
                ty: type_cache(&ext, h.is_dir),
                path: h.path,
                size: if h.is_dir { 0 } else { h.size.max(0) },
                mtime: h.modified / 1000,
                dir: h.is_dir,
            }
        })
        .collect();
    Ok(Page {
        rows,
        matches: res.total,
        total: status.count,
        capped: false,
        scanning: status.state == "indexing",
        scanned: status.count,
        error: res.error.or(if status.state == "error" { Some(status.message) } else { None }),
    })
}

/// Paths of a selection sent as [first, last] index ranges into the current result list.
#[tauri::command]
async fn paths(app: State<'_, App>, args: SearchArgs, ranges: Vec<(usize, usize)>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for (a, b) in ranges {
        let n = (b + 1).saturating_sub(a).min(20000usize.saturating_sub(out.len()));
        if n == 0 {
            break;
        }
        out.extend(app.backend.read().search(&options(&args, a, n)).hits.into_iter().map(|h| h.path));
    }
    Ok(out)
}

#[derive(Serialize)]
struct Status {
    state: String,
    count: usize,
    message: String,
    service: bool,
}

#[tauri::command]
fn status(app: State<'_, App>) -> Status {
    let b = app.backend.read();
    let s = b.status();
    Status { state: s.state, count: s.count, message: s.message, service: b.uses_service() }
}

/// One UAC prompt: install the index service (as a SYSTEM service under Program Files) and start it, then reconnect.
#[tauri::command]
async fn install_service(app: State<'_, App>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| infrastructure::elevation::run_elevated("--svc-setup")).await.map_err(|e| e.to_string())??;
    std::thread::sleep(Duration::from_millis(1500));
    *app.backend.write() = SearchBackend::detect();
    Ok(())
}

// ---- file actions -------------------------------------------------------------------------------------------
#[tauri::command]
fn open_paths(paths: Vec<String>) {
    for p in paths.iter().take(20) {
        shell::open(p);
    }
}
#[tauri::command]
fn reveal(path: String) {
    shell::reveal(&path);
}
#[tauri::command]
fn props(path: String) {
    shell::properties(path);
}
#[tauri::command]
fn clip(paths: Vec<String>, cut: bool) -> Result<usize, String> {
    shell::clipboard_files(&paths, cut).map(|_| paths.len())
}
#[tauri::command]
async fn delete(paths: Vec<String>) -> Result<usize, String> {
    let p2 = paths.clone();
    tauri::async_runtime::spawn_blocking(move || infrastructure::fileops::recycle_many(&p2)).await.map_err(|e| e.to_string())??;
    Ok(paths.iter().filter(|p| !std::path::Path::new(p).exists()).count())
}

#[tauri::command]
fn start_drag(window: tauri::WebviewWindow, paths: Vec<String>) -> Result<(), String> {
    let icon = drag::Image::Raw(include_bytes!("../icons/drag.png").to_vec());
    drag::start_drag(&window, drag::DragItem::Files(paths.into_iter().map(PathBuf::from).collect()), icon, |_, _| {}, Default::default())
        .map_err(|e| e.to_string())
}

// ---- res:// icons and thumbnails ----------------------------------------------------------------------------
/// (entries, insertion order, total bytes). The WebView keeps its own image cache, so this only has to cover re-scrolls.
type Lru = (HashMap<String, Vec<u8>>, VecDeque<String>, usize);
const CACHE_BYTES: usize = 16 << 20;

fn cached<F: FnOnce() -> Option<Vec<u8>>>(key: String, f: F) -> Option<Vec<u8>> {
    static C: OnceLock<Mutex<Lru>> = OnceLock::new();
    let c = C.get_or_init(Default::default);
    if let Some(v) = c.lock().0.get(&key) {
        return Some(v.clone());
    }
    let v = f()?;
    let mut g = c.lock();
    if !g.0.contains_key(&key) {
        g.2 += v.len();
        while g.2 > CACHE_BYTES {
            let Some(old) = g.1.pop_front() else { break };
            if let Some(b) = g.0.remove(&old) {
                g.2 -= b.len();
            }
        }
        g.1.push_back(key.clone());
        g.0.insert(key, v.clone());
    }
    Some(v)
}

fn res(uri: &str) -> Response<Vec<u8>> {
    let after = uri.split_once("://").map(|x| x.1).unwrap_or(uri);
    let (path, query) = after.split_once('/').map(|x| x.1).unwrap_or("").split_once('?').unwrap_or(("", ""));
    let q: HashMap<String, String> = query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), dec(v))).collect();
    let g = |k: &str| q.get(k).cloned().unwrap_or_default();
    let size: i32 = g("s").parse().unwrap_or(128).clamp(32, 256);
    let (body, mime): (Option<Vec<u8>>, &str) = match path {
        "thumb" => {
            let p = g("p");
            if p.to_lowercase().ends_with(".svg") && std::fs::metadata(&p).map(|m| m.len() < 2 << 20).unwrap_or(false) {
                // the shell's SVG thumbnailer starts helper processes (busy cursor); the browser draws SVG itself
                return respond(std::fs::read(&p).ok(), "image/svg+xml");
            }
            (cached(format!("t{size}|{p}|{}", g("m")), || shell::thumbnail(&p, size, false)), "image/png")
        }
        "icon" if !g("p").is_empty() => {
            let p = g("p");
            (cached(format!("f|{p}"), || shell::thumbnail(&p, 32, true)), "image/png")
        }
        "icon" if !g("s").is_empty() => {
            let (e, d) = (g("e"), !g("d").is_empty());
            (cached(format!("j|{e}|{d}"), || icons::jumbo_png((!e.is_empty()).then_some(e.as_str()), d)), "image/png")
        }
        "icon" => {
            let (e, d) = (g("e"), !g("d").is_empty());
            let body = cached(format!("e|{e}|{d}"), || {
                use base64::Engine;
                icons::icon_base64((!e.is_empty()).then_some(e.as_str()), d).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            });
            (body, "image/png")
        }
        _ => (None, ""),
    };
    respond(body, mime)
}

fn respond(body: Option<Vec<u8>>, mime: &str) -> Response<Vec<u8>> {
    match body {
        Some(b) => Response::builder()
            .header("Content-Type", mime)
            .header("Access-Control-Allow-Origin", "*")
            .header("Cache-Control", "max-age=3600")
            .header("Content-Security-Policy", "sandbox")
            .body(b)
            .unwrap(),
        None => Response::builder().status(StatusCode::NOT_FOUND).header("Access-Control-Allow-Origin", "*").body(Vec::new()).unwrap(),
    }
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
            let copy = || native::stamp(&me, &dst, "Mosaic Filename Index", native::index_icons());
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

/// While minimized, ask WebView2 to drop caches and page out what it can; restored, back to normal.
fn low_memory(w: &tauri::WebviewWindow, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use windows_core::Interface;
    let _ = w.with_webview(move |v| unsafe {
        if let Ok(c) = v.controller().CoreWebView2().and_then(|c| c.cast::<ICoreWebView2_19>()) {
            let _ = c.SetMemoryUsageTargetLevel(if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL });
        }
    });
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
    if std::env::args().any(|a| a == "--shell-helper") {
        shell::helper_main();
        return;
    }
    std::thread::spawn(|| {
        ext_info();
    });
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--service") {
        let _ = host::run();
        return;
    }
    if let Some(a) = args.get(1).filter(|a| a.starts_with("--svc-")) {
        std::process::exit(admin(a));
    }
    let named_native = std::env::current_exe().is_ok_and(|p| p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("mosaic")));
    if named_native || args.iter().any(|a| a == "--native") {
        native::app_id("local.mosaic.native");
        native::run();
        return;
    }
    // the native window is the app: start it as mosaic.exe (refreshed from this build) so Task Manager
    // names it right. The web UI is kept behind --web.
    if !args.iter().any(|a| a == "--web") {
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
    native::app_id("local.mosaic.web");
    let app = tauri::Builder::default()
        .manage(App { backend: RwLock::new(SearchBackend::detect()) })
        .register_asynchronous_uri_scheme_protocol("res", |_ctx, req, responder| {
            let uri = req.uri().to_string();
            shell::shell_spawn(Box::new(move |dropped| responder.respond(if dropped { respond(None, "") } else { res(&uri) })));
        })
        .invoke_handler(tauri::generate_handler![search, paths, status, install_service, open_paths, reveal, props, clip, delete, start_drag])
        .setup(|app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Mosaic Web")
                .inner_size(1400.0, 900.0)
                .background_color(tauri::window::Color(15, 18, 22, 255))
                // wry's defaults, plus caps on the GPU process's tile and decoded-image budgets: by default it grows to
                // hundreds of MB while scrolling. A full window of tiles needs well under 64 MB.
                .additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                     --force-gpu-mem-available-mb=64 --force-gpu-mem-discardable-limit-mb=32",
                )
                .initialization_script("window.native={startDrag:p=>window.__TAURI__.core.invoke('start_drag',{paths:p})}")
                .build()?;
            let w = app.get_webview_window("main").unwrap();
            if let Ok(h) = w.hwnd() {
                native::set_icons(h.0 as _, false);
            }
            let w2 = w.clone();
            w.on_window_event(move |e| {
                if let tauri::WindowEvent::Resized(_) = e {
                    low_memory(&w2, w2.is_minimized().unwrap_or(false));
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build");
    app.run(|_, _| {});
}
