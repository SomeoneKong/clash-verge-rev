//! What Linux and macOS share: the configuration kept from the authorized install and the service
//! host behind the Unix socket.

use crate::daemon::Daemon;
use crate::logger;
use anyhow::{Context as _, Result};
use clash_verge_updater::paths::{Paths, socket_path};
use clash_verge_updater::payload::sha256_file;
use clash_verge_updater::state::Initiator;
use clash_verge_updater::sys;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// The group allowed to use the socket and the service; kept from the authorized install.
    pub service_gid: Option<u32>,
}

pub fn config(paths: &Paths) -> Config {
    std::fs::read(paths.config_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn run_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).env("LC_ALL", "C").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn sha(path: &Path) -> Option<String> {
    sha256_file(path).ok()
}

pub fn group_id(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let entry = unsafe { libc::getgrnam(name.as_ptr()) };
    (!entry.is_null()).then(|| unsafe { (*entry).gr_gid })
}

/// The group named by the installing environment: the service group passed down, else the group
/// of whoever authorized the install.
pub fn invoking_gid() -> Option<u32> {
    let env = |name: &str| std::env::var(name).ok().and_then(|value| value.parse::<u32>().ok());
    let user_gid = |uid: u32| {
        let entry = unsafe { libc::getpwuid(uid) };
        (!entry.is_null()).then(|| unsafe { (*entry).pw_gid })
    };
    env("CLASH_VERGE_SERVICE_GID")
        .or_else(|| env("SUDO_UID").or_else(|| env("PKEXEC_UID")).and_then(user_gid))
        .or_else(|| env("SUDO_GID"))
}

pub fn run_service() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let daemon = Daemon::open()?;
        daemon.start().await;
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        logger::log(&format!("service started, version {}", env!("CARGO_PKG_VERSION")));
        tokio::select! {
            served = serve(daemon) => served,
            _ = terminate.recv() => Ok(()),
        }
    })
}

async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    use std::os::unix::fs::chown;
    let socket = socket_path();
    let dir = socket.parent().context("socket has no directory")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let gid = config(&Paths::system()).service_gid;
    chown(&socket, Some(0), gid)?;
    std::fs::set_permissions(
        &socket,
        std::fs::Permissions::from_mode(if gid.is_some() { 0o660 } else { 0o600 }),
    )?;
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(credentials) = stream.peer_cred() else {
            continue;
        };
        let uid = credentials.uid();
        let initiator = Initiator {
            user: sys::user_name(uid),
            id: uid.to_string(),
            pid: credentials
                .pid()
                .and_then(|pid| u32::try_from(pid).ok())
                .unwrap_or_default(),
            session: None,
        };
        tokio::spawn(Arc::clone(&daemon).handle(stream, initiator));
    }
}
