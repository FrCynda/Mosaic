//! File actions invoked from the results context menu: rename, delete to the
//! Recycle Bin, and the shell verbs (Properties, Open with, Run as
//! administrator). Thin, best-effort wrappers over the Win32 shell APIs.
//!
//! The shell *verbs* open dialogs/processes and must run on the UI thread (which
//! owns a message pump); the presentation layer dispatches them via Tauri's
//! `run_on_main_thread`. Rename and recycle are synchronous, UI-less file
//! operations safe to run on any thread.

use std::iter::once;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_CANCELLED};
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
use windows_sys::Win32::UI::Shell::{
    SHFileOperationW, ShellExecuteExW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_DELETE,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, SHFILEOPSTRUCTW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Characters Windows forbids in a file name.
const INVALID_NAME_CHARS: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Rename the item at `path` to `new_name` within the same directory, returning
/// the new absolute path. Validates the name and refuses to overwrite.
pub fn rename(path: &str, new_name: &str) -> Result<String, String> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("Name cannot be empty".into());
    }
    if new_name == "." || new_name == ".." || new_name.contains(INVALID_NAME_CHARS) {
        return Err("Name contains invalid characters".into());
    }

    let src = Path::new(path);
    let parent = src.parent().ok_or("Cannot rename a drive root")?;
    let dest = parent.join(new_name);
    if dest == src {
        return Ok(path.to_string());
    }
    // On case-insensitive volumes `dest.exists()` is true for a case-only rename
    // (a.txt -> A.txt), which is a valid self-rename, not a collision.
    let case_only = dest.exists() && same_file(src, &dest);
    if dest.exists() && !case_only {
        return Err(format!("\"{new_name}\" already exists"));
    }
    // Move atomically via MoveFileExW. Pass MOVEFILE_REPLACE_EXISTING only for a
    // case-only self-rename; otherwise no replace flag, so the OS itself fails if
    // `dest` appeared in the gap after the check above. This closes the TOCTOU
    // window that a check-then-`std::fs::rename` (which replaces) leaves open —
    // honouring the "refuses to overwrite" contract even under concurrency.
    let src_w: Vec<u16> = src.as_os_str().encode_wide().chain(once(0)).collect();
    let dest_w: Vec<u16> = dest.as_os_str().encode_wide().chain(once(0)).collect();
    let flags = if case_only {
        MOVEFILE_REPLACE_EXISTING
    } else {
        0
    };
    // SAFETY: both wide strings are null-terminated and outlive the call.
    let ok = unsafe { MoveFileExW(src_w.as_ptr(), dest_w.as_ptr(), flags) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("rename failed (code {code})"));
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// Whether two paths resolve to the same on-disk file (so a case-only rename on
/// a case-insensitive volume isn't mistaken for a collision). Conservative: any
/// canonicalization failure is treated as "different" so a real collision is
/// never silently overwritten.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Move the item at `path` to the Recycle Bin. UI-less and non-interactive — the
/// caller is expected to confirm beforehand.
pub fn recycle(path: &str) -> Result<(), String> {
    recycle_many(std::slice::from_ref(&path.to_string()))
}

/// Move every item in `paths` to the Recycle Bin in a single shell operation, so
/// they share one undo group and one progress UI. UI-less and non-interactive.
pub fn recycle_many(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    // `pFrom` is a double-null-terminated, null-separated list of paths: each path
    // is NUL-terminated and the whole list ends with an extra NUL.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);

    let mut op: SHFILEOPSTRUCTW = unsafe { zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT) as u16;

    // SAFETY: `op` is fully initialized and `from` outlives the call.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(format!("delete failed (code {rc})"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("delete was cancelled".into());
    }
    Ok(())
}

/// A shell verb the context menu can invoke.
#[derive(Clone, Copy)]
pub enum ShellVerb {
    Properties,
    OpenWith,
    RunAsAdmin,
}

impl ShellVerb {
    /// Parse the UI's action name into a verb, rejecting anything unknown.
    pub fn parse(action: &str) -> Option<Self> {
        match action {
            "properties" => Some(Self::Properties),
            "open_with" => Some(Self::OpenWith),
            "run_as" => Some(Self::RunAsAdmin),
            _ => None,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Properties => "properties",
            Self::OpenWith => "openas",
            Self::RunAsAdmin => "runas",
        }
    }
}

/// Invoke a shell verb on `path`. Must be called on the UI thread, which owns the
/// message pump the resulting dialog needs. A user dismissing the dialog (e.g.
/// declining the UAC prompt for Run as administrator) is treated as success, not
/// an error.
pub fn shell_verb(path: &str, verb: ShellVerb) -> Result<(), String> {
    let verb_w: Vec<u16> = verb.verb().encode_utf16().chain(once(0)).collect();
    let file_w: Vec<u16> = path.encode_utf16().chain(once(0)).collect();

    let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    // No SEE_MASK_NOASYNC: the UI thread persists, so the verb need not block the
    // event loop waiting for the operation to finish.
    info.fMask = SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_NO_UI;
    info.hwnd = ptr::null_mut();
    info.lpVerb = verb_w.as_ptr();
    info.lpFile = file_w.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    // SAFETY: `info` is fully initialized; the wide strings outlive the call.
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        // SAFETY: read immediately after the failed call, same thread.
        if unsafe { GetLastError() } == ERROR_CANCELLED {
            return Ok(());
        }
        return Err(format!("shell action '{}' failed", verb.verb()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_rejects_invalid_names() {
        assert!(rename("C:\\x\\a.txt", "").is_err());
        assert!(rename("C:\\x\\a.txt", "  ").is_err());
        assert!(rename("C:\\x\\a.txt", "b\\c.txt").is_err());
        assert!(rename("C:\\x\\a.txt", "a:b").is_err());
        assert!(rename("C:\\x\\a.txt", "..").is_err());
    }

    #[test]
    fn rename_moves_file_in_place() {
        let dir = std::env::temp_dir().join("att_rename_test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("before.txt");
        std::fs::write(&src, b"hi").unwrap();
        let dst = rename(src.to_str().unwrap(), "after.txt").expect("rename");
        assert!(!src.exists(), "source should be gone");
        assert!(Path::new(&dst).exists(), "dest should exist");
        assert!(dst.ends_with("after.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_allows_case_only_change() {
        let dir = std::env::temp_dir().join("att_rename_case_test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("casefile.txt");
        std::fs::write(&src, b"x").unwrap();
        let dst = rename(src.to_str().unwrap(), "CaseFile.txt").expect("case-only rename");
        assert!(dst.ends_with("CaseFile.txt"), "got {dst}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "moves a file to the Recycle Bin (side effect)"]
    fn recycle_removes_file() {
        let dir = std::env::temp_dir().join("att_recycle_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("trash.txt");
        std::fs::write(&f, b"bin me").unwrap();
        recycle(f.to_str().unwrap()).expect("recycle");
        assert!(!f.exists(), "file should be removed from disk");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
