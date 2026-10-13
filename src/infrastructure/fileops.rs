//! Delete (to the Recycle Bin, or for good), move, and restore from the Recycle Bin: thin, best-effort wrappers.

use std::mem::zeroed;
use std::path::{Path, PathBuf};

use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_MULTIDESTFILES, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_SILENT, FO_COPY, FO_DELETE, FO_MOVE, SHFILEOPSTRUCTW};

/// `pFrom` / `pTo` lists: each path NUL-terminated, the whole list ending with an extra NUL.
fn list<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<u16> {
    let mut v: Vec<u16> = paths.flat_map(|p| p.encode_utf16().chain([0])).collect();
    v.push(0);
    v
}

fn shell_op(func: u32, from: &[u16], to: Option<&[u16]>, flags: u32) -> Result<(), String> {
    let mut op: SHFILEOPSTRUCTW = unsafe { zeroed() };
    op.wFunc = func;
    op.pFrom = from.as_ptr();
    op.pTo = to.map_or(std::ptr::null(), |t| t.as_ptr());
    op.fFlags = (flags | FOF_NOCONFIRMATION | FOF_SILENT) as u16;
    // SAFETY: `op` is fully initialized and the lists outlive the call.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(format!("failed (code {rc})"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Delete every item in `paths` in a single shell operation: to the Recycle Bin with `recycle` (one undo group and
/// one progress UI), else for good. UI-less and non-interactive: the caller asks first.
pub fn delete_many(paths: &[String], recycle: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    shell_op(FO_DELETE, &list(paths.iter().map(|p| p.as_str())), None, if recycle { FOF_ALLOWUNDO } else { 0 })
}

/// Move each `(from, to)`, across volumes too, making `to`'s folders as needed.
pub fn move_many(pairs: &[(String, String)]) -> Result<(), String> {
    if pairs.is_empty() {
        return Ok(());
    }
    let (from, to) = (list(pairs.iter().map(|p| p.0.as_str())), list(pairs.iter().map(|p| p.1.as_str())));
    shell_op(FO_MOVE, &from, Some(&to), FOF_MULTIDESTFILES | FOF_NOCONFIRMMKDIR | FOF_ALLOWUNDO)
}

/// Copy or move `paths` into the folder `dir` the way Explorer does a drop: its progress, and its questions when a
/// name is taken (replace, skip, keep both). Undoable from Explorer too.
pub fn transfer(paths: &[String], dir: &str, copy: bool, owner: isize) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let (from, to) = (list(paths.iter().map(|p| p.as_str())), list(std::iter::once(dir)));
    let mut op: SHFILEOPSTRUCTW = unsafe { zeroed() };
    op.hwnd = owner as _;
    op.wFunc = if copy { FO_COPY } else { FO_MOVE };
    op.pFrom = from.as_ptr();
    op.pTo = to.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR) as u16;
    // SAFETY: `op` is fully initialized and the lists outlive the call.
    match unsafe { SHFileOperationW(&mut op) } {
        0 if op.fAnyOperationsAborted != 0 => Err("cancelled".into()),
        0 => Ok(()),
        rc => Err(format!("failed (code {rc})")),
    }
}

/// A Recycle Bin `$I…` record: when the item was deleted (FILETIME) and the path it was deleted from.
fn recycled_from(b: &[u8]) -> Option<(u64, String)> {
    let num = |r: std::ops::Range<usize>| Some(u64::from_le_bytes(b.get(r)?.try_into().ok()?));
    let name = match num(0..8)? {
        1 => b.get(24..24 + 520)?, // Vista – 8.1: a fixed MAX_PATH buffer
        2 => {
            let n = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            b.get(28..28 + n * 2)?
        }
        _ => return None,
    };
    let w: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    Some((num(16..24)?, String::from_utf16_lossy(&w)))
}

/// Put each of `paths` back from the Recycle Bin where it was deleted from (its newest deletion), as Explorer's
/// Restore does: the bin's `$R…` item moved back and its `$I…` record dropped. Returns how many came back.
pub fn restore(paths: &[String]) -> usize {
    let mut back = 0;
    for p in paths {
        let Some(drive) = p.get(..3).filter(|d| d.ends_with(":\\")) else { continue };
        let want = p.to_lowercase();
        let mut best: Option<(u64, PathBuf)> = None;
        // only the user's own SID folder is readable: the rest are skipped
        for sid in std::fs::read_dir(Path::new(drive).join("$Recycle.Bin")).into_iter().flatten().flatten() {
            for e in std::fs::read_dir(sid.path()).into_iter().flatten().flatten() {
                if !e.file_name().to_string_lossy().starts_with("$I") {
                    continue;
                }
                let Some((when, from)) = std::fs::read(e.path()).ok().and_then(|b| recycled_from(&b)) else { continue };
                if from.to_lowercase() == want && best.as_ref().is_none_or(|b| when > b.0) {
                    best = Some((when, e.path()));
                }
            }
        }
        let Some((_, info)) = best else { continue };
        let item = info.with_file_name(format!("$R{}", &info.file_name().unwrap_or_default().to_string_lossy()[2..]));
        if Path::new(p).exists() {
            continue; // something new took its place: leave both alone
        }
        if let Some(dir) = Path::new(p).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::rename(&item, p).is_ok() {
            let _ = std::fs::remove_file(&info);
            back += 1;
        }
    }
    back
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_recycle_record_versions() {
        let path: Vec<u8> = "C:\\a\\b.txt".encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
        let mut v2 = [2u64.to_le_bytes(), 5u64.to_le_bytes(), 77u64.to_le_bytes()].concat();
        v2.extend(((path.len() / 2) as u32).to_le_bytes());
        v2.extend(&path);
        assert_eq!(recycled_from(&v2), Some((77, "C:\\a\\b.txt".into())));
        let mut v1 = [1u64.to_le_bytes(), 5u64.to_le_bytes(), 9u64.to_le_bytes()].concat();
        v1.extend(&path);
        v1.resize(24 + 520, 0);
        assert_eq!(recycled_from(&v1), Some((9, "C:\\a\\b.txt".into())));
        assert_eq!(recycled_from(&[3, 0, 0]), None);
    }
}
