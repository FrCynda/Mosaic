//! The named pipe's access control. The service runs as LocalSystem and indexes
//! every volume, so the pipe must let a *non-elevated* user connect while
//! excluding anonymous/remote callers.

use std::iter::once;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

const SDDL_REVISION_1: u32 = 1;

/// DACL: Authenticated Users get generic read+write (duplex request/response);
/// SYSTEM and Administrators get full control. No Everyone (`WD`) or Anonymous
/// (`AN`) ACE. No owner/group clause, so the owner defaults to the creating
/// principal — LocalSystem for the service — which avoids needing
/// `SeRestorePrivilege` to set an explicit owner (and lets the in-process test
/// create the pipe as a standard user).
const PIPE_SDDL: &str = "D:(A;;GRGW;;;AU)(A;;GA;;;SY)(A;;GA;;;BA)";

/// Owns a security descriptor plus a `SECURITY_ATTRIBUTES` referencing it, for
/// passing to `CreateNamedPipeW`. Frees the descriptor on drop.
pub struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

// The descriptor is owned exclusively and only read (during pipe creation on the
// owning thread), so moving the value across threads is sound.
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    /// Build the pipe security from the fixed SDDL.
    pub fn new() -> Result<Self, String> {
        let wide: Vec<u16> = PIPE_SDDL.encode_utf16().chain(once(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `wide` is a valid null-terminated SDDL string; `descriptor` is
        // a valid out-pointer the call fills with a LocalAlloc'd descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "failed to build pipe security descriptor (error {})",
                unsafe { GetLastError() }
            ));
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(Self { descriptor, attrs })
    }

    /// Pointer to the `SECURITY_ATTRIBUTES`, valid for `&self`'s lifetime.
    pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attrs
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: `descriptor` was allocated by the conversion call above.
        unsafe {
            LocalFree(self.descriptor as _);
        }
    }
}
