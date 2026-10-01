//! Errors raised while building or maintaining the index. Infrastructure
//! implementations map OS failures onto these variants.

/// Failures that can occur while reading a volume and constructing the index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("access denied opening {0} — run Mosaic as Administrator")]
    AccessDenied(String),

    #[error("failed to open volume {volume}: OS error {code}")]
    Open { volume: String, code: u32 },

    #[error("I/O error reading {volume} at offset {offset}: OS error {code}")]
    Read {
        volume: String,
        offset: u64,
        code: u32,
    },

    #[error("{0} is not an NTFS volume")]
    NotNtfs(String),

    #[error("malformed MFT on {volume}: {detail}")]
    Malformed { volume: String, detail: String },
}

pub type IndexResult<T> = Result<T, IndexError>;
