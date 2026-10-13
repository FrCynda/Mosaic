//! The background service runtime, hosted by the Windows Service Control
//! Manager. Builds an [`Indexer`] (running as LocalSystem, so it can read every
//! volume's MFT), then serves the **query-only** IPC protocol over the named
//! pipe — one short-lived handler thread per connection, each answering exactly
//! one request.
//!
//! Reached only via the `--service` command-line branch, which the SCM uses to
//! launch the installed service; a normal GUI launch never enters here.

use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::application::ipc::{Request, Response, PIPE_NAME, PROTOCOL_VERSION};
use crate::infrastructure::indexing::Indexer;
use crate::infrastructure::service::framing::{read_frame, write_frame, MAX_REQUEST};
use crate::infrastructure::service::pipe::{PipeConnection, PipeServer};

/// The service's registered name (the key under SCM, and the `sc query` target).
pub const SERVICE_NAME: &str = "AllTheThingsSvc";

const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Upper bound on hits the service returns for one query. The protocol accepts
/// `SearchOptions` from any authenticated user, so the client-supplied `limit`
/// is clamped here to bound the result set the LocalSystem service builds and
/// serializes — otherwise a hostile `limit` is a memory-amplification lever.
/// Comfortably exceeds the GUI's interactive limit and most exports; an export
/// that matches more rows than this is reported as capped (total > written).
const MAX_SERVICE_HITS: usize = 200_000;

/// Cap on concurrently-served connections. Each connection gets its own thread
/// and may build a result set up to [`MAX_SERVICE_HITS`]; bounding the count
/// bounds worst-case threads and memory against a client that floods the pipe.
const MAX_INFLIGHT: usize = 64;

/// How long one request may take before its connection's blocking I/O is
/// cancelled. Generous for a real query (searches run in tens of ms), but bounds
/// a client that connects and never sends (or never reads) a full frame — which
/// would otherwise pin a handler thread, and its inflight slot, forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

define_windows_service!(ffi_service_main, service_main);

/// Hand control to the SCM dispatcher, which calls back into [`service_main`].
/// Returns an error (rather than blocking) when the process was launched with
/// `--service` outside the SCM — e.g. a user running it from a console.
pub fn run() -> Result<(), String> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.to_string())
}

/// Entry point the SCM dispatcher invokes on the service's own thread.
fn service_main(_arguments: Vec<OsString>) {
    // A failure here can only be surfaced to the SCM (no console/UI). Reporting
    // the stopped state, and letting the process exit, is the only signal.
    let _ = run_service();
}

/// Register the control handler, start indexing, serve the pipe until the SCM
/// asks us to stop, then report stopped.
fn run_service() -> windows_service::Result<()> {
    // The control handler (called on an SCM thread) and a fatal serve error both
    // request shutdown through this channel; the main service thread waits on it.
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let handler_tx = shutdown_tx.clone();
    let event_handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = handler_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(status(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(15),
    ))?;

    // Build and start the index. Shared with every handler thread for the
    // process's lifetime; methods take `&self` and lock internally.
    let indexer = Arc::new(Indexer::new());
    indexer.start();
    // below-normal CPU priority (the cap below bounds the total); not background mode, whose idle I/O priority
    // made the scan crawl whenever anything else touched the disk
    unsafe { windows_sys::Win32::System::Threading::SetPriorityClass(windows_sys::Win32::System::Threading::GetCurrentProcess(), 0x0000_4000) }; // BELOW_NORMAL_PRIORITY_CLASS
    cap_cpu();
    trim_when_idle();

    let serve_indexer = indexer.clone();
    thread::spawn(move || {
        if serve(serve_indexer).is_err() {
            // Couldn't host the pipe (e.g. a squatter holds the name): ask the
            // service to stop rather than report Running while non-functional.
            let _ = shutdown_tx.send(());
        }
    });

    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, // shutdown too, or a power-off just kills us
        Duration::default(),
    ))?;

    // Block until Stop/Shutdown (or a fatal serve error) is signalled.
    let _ = shutdown_rx.recv();

    status_handle.set_service_status(status(
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(5),
    ))?;
    crate::infrastructure::indexing::save_caches(); // the last few minutes' changes, appended to the cache's log
    crate::contentindex::shutdown(Duration::from_secs(3));
    status_handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::default(),
    ))?;
    // The process exits right after, tearing down the (blocked) accept thread and
    // any in-flight read-only handlers — safe for a query-only service.
    Ok(())
}

/// A `ServiceStatus` for the given lifecycle state with a clean exit code.
fn status(
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

/// Service uptime (seconds) of the last search, for [`trim_when_idle`].
static LAST_SEARCH: AtomicU64 = AtomicU64::new(0);
/// Nobody searched for this long: the index's RAM goes back to Windows.
const IDLE_TRIM_S: u64 = 30;

fn uptime_s() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs()
}

/// While nobody searches, keep the working set trimmed: the index's pages move to Windows' standby list (still
/// cached in RAM, but free for any program that needs it), so an idle service holds a few MB. The next search
/// faults them back in, tens of ms from RAM. Re-trimmed periodically because the journal watcher and the cache
/// saver touch pages meanwhile; trimming an already-trimmed set costs next to nothing.
fn trim_when_idle() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(IDLE_TRIM_S));
        if uptime_s().saturating_sub(LAST_SEARCH.load(Ordering::Relaxed)) >= IDLE_TRIM_S {
            // SAFETY: (-1, -1) is the documented "trim as much as possible" request on our own process.
            unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
        }
    });
}

/// Hard ceiling on the service's total CPU (percent of the whole machine, all cores): a burst of file changes, a
/// merge or a re-sort can't take over the machine, it just takes longer. A job object enforces it for every thread.
const CPU_CAP_PERCENT: u32 = 50;

fn cap_cpu() {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls on a job we create here and a handle to our own process.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
        info.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        info.Anonymous.CpuRate = CPU_CAP_PERCENT * 100; // hundredths of a percent
        let set = SetInformationJobObject(job, JobObjectCpuRateControlInformation, &info as *const _ as *const _, size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32);
        if set != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

/// Accept loop: host the pipe and spawn one handler thread per connection. A
/// failure to create the *first* pipe instance is fatal (returned as `Err`);
/// transient per-connection errors are tolerated so a single misbehaving client
/// can't stop the service.
fn serve(indexer: Arc<Indexer>) -> Result<(), String> {
    let mut server = PipeServer::new(PIPE_NAME)?;
    let inflight = Arc::new(AtomicUsize::new(0));
    let mut ever_accepted = false;

    loop {
        match server.accept() {
            Ok(conn) => {
                ever_accepted = true;
                // Shed load past the concurrency cap: drop the connection (it
                // closes on `conn`'s drop) rather than spawn an unbounded thread.
                if inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT {
                    continue;
                }
                inflight.fetch_add(1, Ordering::Relaxed);
                let indexer = indexer.clone();
                let inflight = inflight.clone();
                thread::spawn(move || {
                    serve_connection(conn, &indexer, REQUEST_TIMEOUT);
                    inflight.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(e) => {
                if !ever_accepted {
                    // The first instance never came up — fatal (a name squatter,
                    // or the security descriptor was rejected).
                    return Err(format!("failed to host pipe {PIPE_NAME}: {e}"));
                }
                // Transient: back off briefly so we don't spin on a tight error.
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Serve one request under a deadline: a watchdog cancels the connection's
/// blocking I/O if the handler hasn't finished within [`REQUEST_TIMEOUT`], so a
/// peer that connects and never sends (or never reads) a full frame can't pin
/// the handler thread and its inflight slot.
fn serve_connection(mut conn: PipeConnection, indexer: &Indexer, timeout: Duration) {
    let canceller = conn.canceller();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            canceller.cancel();
        }
    });

    let _ = handle_connection(&mut conn, indexer);

    // Signal completion, then join the watchdog BEFORE `conn` drops so the
    // canceller can never touch a closed (and possibly reused) handle.
    let _ = done_tx.send(());
    let _ = watchdog.join();
}

/// Serve exactly one request on a connection. A malformed request is answered
/// with [`Response::Error`] (so the client sees a clear failure rather than a
/// dropped pipe) and the connection then closes.
fn handle_connection(conn: &mut PipeConnection, indexer: &Indexer) -> std::io::Result<()> {
    let request = match read_frame::<_, Request>(conn, MAX_REQUEST) {
        Ok(req) => req,
        Err(e) => {
            return write_frame(conn, &Response::Error(format!("bad request: {e}")));
        }
    };

    let response = match request {
        Request::Search(mut options) => {
            LAST_SEARCH.store(uptime_s(), Ordering::Relaxed);
            options.limit = options.limit.min(MAX_SERVICE_HITS);
            Response::Search(indexer.catalog().read().search(&options))
        }
        Request::Content { query, limit } => {
            LAST_SEARCH.store(uptime_s(), Ordering::Relaxed);
            match crate::contentindex::search(&query, limit.min(MAX_SERVICE_HITS)) {
                Ok(hits) => Response::Content(hits.into_iter().map(|(p, _)| p).collect()),
                Err(e) => Response::Error(e),
            }
        }
        Request::Status => Response::Status(indexer.status()),
        Request::Ping => Response::Pong {
            protocol_version: PROTOCOL_VERSION,
        },
    };
    write_frame(conn, &response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::service::pipe::connect;
    use std::time::Instant;

    /// A client that connects but never sends a full frame must not pin the
    /// handler thread: the deadline watchdog cancels the blocked read so the
    /// connection (and its inflight slot) is released. Guards the slot-exhaustion
    /// DoS the adversarial review caught.
    #[test]
    fn stalled_request_is_reaped_by_timeout() {
        let name = format!(r"\\.\pipe\att-test-reap-{}", std::process::id());
        let server_name = name.clone();

        let server = thread::spawn(move || {
            let mut server = PipeServer::new(&server_name).expect("create server");
            let conn = server.accept().expect("accept");
            // No `start()`, so no volume access — the stalled read is reaped
            // before any request reaches the (empty) indexer.
            let indexer = Indexer::new();
            let began = Instant::now();
            serve_connection(conn, &indexer, Duration::from_millis(200));
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "serve_connection hung on a silent client instead of timing out"
            );
        });

        // Connect and send nothing; hold the pipe open past the server's timeout.
        let client = connect(&name, Duration::from_secs(2)).expect("connect");
        server.join().expect("server thread");
        drop(client);
    }
}
