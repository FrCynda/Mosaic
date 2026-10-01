//! Raw read-only access to an NTFS volume via `\\.\C:`-style device paths.
//! Requires Administrator rights; opening the volume otherwise fails with
//! access-denied.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::application::IndexError;

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_BEGIN: u32 = 0;

/// An open handle to a raw volume. Read-only; cloned handles are not supported.
pub struct Volume {
    handle: HANDLE,
    drive: char,
}

impl Volume {
    /// Open the volume for drive letter `drive` (e.g. `'C'`).
    pub fn open(drive: char) -> Result<Self, IndexError> {
        let volume = format!("{drive}:");
        let device: Vec<u16> = format!(r"\\.\{drive}:")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `device` is a valid null-terminated UTF-16 string; all other
        // pointers are null where the API permits.
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return Err(if code == ERROR_ACCESS_DENIED {
                IndexError::AccessDenied(volume)
            } else {
                IndexError::Open { volume, code }
            });
        }

        Ok(Self { handle, drive })
    }

    /// Issue a `DeviceIoControl` against the volume handle. Returns the number
    /// of bytes written to `out_buf`, or the OS error code on failure.
    ///
    /// # Safety
    /// `in_buf`/`out_buf` must be valid for the given sizes for the duration of
    /// the call.
    pub unsafe fn device_io_control(
        &self,
        code: u32,
        in_buf: *const c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
    ) -> Result<u32, u32> {
        let mut returned: u32 = 0;
        let ok = DeviceIoControl(
            self.handle,
            code,
            in_buf,
            in_size,
            out_buf,
            out_size,
            &mut returned,
            ptr::null_mut(),
        );
        if ok == 0 {
            Err(GetLastError())
        } else {
            Ok(returned)
        }
    }

    /// Fill `buf` with bytes read starting at absolute byte `offset`.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), IndexError> {
        let err = |code| IndexError::Read {
            volume: format!("{}:", self.drive),
            offset,
            code,
        };

        // SAFETY: handle is valid; passing null for the new-pointer out-param.
        let ok =
            unsafe { SetFilePointerEx(self.handle, offset as i64, ptr::null_mut(), FILE_BEGIN) };
        if ok == 0 {
            return Err(err(unsafe { GetLastError() }));
        }

        let mut filled = 0usize;
        while filled < buf.len() {
            let mut read: u32 = 0;
            let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
            // SAFETY: writing into the valid, in-bounds tail of `buf`.
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf[filled..].as_mut_ptr(),
                    want,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(err(unsafe { GetLastError() }));
            }
            if read == 0 {
                return Err(err(0)); // unexpected end of volume
            }
            filled += read as usize;
        }
        Ok(())
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: handle was produced by CreateFileW and is not closed twice.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
