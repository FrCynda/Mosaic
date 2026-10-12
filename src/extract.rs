//! File → plain text. Shared by the preview pane (`native_preview.rs`, which additionally keeps its
//! own UTF-16 layout of the result for on-screen display) and the content indexer
//! (`contentindex.rs`, which only wants the text). Pure functions over bytes/paths — no UI, no
//! index state — so both callers, and this module's own tests, can use it without touching a window
//! or an index.

/// Shown as text even when Windows has a (white, plain) preview handler for them.
pub const TEXTS: &[&str] = &[
    "txt", "log", "md", "markdown", "csv", "tsv", "json", "jsonc", "xml", "yml", "yaml", "toml", "ini", "cfg", "conf", "config", "env", "properties",
    "rs", "py", "pyw", "js", "mjs", "cjs", "jsx", "ts", "tsx", "c", "h", "cpp", "hpp", "cc", "cxx", "cs", "java", "kt", "kts", "go", "rb", "php",
    "sh", "bash", "zsh", "ps1", "psm1", "psd1", "bat", "cmd", "css", "scss", "sass", "less", "html", "htm", "vue", "svelte", "sql", "lua", "swift",
    "dart", "r", "pl", "pm", "scala", "gradle", "cmake", "mk", "make", "dockerfile", "gitignore", "gitattributes", "editorconfig", "reg", "srt",
    "vtt", "ass", "diff", "patch", "tex", "nfo", "svg", "sln", "csproj", "vcxproj", "props", "targets", "lock", "gradle",
];
/// Office Open XML / OpenDocument: zip files of XML, read as text when Windows has no preview/filter
/// handler for them installed (i.e. no Office / LibreOffice) — better than nothing for the common case.
pub const OFFICE: &[&str] = &["docx", "docm", "dotx", "dotm", "xlsx", "xlsm", "pptx", "pptm", "odt", "ods", "odp"];

/// Plain text rather than binary: no NUL bytes in the start (UTF-16 has a byte-order mark instead).
pub fn looks_text(b: &[u8]) -> bool {
    b.starts_with(&[0xff, 0xfe]) || b.starts_with(&[0xfe, 0xff]) || !b[..b.len().min(8192)].contains(&0)
}

/// Bytes as text: a byte-order mark decides, else UTF-8, else Windows-1252 (Latin-1 as the fallback,
/// where each byte is directly its own Unicode code point).
pub fn decode_str(b: &[u8], cut: bool) -> String {
    if let Some(r) = b.strip_prefix(&[0xff, 0xfe]) {
        return String::from_utf16_lossy(&r.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>());
    }
    if let Some(r) = b.strip_prefix(&[0xfe, 0xff]) {
        return String::from_utf16_lossy(&r.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>());
    }
    let b = b.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(b);
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        // cut mid-character at the caller's own limit: keep the valid part
        Err(e) if cut && e.error_len().is_none() => std::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or("").to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// Tags dropped, paragraph/row breaks kept, common XML entities decoded. Not a real XML parser — just
/// enough to turn Office/OpenDocument markup into readable plain text.
pub fn strip_xml(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)</(w:p|a:p|text:p|text:h|row)[^>]*>").unwrap());
    let s = RE.replace_all(s, "\n");
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Plain text pulled out of an Office/OpenDocument zip (`ext` must be one of [`OFFICE`]).
pub fn office_text(path: &str, ext: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut z = zip::ZipArchive::new(f).ok()?;
    fn read(z: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
        let mut e = z.by_name(name).ok()?;
        let mut s = String::new();
        std::io::Read::read_to_string(&mut e, &mut s).ok()?;
        Some(s)
    }
    let xml = match ext {
        "docx" | "docm" | "dotx" | "dotm" => read(&mut z, "word/document.xml").unwrap_or_default(),
        "xlsx" | "xlsm" => read(&mut z, "xl/sharedStrings.xml").unwrap_or_default(),
        "odt" | "ods" | "odp" => read(&mut z, "content.xml").unwrap_or_default(),
        "pptx" | "pptm" => {
            let mut names: Vec<String> = (0..z.len()).filter_map(|i| Some(z.by_index(i).ok()?.name().to_string())).filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml")).collect();
            names.sort_by_key(|n| n.trim_start_matches("ppt/slides/slide").trim_end_matches(".xml").parse::<u32>().unwrap_or(0));
            names.iter().filter_map(|n| read(&mut z, n)).collect::<Vec<_>>().join("\n\n")
        }
        _ => String::new(),
    };
    let text = strip_xml(&xml);
    (!text.trim().is_empty()).then_some(text)
}

/// Cap on IFilter output: same reasoning as `TEXT_MAX`-style guards elsewhere — one bloated
/// document shouldn't blow up the index or stall the extraction worker.
const IFILTER_MAX_CHARS: usize = 2_000_000;

/// Text pulled from anything with a registered Windows Search IFilter (PDF, legacy binary
/// Office `.doc`/`.xls`/`.ppt`, etc.) — the same mechanism Windows' own indexer uses. `None`
/// if nothing's registered for this file or extraction fails; never fatal to the caller, same
/// "no preview for this type" fallback already used for `IPreviewHandler` in `native_preview.rs`.
pub fn ifilter_text(path: &str) -> Option<String> {
    use windows::core::{Interface, PCWSTR, PWSTR};
    use windows::Win32::Storage::IndexServer::{IFilter, LoadIFilterEx, CHUNK_TEXT, IFILTER_INIT_APPLY_INDEX_ATTRIBUTES, STAT_CHUNK};
    use windows::Win32::System::Com::{CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};

    unsafe {
        // idempotent per thread (returns S_FALSE if already initialized elsewhere on this thread) — needed
        // because LoadIFilterEx does a CoCreateInstance internally, and callers of this function (the
        // out-of-process `--extract` worker, most notably) may never have touched COM otherwise
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut punk: *mut std::ffi::c_void = std::ptr::null_mut();
        LoadIFilterEx(PCWSTR(wpath.as_ptr()), CLSCTX_INPROC_SERVER.0, &IFilter::IID, &mut punk).ok()?;
        if punk.is_null() {
            return None;
        }
        let filter = IFilter::from_raw(punk);

        let mut flags = 0u32;
        if filter.Init(IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0 as u32, &[], &mut flags) < 0 {
            return None;
        }

        let mut out = String::new();
        loop {
            let mut stat = STAT_CHUNK::default();
            if filter.GetChunk(&mut stat) < 0 {
                break; // includes FILTER_E_END_OF_CHUNKS: no more chunks
            }
            if stat.flags != CHUNK_TEXT {
                continue;
            }
            loop {
                let mut buf = [0u16; 4096];
                let mut n = buf.len() as u32;
                let hr = filter.GetText(&mut n, PWSTR(buf.as_mut_ptr()));
                if hr < 0 {
                    break; // FILTER_E_NO_MORE_TEXT (or any other failure): done with this chunk
                }
                out.push_str(&String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]));
                out.push(' ');
                if out.len() > IFILTER_MAX_CHARS {
                    return Some(out);
                }
                // hr == S_OK: more text in this chunk: keep pulling. FILTER_S_LAST_TEXT (>0): last piece, stop.
                if hr != 0 {
                    break;
                }
            }
        }
        (!out.trim().is_empty()).then_some(out)
    }
}

/// Cap on a plain-text file read for indexing — generous compared to the preview pane's
/// `TEXT_MAX` (this is for search relevance, not on-screen display).
const CONTENT_TEXT_MAX: usize = 5 * 1024 * 1024;

fn ext_of(path: &str) -> String {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Known text/code by extension or sniffing, then Office/OpenDocument XML — pure Rust + zip, no
/// third-party code runs, so (unlike `ifilter_text`) this is always safe to call in-process. This
/// alone covers most files worth indexing; `full_text` only reaches further for the rest.
pub fn local_text(path: &str) -> Option<String> {
    let ext = ext_of(path);
    if OFFICE.contains(&ext.as_str()) {
        if let Some(t) = office_text(path, &ext) {
            return Some(t);
        }
    } else if let Ok(mut f) = std::fs::File::open(path) {
        use std::io::Read;
        let known_text = TEXTS.contains(&ext.as_str());
        // sniff just enough to rule out a binary before committing to a full read: most files on a
        // disk (images, executables, media) are binary, and reading the full CONTENT_TEXT_MAX cap
        // out of every one of them before checking is the difference between a bulk scan finishing
        // in seconds and one that's still crawling the drive hours later
        let mut sniff = [0u8; 8192];
        let n = f.read(&mut sniff).unwrap_or(0);
        if known_text || looks_text(&sniff[..n]) {
            let mut b = sniff[..n].to_vec();
            if f.take(CONTENT_TEXT_MAX as u64 + 1 - n as u64).read_to_end(&mut b).is_ok() {
                let cut = b.len() > CONTENT_TEXT_MAX;
                b.truncate(CONTENT_TEXT_MAX);
                return Some(decode_str(&b, cut));
            }
        }
    }
    None
}

/// Binary formats worth paying for an out-of-process IFilter extraction. Not derived from the
/// registry: Windows registers *some* persistent handler, under the search-filter category, for
/// almost every extension on disk (verified: plain `.txt` and `.dll` both resolve to the same
/// generic catch-all CLSID) — so a registry probe can't tell "real content filter" from "nothing
/// useful." Recoll's own indexer sidesteps the same problem the same way: a fixed list of formats
/// it knows are worth an external filter, not a query of what's installed.
pub const IFILTER_EXTS: &[&str] = &["pdf", "doc", "xls", "ppt", "rtf", "msg", "one", "pub", "vsd", "wps", "wri"];

/// Is this extension one we bother spawning `ifilter_text`'s subprocess for at all?
pub fn has_ifilter(path: &str) -> bool {
    IFILTER_EXTS.contains(&ext_of(path).as_str())
}

/// The full fallback chain used by the `--extract` worker: `local_text`, then whatever Windows has
/// an IFilter registered for (PDF, legacy binary Office, etc.). `None` if nothing yields text —
/// not an error, just nothing to index for this file.
pub fn full_text(path: &str) -> Option<String> {
    local_text(path).or_else(|| has_ifilter(path).then(|| ifilter_text(path)).flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_told_from_binary_and_decoded() {
        assert!(looks_text(b"fn main() {}\n"));
        assert!(looks_text(&[0xff, 0xfe, b'h', 0, b'i', 0]));
        assert!(!looks_text(b"MZ\x90\0\x03\0"));
        assert_eq!(decode_str(&[0xff, 0xfe, b'h', 0, b'i', 0], false), "hi");
        assert_eq!(decode_str("\u{feff}é".as_bytes(), false), "é");
        assert_eq!(decode_str(&[b'a', 0xe9], false), "aé"); // not UTF-8: Latin-1
        assert_eq!(decode_str(&"ab€".as_bytes()[..4], true), "ab"); // cut inside the €
    }

    #[test]
    fn strip_xml_keeps_text_drops_tags_and_decodes_entities() {
        let xml = r#"<w:p><w:r><w:t>Hello &amp; welcome</w:t></w:r></w:p><w:p><w:r><w:t>Line two</w:t></w:r></w:p>"#;
        assert_eq!(strip_xml(xml), "Hello & welcome\nLine two\n");
    }

    #[test]
    fn office_text_reads_a_real_docx_zip() {
        let dir = std::env::temp_dir().join(format!("extract_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.docx");
        let f = std::fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file::<_, ()>("word/document.xml", Default::default()).unwrap();
        std::io::Write::write_all(&mut z, br#"<w:document><w:body><w:p><w:r><w:t>hello docx</w:t></w:r></w:p></w:body></w:document>"#).unwrap();
        z.finish().unwrap();

        let text = office_text(path.to_str().unwrap(), "docx").unwrap();
        assert!(text.contains("hello docx"), "got: {text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
