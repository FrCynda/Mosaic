//! Application layer: orchestrates the domain. Defines the indexing contract,
//! owns the per-volume index and the multi-volume catalog, and contains no
//! platform or UI code.

pub mod catalog;
pub mod error;
pub mod index;
pub mod indexer;
pub mod ipc;
pub mod search;
pub mod status;

pub use catalog::Catalog;
pub use error::{IndexError, IndexResult};
pub use index::{Rec, SearchIndex, SearchResult};
pub use indexer::{RawRecord, VolumeEnumerator};
pub use search::SearchOptions;
pub use status::IndexStatus;
