//! The contract between the search index and any source that can stream file
//! records off a volume. Defined here (inner layer); implemented by the NTFS
//! MFT reader in `infrastructure`.

use crate::application::error::IndexResult;

/// A single file record streamed from a volume, before path reconstruction.
#[derive(Debug, Clone)]
pub struct RawRecord {
    /// This record's MFT number.
    pub record_no: u64,
    /// The MFT number of the containing directory.
    pub parent_no: u64,
    /// File or directory name (Win32 namespace, not 8.3 DOS).
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes; `None` for directories or when unknown.
    pub size: Option<u64>,
    /// Windows FILETIME (100 ns ticks since 1601-01-01); `0` when unknown.
    pub modified_ft: u64,
    /// Creation time as a Windows FILETIME; `0` when unknown.
    pub created_ft: u64,
    /// Last-access time as a Windows FILETIME; `0` when unknown.
    pub accessed_ft: u64,
    /// Windows DOS file attributes bitmask (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
}

/// A source that can enumerate every in-use file record on a volume.
pub trait VolumeEnumerator {
    /// The drive letter this enumerator reads, e.g. `'C'`.
    fn drive(&self) -> char;

    /// Stream every in-use record into `sink`. Returns once the whole MFT has
    /// been read.
    fn enumerate(&mut self, sink: &mut dyn FnMut(RawRecord)) -> IndexResult<()>;
}
