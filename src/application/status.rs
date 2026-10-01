//! Index lifecycle status, surfaced to the UI via a Tauri command.

use serde::{Deserialize, Serialize};

/// A snapshot of the index's state for the frontend status bar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    /// One of `"indexing"`, `"ready"`, `"error"`.
    pub state: String,
    pub count: usize,
    pub volume: String,
    pub message: String,
    /// Bumped whenever the live index applies file changes: the UI re-runs its search when it moves.
    #[serde(default)]
    pub changes: u64,
    /// The content index's one-time bulk-scan progress (see `contentindex::progress`), 0/0 once
    /// done or before it starts. Filled in by whichever process actually runs the scan — the
    /// service, when one's installed, otherwise the GUI itself — so `#[serde(default)]` lets an
    /// older peer's response (missing the field) still deserialize as "nothing to report".
    #[serde(default)]
    pub content_done: usize,
    #[serde(default)]
    pub content_total: usize,
}

impl IndexStatus {
    pub fn indexing(volume: impl Into<String>, count: usize) -> Self {
        Self {
            state: "indexing".into(),
            count,
            volume: volume.into(),
            message: String::new(),
            changes: 0,
            content_done: 0,
            content_total: 0,
        }
    }

    pub fn ready(volume: impl Into<String>, count: usize) -> Self {
        Self {
            state: "ready".into(),
            count,
            volume: volume.into(),
            message: String::new(),
            changes: 0,
            content_done: 0,
            content_total: 0,
        }
    }

    pub fn error(volume: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            state: "error".into(),
            count: 0,
            volume: volume.into(),
            message: message.into(),
            changes: 0,
            content_done: 0,
            content_total: 0,
        }
    }
}
