//! Process elevation. The GUI runs unelevated (`asInvoker`); the operations that
//! genuinely need admin — installing or controlling the LocalSystem service —
//! relaunch this exe elevated via the UAC `runas` verb to run a one-shot
//! `--svc-*` command, then wait for its result.

use std::ffi::c_void;
use std::iter::once;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// `GetLastError` value when the user declines the UAC prompt.
const ERROR_CANCELLED: u32 = 1223;

/// Whether the current process holds an elevated token. A failure to read the
/// token is treated as "not elevated", so callers relaunch (and UAC decides).
pub fn is_elevated() -> bool {
    // SAFETY: standard token-query sequence; `token` is closed before returning.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

/// Relaunch this exe elevated to run `arg` (a `--svc-*` one-shot command), wait
/// for it, and map its exit code to a result. The relaunched process never
/// builds a window — it runs the command and exits.
pub fn run_elevated(arg: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(arg);

    // SAFETY: zeroed then every field we rely on is set; the wide buffers live
    // for the whole call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;

    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_CANCELLED {
            "elevation was declined".into()
        } else {
            format!("could not relaunch elevated (error {code})")
        });
    }
    if info.hProcess.is_null() {
        return Err("the elevated helper did not start".into());
    }

    // SAFETY: `hProcess` is a live process handle (SEE_MASK_NOCLOSEPROCESS),
    // closed exactly once below.
    let exit_code = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if got == 0 {
            return Err("could not read the elevated helper's result".into());
        }
        code
    };

    if exit_code == 0 {
        Ok(())
    } else {
        Err("the service operation failed when run elevated".into())
    }
}
