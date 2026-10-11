//! Delete to the Recycle Bin: a thin, best-effort wrapper over the Win32 shell API.

use std::mem::zeroed;

use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};

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
