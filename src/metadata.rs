//! The Metadata toggle's index: the text ExifTool reads from inside files (EXIF/XMP, PNG text chunks like Stable
//! Diffusion's parameters, ID3, document properties), without the file system's own fields (name, folder, dates).
//! Kept per path and mtime in %LOCALAPPDATA%\Mosaic\metadata.bin; a sync only reads new or changed files.

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;

use crate::application::index::Hit;
use crate::application::SearchOptions;
use crate::backend::SearchHandle;

/// File types whose metadata is worth reading.
const EXTS: &str = "jpg;jpeg;png;webp;gif;tif;tiff;heic;avif;psd;mp3;flac;m4a;ogg;opus;wma;wav;mp4;mkv;mov;avi;wmv;webm;m4v;pdf;doc;docx;xls;xlsx;ppt;pptx;odt;ods;odp;epub;rtf";
/// Where not to look: system files and app caches carry no metadata anyone searches for.
const SKIP: &str = r"!path:\appdata\ !path:c:\windows\ !path:$recycle.bin";
const BATCH: usize = 256;
/// ExifTool processes at once: half the cores, so the machine stays usable during the first run
fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(2))
}
const TEXT_MAX: usize = 16 << 10;

/// A handful of the most-searched EXIF/ID3/document fields, pulled out of the flattened blob so they can be
/// queried by name (`camera-model:`, `iso-speed:`, …) instead of a substring match over everything.
// ponytail: five fields, not the plan's full ~15 — add a row to `FIELDS`/`STRUCT_OF` when another is wanted.
#[derive(Default, serde::Serialize, serde::Deserialize, Clone)]
struct Fields {
    camera: String,   // Model
    artist: String,   // Artist / Creator
    iso: Option<i64>,     // ISO
    width: Option<i64>,   // ImageWidth
    height: Option<i64>,  // ImageHeight
}

/// `query prefix` -> ExifTool tag name, for both extraction (`read`) and query parsing (`extract_field`).
const FIELDS: [(&str, &str); 5] = [("camera-model", "Model"), ("artist", "Artist"), ("iso-speed", "ISO"), ("width", "ImageWidth"), ("height", "ImageHeight")];

/// path → (mtime ms, size, lowercased metadata text, structured fields)
type Files = HashMap<String, (i64, i64, String, Fields)>;

static FILES: RwLock<Option<Files>> = RwLock::new(None);
static GEN: AtomicUsize = AtomicUsize::new(0);
/// (files read, files to read) while a sync runs
static PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static SYNCED: Mutex<Option<Instant>> = Mutex::new(None);
static ERROR: Mutex<Option<String>> = Mutex::new(None);

fn store() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("metadata.bin")
}

fn exiftool() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("exiftool.exe")).find(|f| f.exists()));
    on_path.or_else(|| Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"Programs\ExifTool\ExifTool.exe")).filter(|f| f.exists()))
}

/// Bumps whenever the index changes, so cached results know to go stale.
pub fn generation() -> usize {
    GEN.load(Ordering::Relaxed)
}
pub fn progress() -> Option<(usize, usize)> {
    *PROGRESS.lock()
}
pub fn error() -> Option<String> {
    ERROR.lock().clone()
}

/// Bring the index up to date in the background: at most once a minute, never twice at a time.
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
    let exe = exiftool().ok_or("Metadata needs ExifTool (exiftool.exe on PATH)")?;
    if FILES.read().is_none() {
        let saved: Files = std::fs::read(store()).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default();
        *FILES.write() = Some(saved);
        GEN.fetch_add(1, Ordering::Relaxed);
    }
    // every candidate from the name index, a page at a time
    let mut now: HashMap<String, (i64, i64)> = HashMap::new();
    for offset in (0..).step_by(100_000) {
        let opts = SearchOptions { query: format!("ext:{EXTS} {SKIP}"), limit: 100_000, offset, ..SearchOptions::default() };
        let r = h.search(&opts);
        if let Some(e) = r.error {
            return Err(e);
        }
        let n = r.hits.len();
        now.extend(r.hits.into_iter().filter(|x| !x.is_dir).map(|x| (x.path, (x.modified, x.size))));
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
        for k in 0..workers() {
            let (queue, exe, now, saved) = (&queue, &exe, &now, &saved);
            s.spawn(move || {
                loop {
                    // the lock must drop before ExifTool runs: in `while let` it would be held for the whole batch
                    let next = queue.lock().pop();
                    let Some(chunk) = next else { break };
                    let texts = read(exe, chunk, k);
                    let mut g = FILES.write();
                    let files = g.as_mut().unwrap();
                    for p in chunk {
                        let (m, size) = now[p];
                        let (text, fields) = texts.get(&p.to_lowercase()).cloned().unwrap_or_default();
                        files.insert(p.clone(), (m, size, text, fields));
                    }
                    drop(g);
                    if let Some(pr) = PROGRESS.lock().as_mut() {
                        pr.0 += chunk.len();
                    }
                    GEN.fetch_add(1, Ordering::Relaxed);
                    // keep what's read so far: closing the app mid-run doesn't start the next one over
                    let mut s = saved.lock();
                    if s.elapsed() > Duration::from_secs(30) {
                        *s = Instant::now();
                        let _ = save();
                    }
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

/// Pull the structured fields out of one ExifTool JSON object (a text string or, for the numeric
/// fields, a JSON number or a numeric string — ExifTool emits either depending on `-j` quirks).
fn fields_of(o: &serde_json::Map<String, serde_json::Value>) -> Fields {
    let text = |tag: &str| o.get(tag).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())));
    let num = |tag: &str| o.get(tag).and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
    Fields { camera: text("Model").unwrap_or_default(), artist: text("Artist").unwrap_or_default(), iso: num("ISO"), width: num("ImageWidth"), height: num("ImageHeight") }
}

/// One ExifTool run over `paths`: lowercased path → (the metadata's text values lowercased, the structured fields).
fn read(exe: &PathBuf, paths: &[String], k: usize) -> HashMap<String, (String, Fields)> {
    let args = std::env::temp_dir().join(format!("mosaic-meta-{}-{k}.txt", std::process::id()));
    if std::fs::write(&args, paths.join("\n")).is_err() {
        return HashMap::new();
    }
    let out = Command::new(exe)
        .args(["-charset", "filename=utf8", "-q", "-q", "-fast", "-j", "-a", "--File:all", "--System:all", "--ExifTool:all", "--Composite:all", "-@"])
        .arg(&args)
        .creation_flags(0x0800_0000) // no console window
        .output();
    let _ = std::fs::remove_file(&args);
    let Ok(out) = out else { return HashMap::new() };
    let objs: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    objs.into_iter()
        .filter_map(|o| {
            let path = o.get("SourceFile")?.as_str()?.replace('/', "\\").to_lowercase();
            let mut text = String::new();
            for (k, v) in &o {
                // text values only: numbers are sizes and depths, binary blobs come as "(Binary data …)"
                if let Some(s) = v.as_str().filter(|s| k != "SourceFile" && !s.starts_with("(Binary data")) {
                    text.push_str(&s.to_lowercase());
                    text.push('\n');
                }
            }
            let mut n = TEXT_MAX.min(text.len());
            while !text.is_char_boundary(n) {
                n -= 1;
            }
            text.truncate(n);
            Some((path, (text, fields_of(&o))))
        })
        .collect()
}

/// One `field:value` operator from a search box query: `camera-model:canon`, `iso-speed:>800`, `width:1920..3840`.
/// A text field is a case-insensitive substring; a numeric field takes `size:`'s `>`/`>=`/`<`/`<=`/`A..B` grammar.
#[derive(Debug)]
pub enum FieldFilter {
    Text(String),
    Num { min: Option<i64>, max: Option<i64> },
}

fn parse_num_filter(v: &str) -> Option<FieldFilter> {
    let (op, rest) = [">=", "<=", ">", "<"].iter().find_map(|op| v.strip_prefix(op).map(|r| (*op, r))).unwrap_or(("", v));
    if let Some((lo, hi)) = rest.split_once("..") {
        return Some(FieldFilter::Num { min: lo.parse().ok(), max: hi.parse().ok() });
    }
    let n: i64 = rest.parse().ok()?;
    Some(match op {
        ">" => FieldFilter::Num { min: Some(n + 1), max: None },
        ">=" => FieldFilter::Num { min: Some(n), max: None },
        "<" => FieldFilter::Num { min: None, max: Some(n - 1) },
        "<=" => FieldFilter::Num { min: None, max: Some(n) },
        _ => FieldFilter::Num { min: Some(n), max: Some(n) },
    })
}

/// Pull the first recognised `field:value` operator out of a query, returning the query with it
/// removed and the parsed filter. Unrecognised field names are left in the query untouched (they
/// fall through to the normal name/path search, same as any other word).
pub fn extract_field(query: &str) -> (String, Option<(&'static str, FieldFilter)>) {
    for (prefix, tag) in FIELDS {
        let needle = format!("{prefix}:");
        if let Some(start) = query.to_lowercase().find(&needle) {
            let rest = &query[start + needle.len()..];
            let end = rest.find(' ').unwrap_or(rest.len());
            let (value, tail) = (&rest[..end], &rest[end..]);
            let filter = if matches!(prefix, "iso-speed" | "width" | "height") {
                parse_num_filter(value)
            } else {
                Some(FieldFilter::Text(value.to_lowercase()))
            };
            let cleaned = format!("{}{}", query[..start].trim_end(), tail).trim().to_string();
            if let Some(f) = filter {
                return (cleaned, Some((tag, f)));
            }
            return (cleaned, None); // an unparseable value drops the operator, like `size:` elsewhere
        }
    }
    (query.to_string(), None)
}

/// Whether `path`'s structured fields satisfy a `field_matches` filter from [`extract_field`]. `false`
/// for a path metadata hasn't been read for yet (matches `field:` semantics: needs `tags:` synced first).
pub fn field_matches(path: &str, tag: &'static str, filter: &FieldFilter) -> bool {
    let g = FILES.read();
    let Some((.., fields)) = g.as_ref().and_then(|f| f.get(path)) else { return false };
    match tag {
        "Model" => matches!(filter, FieldFilter::Text(v) if fields.camera.to_lowercase().contains(v.as_str())),
        "Artist" => matches!(filter, FieldFilter::Text(v) if fields.artist.to_lowercase().contains(v.as_str())),
        "ISO" => num_matches(fields.iso, filter),
        "ImageWidth" => num_matches(fields.width, filter),
        "ImageHeight" => num_matches(fields.height, filter),
        _ => false,
    }
}

fn num_matches(value: Option<i64>, filter: &FieldFilter) -> bool {
    let (Some(v), FieldFilter::Num { min, max }) = (value, filter) else { return false };
    min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
}

/// Files whose metadata holds every term: space-separated terms, `"quoted phrases"`, `-term` excludes.
pub fn search(q: &str) -> Vec<Hit> {
    let (want, not) = terms(q);
    if want.is_empty() {
        return Vec::new();
    }
    let g = FILES.read();
    let Some(files) = g.as_ref() else { return Vec::new() };
    files
        .par_iter()
        .filter(|(_, (_, _, t, _))| want.iter().all(|w| t.contains(w.as_str())) && !not.iter().any(|w| t.contains(w.as_str())))
        .map(|(p, &(modified, size, _, _))| Hit {
            name: p.rsplit('\\').next().unwrap_or(p).to_string(),
            path: p.clone(),
            size,
            modified,
            created: 0,
            accessed: 0,
            attributes: 0,
            is_dir: false,
        })
        .collect()
}

fn terms(q: &str) -> (Vec<String>, Vec<String>) {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#"-?"[^"]*"|\S+"#).unwrap());
    let (mut want, mut not) = (Vec::new(), Vec::new());
    for m in RE.find_iter(q) {
        let t = m.as_str();
        let (neg, t) = t.strip_prefix('-').map_or((false, t), |r| (true, r));
        let t = t.trim_matches('"').to_lowercase();
        if !t.is_empty() {
            if neg { not.push(t) } else { want.push(t) }
        }
    }
    (want, not)
}

#[cfg(test)]
mod tests {
    #[test]
    fn candidates_are_media_outside_system_folders() {
        use crate::application::search::{EntryView, Matcher};
        let m = Matcher::compile(&super::SearchOptions { query: format!("ext:{} {}", super::EXTS, super::SKIP), ..Default::default() }).unwrap();
        let hit = |p: &str| {
            let (pl, name) = (p.to_lowercase(), p.rsplit('\\').next().unwrap());
            let nl = name.to_lowercase();
            m.eval(&EntryView { name, name_lower: &nl, path: p, path_lower: &pl, is_dir: false, size: None, modified_ms: None, created_ms: None, accessed_ms: None, attributes: 0 })
        };
        assert!(hit(r"C:\Users\frenc\Documents\CheatEngine\outputs\MainSD\00000-1321019266-standing.jpg"));
        assert!(!hit(r"C:\Users\frenc\AppData\Local\Temp\x.png"));
        assert!(!hit(r"C:\Users\frenc\notes.txt"));
    }

    #[test]
    fn terms_phrases_and_excludes() {
        let (want, not) = super::terms(r#"CFG "scale: 4.5" -euler"#);
        assert_eq!(want, ["cfg", "scale: 4.5"]);
        assert_eq!(not, ["euler"]);
    }

    #[test]
    fn fields_of_flattens_a_sample_exiftool_json_object() {
        // A trimmed real-shaped `-j` object: string tags, a numeric ISO, and a string-typed ImageWidth
        // (ExifTool emits numeric-looking tags as either JSON numbers or numeric strings depending on the tag).
        let sample = r#"{
            "SourceFile": "C:/Photos/beach.jpg",
            "Model": "Canon EOS R5",
            "Artist": "Jane Doe",
            "ISO": 400,
            "ImageWidth": "6000",
            "ImageHeight": 4000,
            "ThumbnailImage": "(Binary data 12345 bytes)"
        }"#;
        let o: serde_json::Map<String, serde_json::Value> = serde_json::from_str(sample).unwrap();
        let fields = super::fields_of(&o);
        assert_eq!(fields.camera, "Canon EOS R5");
        assert_eq!(fields.artist, "Jane Doe");
        assert_eq!(fields.iso, Some(400));
        assert_eq!(fields.width, Some(6000));
        assert_eq!(fields.height, Some(4000));
    }

    #[test]
    fn fields_of_defaults_when_tags_are_absent() {
        let o: serde_json::Map<String, serde_json::Value> = serde_json::from_str(r#"{"SourceFile": "x.jpg"}"#).unwrap();
        let fields = super::fields_of(&o);
        assert_eq!(fields.camera, "");
        assert_eq!(fields.artist, "");
        assert_eq!(fields.iso, None);
        assert_eq!(fields.width, None);
        assert_eq!(fields.height, None);
    }

    #[test]
    fn extract_field_pulls_the_operator_and_cleans_the_query() {
        let (rest, f) = super::extract_field("sunset camera-model:Canon beach");
        assert_eq!(rest, "sunset beach");
        assert!(matches!(f, Some(("Model", super::FieldFilter::Text(v))) if v == "canon"));

        let (rest, f) = super::extract_field("iso-speed:>800");
        assert_eq!(rest, "");
        assert!(matches!(f, Some(("ISO", super::FieldFilter::Num { min: Some(801), max: None }))));

        let (rest, f) = super::extract_field("just a plain search");
        assert_eq!(rest, "just a plain search");
        assert!(f.is_none());
    }

    #[test]
    fn num_filter_grammar_matches_sizes_style() {
        assert!(matches!(super::parse_num_filter(">800"), Some(super::FieldFilter::Num { min: Some(801), max: None })));
        assert!(matches!(super::parse_num_filter("<=100"), Some(super::FieldFilter::Num { min: None, max: Some(100) })));
        assert!(matches!(super::parse_num_filter("100..200"), Some(super::FieldFilter::Num { min: Some(100), max: Some(200) })));
        assert!(matches!(super::parse_num_filter("400"), Some(super::FieldFilter::Num { min: Some(400), max: Some(400) })));
        assert!(super::parse_num_filter("nope").is_none());
    }

    #[test]
    fn field_matches_reads_the_structured_fields_of_a_synced_file() {
        let path = r"C:\ponytail-test\field-matches-fixture.jpg".to_string();
        super::FILES.write().get_or_insert_with(Default::default).insert(
            path.clone(),
            (0, 0, String::new(), super::Fields { camera: "Canon EOS R5".into(), artist: String::new(), iso: Some(400), width: Some(1920), height: None }),
        );
        assert!(super::field_matches(&path, "Model", &super::FieldFilter::Text("canon".into())));
        assert!(!super::field_matches(&path, "Model", &super::FieldFilter::Text("nikon".into())));
        assert!(super::field_matches(&path, "ISO", &super::FieldFilter::Num { min: Some(100), max: Some(800) }));
        assert!(!super::field_matches(&path, "ISO", &super::FieldFilter::Num { min: Some(800), max: None }));
        // never synced: no false positive from an absent entry
        assert!(!super::field_matches("C:\\never\\synced.jpg", "Model", &super::FieldFilter::Text("canon".into())));
    }
}
