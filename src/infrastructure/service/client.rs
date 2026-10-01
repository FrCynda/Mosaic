//! The client side of the query-only IPC: a thin transport over the named pipe.
//! Connection-per-request (matching the server), so the client holds no live
//! handle — each call opens a fresh connection, sends one framed [`Request`],
//! and reads one framed [`Response`]. Higher-level dual-mode dispatch lives in
//! `presentation::backend`.

use std::io;
use std::time::Duration;

use crate::application::ipc::{Request, Response, PIPE_NAME, PROTOCOL_VERSION};
use crate::application::{IndexStatus, SearchOptions, SearchResult};
use crate::infrastructure::service::framing::{read_frame, write_frame, MAX_RESPONSE};
use crate::infrastructure::service::pipe::connect;

/// A stateless handle to the background service. Cheap to clone/store.
#[derive(Clone, Copy)]
pub struct ServiceClient;

impl ServiceClient {
    /// Probe for a live service whose protocol matches ours. Returns a client
    /// only on a `Pong` carrying [`PROTOCOL_VERSION`] — a missing service, or
    /// one from a mismatched (e.g. mid-auto-update) build, yields `None` so the
    /// caller falls back to in-process indexing. Fails fast: no service means an
    /// instant `None`, not a stalled startup.
    pub fn probe() -> Option<Self> {
        match Self.exchange(&Request::Ping) {
            Ok(Response::Pong { protocol_version }) if protocol_version == PROTOCOL_VERSION => {
                Some(Self)
            }
            _ => None,
        }
    }

    /// Run a search via the service. A transport or protocol failure becomes a
    /// [`SearchResult`] carrying the error, so the UI degrades visibly instead
    /// of silently falling back to a different (local) index mid-session.
    pub fn search(&self, options: &SearchOptions) -> SearchResult {
        match self.exchange(&Request::Search(options.clone())) {
            Ok(Response::Search(result)) => result,
            Ok(Response::Error(message)) => SearchResult::error(message, 0),
            Ok(_) => SearchResult::error("unexpected response from service".into(), 0),
            Err(e) => SearchResult::error(format!("background service unavailable: {e}"), 0),
        }
    }

    /// Fetch the service's index status, degrading to an error status when the
    /// service can't be reached.
    pub fn status(&self) -> IndexStatus {
        match self.exchange(&Request::Status) {
            Ok(Response::Status(status)) => status,
            Ok(Response::Error(message)) => IndexStatus::error("service", message),
            Ok(_) => IndexStatus::error("service", "unexpected response from service"),
            Err(e) => IndexStatus::error("service", format!("background service unavailable: {e}")),
        }
    }

    /// One request/response round-trip over a fresh connection.
    fn exchange(&self, request: &Request) -> io::Result<Response> {
        // Fail fast (ZERO timeout): a dead service should degrade now, not stall.
        let mut conn = connect(PIPE_NAME, Duration::ZERO)?;
        write_frame(&mut conn, request)?;
        read_frame(&mut conn, MAX_RESPONSE)
    }
}
