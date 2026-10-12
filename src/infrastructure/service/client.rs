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

    /// Full-text content search via the service, which owns the content index — paths, best first.
    pub fn content(&self, query: &str, limit: usize) -> Result<Vec<String>, String> {
        match self.exchange(&Request::Content { query: query.into(), limit }) {
            Ok(Response::Content(paths)) => Ok(paths),
            // a service from before content search moved over IPC can't parse the request
            Ok(Response::Error(m)) if m.starts_with("bad request") => {
                Err("the background service is out of date — reinstall it in Settings to search content".into())
            }
            Ok(Response::Error(message)) => Err(message),
            Ok(_) => Err("unexpected response from service".into()),
            Err(e) => Err(format!("background service unavailable: {e}")),
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

#[cfg(test)]
mod tests {
    /// Round trips against the installed service: `cargo test --release bench_service -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_service() {
        use super::*;
        use std::time::Instant;
        let c = ServiceClient::probe().expect("service up");
        let med = |f: &dyn Fn()| {
            let mut t: Vec<f64> = (0..9).map(|_| { let s = Instant::now(); f(); s.elapsed().as_secs_f64() * 1000.0 }).collect();
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            t[4]
        };
        println!("BENCH svc_status   median_ms={:.2}", med(&|| { c.status(); }));
        let o = |q: &str, offset| SearchOptions { query: q.into(), limit: 100, offset, folders_first: true, ..SearchOptions::default() };
        for q in ["", "win", "ext:jpg;png;gif", "zzqxj"] {
            // alternate direction so each call is a fresh rank, not a view-cache page
            let n = std::cell::Cell::new(false);
            let cold = med(&|| { n.set(!n.get()); c.search(&SearchOptions { ascending: n.get(), ..o(q, 0) }); });
            c.search(&o(q, 0));
            let page = med(&|| { c.search(&o(q, 300)); });
            println!("BENCH svc_q {q:<16} cold_ms={cold:7.2} page_ms={page:6.2}");
        }
        // the first search after the service sat idle (its working set trimmed meanwhile)
        if std::env::var_os("MOSAIC_BENCH_IDLE").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(65));
            let s = Instant::now();
            c.search(&o("report", 0));
            println!("BENCH svc_after_idle_ms={:.2}", s.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
