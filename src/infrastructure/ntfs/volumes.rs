//! Discovery of fixed NTFS volumes to index.

use std::iter::once;
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

const DRIVE_FIXED: u32 = 3;

/// Enumerate the drive letters of all fixed (non-removable) NTFS volumes.
pub fn ntfs_fixed_drives() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();

    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\")
            .encode_utf16()
            .chain(once(0))
            .collect();

        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }

        let mut fs_name = [0u16; 16];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            continue;
        }

        let fs = String::from_utf16_lossy(&fs_name);
        if fs.trim_end_matches('\0').eq_ignore_ascii_case("NTFS") {
            drives.push(letter);
        }
    }
    drives
}

/// The volume serial number for `drive`, used to detect a reformatted volume
/// when validating a cached index. Returns `0` if it cannot be read.
pub fn volume_serial(drive: char) -> u32 {
    let root: Vec<u16> = format!("{drive}:\\")
        .encode_utf16()
        .chain(once(0))
        .collect();
    let mut serial: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            ptr::null_mut(),
            0,
            &mut serial,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        0
    } else {
        serial
    }
}
