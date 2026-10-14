//! Folders the index doesn't hold, listed by asking the file system: network shares (`\\server\share`), Linux
//! (`\\wsl.localhost\…`), drives that aren't local NTFS (USB sticks, mapped drives), and the lists of them: This PC
//! (`pc:`), Network (`net:`) and Linux (`linux:`); and the recent files and folders (`recent:`, `recent-dirs:`),
//! wherever they are. Only the last listing is kept, for the list to page through.

use std::os::windows::fs::MetadataExt;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};

use crate::application::index::{Hit, SearchResult};
use crate::application::search::{EntryView, Matcher};
use crate::application::SearchOptions;
use crate::i18n::t;

/// The lists made here rather than read from one folder.
const LISTS: [&str; 5] = ["pc:", "net:", "linux:", "recent:", "recent-dirs:"];

/// A list of `q`'s own (This PC, Network, Linux, Recent), or the folder of a `child:` query the index can't answer.
fn target(q: &str) -> Option<String> {
    let q = q.trim();
    if LISTS.contains(&q) {
        return Some(q.into());
    }
    let (d, _) = q.strip_prefix("child:\"")?.split_once('"')?;
    outside(d).then(|| d.to_string())
}

/// Whether folder `d` is outside the index: on the network, in Linux, or on a drive that isn't local NTFS.
pub fn outside(d: &str) -> bool {
    match d.as_bytes() {
        [b'\\', b'\\', ..] => true,
        [l, b':', ..] if l.is_ascii_alphabetic() => !indexed(l.to_ascii_uppercase()),
        _ => false,
    }
}

/// A server (`\\name`), whose shares are its folders though the file system has no folder there.
pub fn server(p: &str) -> bool {
    p.strip_prefix("\\\\").is_some_and(|s| !s.is_empty() && !s.trim_end_matches('\\').contains('\\'))
}

/// Whether drive `l` is a local NTFS one, as the index service reads. Asked again after ten seconds (a stick swapped).
fn indexed(l: u8) -> bool {
    static SEEN: Mutex<[(Option<Instant>, bool); 26]> = Mutex::new([(None, false); 26]);
    let k = (l - b'A') as usize;
    if let (Some(at), v) = SEEN.lock()[k] {
        if at.elapsed() < Duration::from_secs(10) {
            return v;
        }
    }
    let root: Vec<u16> = format!("{}:\\", l as char).encode_utf16().chain([0]).collect();
    let mut fs = [0u16; 32];
    let v = unsafe {
        matches!(GetDriveTypeW(root.as_ptr()), 2 | 3) // removable, fixed: not network (4), CD (5) or RAM disk
            && GetVolumeInformationW(root.as_ptr(), null_mut(), 0, null_mut(), null_mut(), null_mut(), fs.as_mut_ptr(), 32) != 0
            && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS"
    };
    SEEN.lock()[k] = (Some(Instant::now()), v);
    v
}

/// `q`'s result when it lists something the index can't, else `None`. A first page reads the folder afresh; later
/// pages come from that reading, so scrolling costs nothing.
pub fn find(q: &str, o: &SearchOptions) -> Option<SearchResult> {
    type Last = (String, Arc<Vec<Hit>>, Option<String>);
    static LAST: Mutex<Option<Last>> = Mutex::new(None);
    let dir = target(q)?;
    let key = format!("{:?}", SearchOptions { offset: 0, limit: 0, ..o.clone() });
    let kept = LAST.lock().as_ref().filter(|l| o.offset > 0 && l.0 == key).map(|l| (l.1.clone(), l.2.clone()));
    let (hits, error) = kept.unwrap_or_else(|| {
        let (hits, error) = list(&dir, o);
        let hits = Arc::new(hits);
        *LAST.lock() = Some((key, hits.clone(), error.clone()));
        (hits, error)
    });
    Some(SearchResult { total: hits.len(), took_ms: 0, hits: hits.iter().skip(o.offset).take(o.limit).cloned().collect(), error, capped: false, stamp: None })
}

fn list(dir: &str, o: &SearchOptions) -> (Vec<Hit>, Option<String>) {
    let d = dir.to_string();
    // on its own thread: a share that stopped answering holds that thread, not every tab's searches
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read(&d));
    });
    let mut hits = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return (Vec::new(), Some(e)),
        Err(_) => return (Vec::new(), Some(t("direct.no_answer").into())),
    };
    if !LISTS.contains(&dir) {
        // a folder's own: the query's other words, the category, hidden files as asked
        let m = match Matcher::compile(o) {
            Ok(m) => m,
            Err(e) => return (Vec::new(), Some(e)),
        };
        hits.retain(|h| {
            let a = h.attributes;
            let shown = if a & 0x6 == 0x6 { o.show_system } else { o.show_hidden || a & 0x2 == 0 };
            let (nl, pl) = (h.name.to_lowercase(), h.path.to_lowercase());
            shown
                && m.eval(&EntryView {
                    name: &h.name,
                    name_lower: &nl,
                    path: &h.path,
                    path_lower: &pl,
                    is_dir: h.is_dir,
                    size: (h.size >= 0).then_some(h.size as u64),
                    modified_ms: Some(h.modified),
                    created_ms: Some(h.created),
                    accessed_ms: Some(h.accessed),
                    attributes: a,
                })
        });
    }
    crate::application::catalog::sort_hits(&mut hits, o);
    (hits, None)
}

fn place(name: String, path: String) -> Hit {
    Hit { name, path, size: -1, modified: 0, created: 0, accessed: 0, attributes: 0x10, is_dir: true }
}

fn read(dir: &str) -> Result<Vec<Hit>, String> {
    let low = dir.trim_end_matches('\\').to_ascii_lowercase();
    match low.as_str() {
        "pc:" => return Ok(drives(|_| true)),
        "recent:" | "recent-dirs:" => return Ok(crate::recent::list(low == "recent-dirs:", 200).iter().filter_map(|p| entry(p, std::fs::metadata(p).ok()?)).collect()),
        "net:" => {
            // mapped network drives, then the servers this PC connects to. Not a scan of the network: asking the
            // neighbourhood (SMB1 browsing) took 20 s here to find nothing. A typed `\\server` lists its shares.
            let mut v = drives(|ty| ty == 4);
            let mut servers: Vec<String> = wnet(None).iter().filter_map(|s| s[2..].split('\\').next()).map(|n| format!(r"\\{n}")).collect();
            servers.sort_by_key(|s| s.to_lowercase());
            servers.dedup_by_key(|s| s.to_lowercase());
            v.extend(servers.into_iter().map(|s| place(s[2..].to_string(), s)));
            return Ok(v);
        }
        "linux:" | "\\\\wsl.localhost" | "\\\\wsl$" => return Ok(distros().into_iter().map(|d| place(d.clone(), format!("\\\\wsl.localhost\\{d}"))).collect()),
        "" | "\\" => return read("net:"),
        _ if server(dir) => {
            let v = wnet(Some(dir.trim_end_matches('\\')));
            return Ok(v.into_iter().map(|s| place(s.rsplit('\\').next().unwrap_or_default().to_string(), s)).collect());
        }
        _ => {}
    }
    let base = dir.trim_end_matches('\\');
    let rd = std::fs::read_dir(format!("{base}\\")).map_err(|e| e.to_string())?;
    // each entry's metadata comes from the directory listing itself: no extra trip per file
    Ok(rd.flatten().filter_map(|e| entry(&format!("{base}\\{}", e.file_name().to_string_lossy()), e.metadata().ok()?)).collect())
}

/// The file or folder at `path`, as a hit.
fn entry(path: &str, m: std::fs::Metadata) -> Option<Hit> {
    let ms = |ft: u64| if ft == 0 { 0 } else { (ft / 10_000) as i64 - 11_644_473_600_000 };
    let is_dir = m.file_attributes() & 0x10 != 0;
    Some(Hit {
        name: path.rsplit('\\').next()?.to_string(),
        path: path.to_string(),
        size: if is_dir { -1 } else { m.file_size() as i64 },
        modified: ms(m.last_write_time()),
        created: ms(m.creation_time()),
        accessed: ms(m.last_access_time()),
        attributes: m.file_attributes(),
        is_dir,
    })
}

/// The drives whose type (`GetDriveTypeW`) `keep` takes, as "Label (C:)".
fn drives(keep: impl Fn(u32) -> bool) -> Vec<Hit> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i) as char);
            let w: Vec<u16> = root.encode_utf16().chain([0]).collect();
            let ty = unsafe { GetDriveTypeW(w.as_ptr()) };
            if !keep(ty) {
                return None;
            }
            let mut name = [0u16; 261];
            let ok = ty != 5 && unsafe { GetVolumeInformationW(w.as_ptr(), name.as_mut_ptr(), 261, null_mut(), null_mut(), null_mut(), null_mut(), 0) } != 0; // an empty tray: don't spin it up
            let n = name.iter().position(|&c| c == 0).unwrap_or(0);
            let vol = if ok && n > 0 { String::from_utf16_lossy(&name[..n]) } else { t("side.local_disk").to_string() };
            Some(place(format!("{vol} ({}:)", &root[..1]), root))
        })
        .collect()
}

/// The Linux distributions installed, by name (what `\\wsl.localhost\` lists).
fn distros() -> Vec<String> {
    use windows_sys::Win32::System::Registry::*;
    let mut out = Vec::new();
    unsafe {
        let mut k = null_mut();
        let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss\0".encode_utf16().collect();
        if RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut k) != 0 {
            return out;
        }
        let value: Vec<u16> = "DistributionName\0".encode_utf16().collect();
        for i in 0.. {
            let (mut sub, mut n) = ([0u16; 128], 128u32);
            if RegEnumKeyExW(k, i, sub.as_mut_ptr(), &mut n, null(), null_mut(), null_mut(), null_mut()) != 0 {
                break;
            }
            let (mut buf, mut len) = ([0u16; 256], 512u32);
            if RegGetValueW(k, sub.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut len) == 0 {
                let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
                if !s.is_empty() {
                    out.push(s);
                }
            }
        }
        RegCloseKey(k);
    }
    out
}

/// The shares this PC is connected to or remembers (`None`), or a server's shared folders, as UNC paths.
fn wnet(server: Option<&str>) -> Vec<String> {
    use windows_sys::Win32::NetworkManagement::WNet::*;
    let mut out = Vec::new();
    let mut name: Vec<u16> = server.unwrap_or_default().encode_utf16().chain([0]).collect();
    let scopes: &[u32] = if server.is_some() { &[RESOURCE_GLOBALNET] } else { &[RESOURCE_CONNECTED, RESOURCE_REMEMBERED] };
    for &scope in scopes {
        unsafe { wnet_enum(scope, server.map(|_| name.as_mut_ptr()), &mut out) };
    }
    out
}

/// One `WNetOpenEnumW` walk (`scope`, under `remote` if given), its UNC names into `out`.
unsafe fn wnet_enum(scope: u32, remote: Option<*mut u16>, out: &mut Vec<String>) {
    use windows_sys::Win32::NetworkManagement::WNet::*;
    let mut h = null_mut();
    let mut nr: NETRESOURCEW = std::mem::zeroed();
    nr.dwUsage = RESOURCEUSAGE_CONTAINER;
    let r = match remote {
        None => WNetOpenEnumW(scope, RESOURCETYPE_DISK, 0, null(), &mut h),
        Some(p) => {
            nr.lpRemoteName = p;
            WNetOpenEnumW(scope, RESOURCETYPE_DISK, 0, &nr, &mut h)
        }
    };
    if r != 0 {
        return;
    }
    let mut buf = vec![0u64; 4096]; // NETRESOURCEWs and their strings, aligned
    loop {
        let (mut count, mut size) = (u32::MAX, (buf.len() * 8) as u32);
        if WNetEnumResourceW(h, &mut count, buf.as_mut_ptr().cast(), &mut size) != 0 {
            break;
        }
        for r in std::slice::from_raw_parts(buf.as_ptr() as *const NETRESOURCEW, count as usize) {
            if r.lpRemoteName.is_null() {
                continue;
            }
            let n = (0..).take_while(|&i| *r.lpRemoteName.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(r.lpRemoteName, n));
            if s.starts_with("\\\\") {
                out.push(s);
            }
        }
    }
    WNetCloseEnum(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_folders_are_listed_directly() {
        assert!(outside(r"\\server\share") && outside(r"\\wsl.localhost\Ubuntu"));
        assert!(server(r"\\server") && server(r"\\server\") && !server(r"\\server\share") && !server(r"\\"));
        assert_eq!(target("pc:").as_deref(), Some("pc:"));
        assert_eq!(target(r#"child:"\\nas\photos" beach"#).as_deref(), Some(r"\\nas\photos"));
        assert_eq!(target("holiday"), None);
    }

    #[test]
    fn a_folder_lists_its_files_filtered_and_sorted() {
        let d = std::env::temp_dir().join("mosaic-direct-test");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("b.txt"), "x").unwrap();
        std::fs::write(d.join("a.jpg"), "xy").unwrap();
        let dir = d.to_string_lossy().into_owned();
        let o = SearchOptions { query: format!("child:\"{dir}\""), limit: 100, folders_first: true, ascending: true, ..SearchOptions::default() };
        let (v, e) = list(&dir, &o);
        assert!(e.is_none());
        assert_eq!(v.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["sub", "a.jpg", "b.txt"]);
        assert_eq!(v[1].size, 2);
        let o = SearchOptions { query: format!("child:\"{dir}\" .txt"), ..o };
        assert_eq!(list(&dir, &o).0.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
