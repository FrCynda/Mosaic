//! The query-only IPC protocol between the GUI client and the background
//! service. Platform-free: just the request/response envelope reusing the
//! existing search DTOs. Transport (named pipe), framing, and the server live in
//! `infrastructure::service`.
//!
//! The protocol is deliberately **query-only** — no request can make the
//! (elevated) service open, write, rename, delete, or execute anything. All such
//! actions stay in the GUI, running in the user's own context.

use serde::{Deserialize, Serialize};

use crate::application::{IndexStatus, SearchOptions, SearchResult};

/// The named pipe the service hosts and the GUI connects to.
pub const PIPE_NAME: &str = r"\\.\pipe\AllTheThings";

/// Bumped on any incompatible wire change. A client that gets a different
/// version from `Pong` falls back to in-process indexing rather than risk
/// misparsing a mismatched service from an in-flight auto-update.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request from the GUI to the service.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Run a search and return ranked hits.
    Search(SearchOptions),
    /// Fetch the indexer's status snapshot.
    Status,
    /// Liveness + version probe used to decide whether to use the service.
    Ping,
}

/// A response from the service to the GUI.
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Search(SearchResult),
    Status(IndexStatus),
    Pong {
        protocol_version: u32,
    },
    /// A server-side failure, surfaced to the user as degraded status.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let req = Request::Search(SearchOptions {
            query: "foo bar".into(),
            ..SearchOptions::default()
        });
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Search(o) => assert_eq!(o.query, "foo bar"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_roundtrips() {
        let resp = Response::Pong {
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, Response::Pong { protocol_version } if protocol_version == PROTOCOL_VERSION)
        );
    }
}
