//! Service Control Manager lifecycle for the optional background service:
//! install, uninstall, start, stop, and a reconciled status read.
//!
//! Status reads need only `CONNECT` + `QUERY_STATUS`, which an unelevated
//! caller has, so the GUI can always reflect real SCM state. Install / start /
//! stop / delete require elevation; in this phase the GUI is still elevated, so
//! they call straight through (an elevated relaunch is a later phase). The
//! current state is always read back from the SCM rather than cached, so a
//! service removed or stopped out-of-band can never leave the UI stale.

use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::infrastructure::service::host::SERVICE_NAME;

const DISPLAY_NAME: &str = "Mosaic File Index Service";
const DESCRIPTION: &str =
    "Indexes NTFS volumes in the background so Mosaic can search without elevation.";

/// Win32 codes we special-case.
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_EXISTS: i32 = 1073;

/// How long [`uninstall`] waits for a running service to stop before deleting.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// The service's lifecycle as the Settings UI needs it — derived live from the
/// SCM on every read.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SvcState {
    /// No service is registered.
    NotInstalled,
    /// Registered but not running.
    Stopped,
    /// Registered and running.
    Running,
    /// A start has been requested and is in progress.
    StartPending,
    /// A stop has been requested and is in progress.
    StopPending,
    /// Registered, in some other transient/paused state.
    Other,
}

/// Read the service's current state straight from the SCM.
pub fn status() -> Result<SvcState, String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => {
            let status = service.query_status().map_err(friendly)?;
            Ok(map_state(status.current_state))
        }
        Err(e) if is_not_found(&e) => Ok(SvcState::NotInstalled),
        Err(e) => Err(friendly(e)),
    }
}

/// Register the service: this executable plus `--service`, account LocalSystem,
/// auto-start at boot. Idempotent — a pre-existing service is treated as success
/// so the UI reconciles to "installed" rather than surfacing a spurious error.
pub fn install(exe: std::path::PathBuf) -> Result<(), String> {
    let manager = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // None => LocalSystem
        account_password: None,
    };

    match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG) {
        Ok(service) => {
            // Best-effort: a missing description doesn't make the install a failure.
            let _ = service.set_description(DESCRIPTION);
        }
        Err(e) if is_code(&e, ERROR_SERVICE_EXISTS) => {
            // Reinstall over a pre-existing (possibly stale-named, e.g. a prior "AllTheThings
            // Index Service") registration: refresh its config so Services/Task Manager show
            // today's name instead of whatever it was registered under originally.
            if let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG) {
                let _ = service.change_config(&info);
                let _ = service.set_description(DESCRIPTION);
            }
        }
        Err(e) => return Err(friendly(e)),
    }
    // Best-effort, also for an existing install: restart after a crash or a kill (2 s, 5 s, then 30 s; the count
    // resets after a day), and let signed-in users start it (RP for IU), so the GUI can bring it back without an
    // admin prompt. Users still can't stop, reconfigure or delete it.
    let sc = |args: &[&str]| {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("sc.exe").args(args).creation_flags(0x0800_0000).status();
    };
    sc(&["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/2000/restart/5000/restart/30000"]);
    sc(&["sdset", SERVICE_NAME, "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)"]);
    Ok(())
}

/// Stop (if running) and delete the service. Idempotent — an already-absent
/// service is success.
pub fn uninstall() -> Result<(), String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(e) if is_not_found(&e) => return Ok(()),
        Err(e) => return Err(friendly(e)),
    };

    // Best-effort stop first so the binary isn't left running after deletion.
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            wait_for_stop(&service);
        }
    }
    service.delete().map_err(friendly)
}

/// Start the service. A service already running (or starting) is success.
pub fn start() -> Result<(), String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(friendly)?;

    if let Ok(status) = service.query_status() {
        if matches!(
            status.current_state,
            ServiceState::Running | ServiceState::StartPending
        ) {
            return Ok(());
        }
    }
    let no_args: [&OsStr; 0] = [];
    service.start(&no_args).map_err(friendly)
}

/// Stop the service and wait (briefly) until it has. A service already stopped is success.
pub fn stop() -> Result<(), String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(friendly)?;

    if let Ok(status) = service.query_status() {
        match status.current_state {
            ServiceState::Stopped => return Ok(()),
            ServiceState::StopPending => {}
            _ => {
                service.stop().map_err(friendly)?;
            }
        }
    }
    wait_for_stop(&service);
    Ok(())
}

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
    ServiceManager::local_computer(None::<&str>, access).map_err(friendly)
}

/// Poll briefly for the service to reach `Stopped` so a following `delete`
/// removes the binary registration cleanly rather than marking it pending.
fn wait_for_stop(service: &windows_service::service::Service) {
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        match service.query_status() {
            Ok(status) if status.current_state == ServiceState::Stopped => return,
            Ok(_) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
}

fn map_state(state: ServiceState) -> SvcState {
    match state {
        ServiceState::Stopped => SvcState::Stopped,
        ServiceState::Running => SvcState::Running,
        ServiceState::StartPending => SvcState::StartPending,
        ServiceState::StopPending => SvcState::StopPending,
        _ => SvcState::Other,
    }
}

fn is_not_found(e: &windows_service::Error) -> bool {
    is_code(e, ERROR_SERVICE_DOES_NOT_EXIST)
}

fn is_code(e: &windows_service::Error, code: i32) -> bool {
    matches!(e, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(code))
}

/// Turn SCM errors into actionable messages: friendly text for access-denied,
/// and the real OS code + message for other Win32 failures. (windows-service's
/// own `Display` for the `Winapi` variant is just the constant "IO error in
/// winapi call", which is undiagnosable in the Settings UI.)
fn friendly(e: windows_service::Error) -> String {
    if is_code(&e, ERROR_ACCESS_DENIED) {
        return "administrator rights are required to manage the service".into();
    }
    match e {
        windows_service::Error::Winapi(io) => format!(
            "service operation failed (error {}): {io}",
            io.raw_os_error().unwrap_or(0)
        ),
        other => other.to_string(),
    }
}
