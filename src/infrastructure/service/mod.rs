//! Local IPC and SCM lifecycle for the optional background service: a
//! query-only named-pipe server (the LocalSystem [`host`]) and client, plus
//! [`scm`] install/uninstall/start/stop/status. The GUI talks to it through
//! `presentation::backend`.

pub mod client;
pub mod framing;
pub mod host;
pub mod pipe;
pub mod scm;
mod security;
