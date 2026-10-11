//! The GUI's search backend, chosen once at startup:
//!
//! - [`SearchBackend::Service`] — a background service is running and answered a
//!   version-matched `Ping`, so searches and status go over the IPC pipe and the
//!   GUI does not index at all.
//! - [`SearchBackend::Local`] — no usable service, so the (elevated) GUI indexes
//!   in-process exactly as it did before the service existed.
//!
//! The choice is fixed for the session: if a `Service` backend's service later
//! dies, queries report a degraded status rather than silently re-indexing
//! locally — a mid-session switch would surprise the user with a stale or
//! differently-scoped index.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::application::{Catalog, IndexStatus, SearchOptions, SearchResult};
use crate::infrastructure::indexing::Indexer;
use crate::infrastructure::service::client::ServiceClient;

/// Where searches and status are served from for this session.
pub enum SearchBackend {
    /// Query the running background service over IPC.
    Service(ServiceClient),
    /// Index in-process and query the local catalog.
    Local(Indexer),
}

/// A detached, `'static` snapshot of the backend's search capability, so a search
/// can run off the async (tokio) worker thread via `spawn_blocking` without
/// borrowing [`crate::presentation::state::AppState`]. Used by content search,
/// whose backend round-trip (pipe I/O or a large index scan) must not block a
/// tokio worker.
#[derive(Clone)]
pub enum SearchHandle {
    Service(ServiceClient),
    Local(Arc<RwLock<Catalog>>),
}

impl SearchHandle {
    pub fn search(&self, options: &SearchOptions) -> SearchResult {
        match self {
            SearchHandle::Service(client) => client.search(options),
            SearchHandle::Local(catalog) => catalog.read().search(options),
        }
    }

    /// Content-index search, from whichever process owns the index: the service when there is
    /// one (it writes the index as SYSTEM; an unelevated GUI can't open it), else this process.
    pub fn content(&self, query: &str, limit: usize) -> Result<Vec<String>, String> {
        match self {
            SearchHandle::Service(client) => client.content(query, limit),
            SearchHandle::Local(_) => Ok(crate::contentindex::search(query, limit)?.into_iter().map(|(p, _)| p).collect()),
        }
    }
}

impl SearchBackend {
    /// Probe for a usable service; use it if present, otherwise start in-process
    /// indexing and use that. Called once at startup.
    pub fn detect() -> Self {
        // installed but stopped (killed, or crashed past its restarts): start it, then give its pipe a moment
        let probe = || {
            ServiceClient::probe().or_else(|| {
                crate::infrastructure::service::scm::start().ok()?;
                (0..40).find_map(|_| {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    ServiceClient::probe()
                })
            })
        };
        match probe() {
            Some(client) => SearchBackend::Service(client),
            None => {
                let indexer = Indexer::new();
                indexer.start();
                SearchBackend::Local(indexer)
            }
        }
    }

    /// Run a search against whichever backend was selected.
    pub fn search(&self, options: &SearchOptions) -> SearchResult {
        match self {
            SearchBackend::Service(client) => client.search(options),
            SearchBackend::Local(indexer) => indexer.catalog().read().search(options),
        }
    }

    /// Current index status from the selected backend.
    pub fn status(&self) -> IndexStatus {
        match self {
            SearchBackend::Service(client) => client.status(),
            SearchBackend::Local(indexer) => indexer.status(),
        }
    }

    /// Whether the GUI is querying the background service (for the status bar).
    /// A detached handle for running a search off the async worker thread.
    pub fn handle(&self) -> SearchHandle {
        match self {
            SearchBackend::Service(client) => SearchHandle::Service(*client),
            SearchBackend::Local(indexer) => SearchHandle::Local(indexer.catalog()),
        }
    }
}
