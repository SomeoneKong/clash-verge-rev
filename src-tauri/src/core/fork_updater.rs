//! Updates through the privileged update component when it is installed: the payload is downloaded
//! here, through the user's own proxy, and handed over; the component verifies and applies it
//! without asking for elevation. Without the component, the Tauri updater path is used instead.

use super::updater::{DownloadEvent, download};
use anyhow::{Context as _, Result, anyhow, bail};
use clash_verge_logging::{Type, logging};
use clash_verge_updater::client::{self, Connection};
use clash_verge_updater::manifest::{Kind, Platform};
use clash_verge_updater::protocol::{Hello, PROTOCOL, Request, Response, Staged, Trigger, TxnView};
use clash_verge_updater::state::{Finished, Outcome as TxnOutcome};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri_plugin_updater::Update;
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

pub(crate) mod automatic;
pub(crate) mod interface;

pub enum Outcome {
    /// Installed; the caller restarts into the new version.
    Applied,
    Cancelled,
    /// The component cannot take this update and changed nothing; the Tauri updater path applies.
    Fallback(String),
}

#[derive(Deserialize)]
struct PayloadEntry {
    url: String,
    sha256: String,
    signatures: Vec<String>,
}

#[derive(Deserialize)]
struct PlatformPayloads {
    fingerprint: Option<String>,
    patch: Option<PayloadEntry>,
    full: Option<PayloadEntry>,
    deb: Option<PayloadEntry>,
    bundle: Option<PayloadEntry>,
}

/// App updates, core upgrades and service changes in this process take turns.
static GATE: Mutex<()> = Mutex::const_new(());

/// Held by whoever installs an app update, through the component or the app's own updater.
pub(crate) fn take_gate() -> Result<MutexGuard<'static, ()>> {
    GATE.try_lock()
        .map_err(|_| anyhow!("an app update, core upgrade or service change is in progress"))
}

/// Waited for by the confirmation of a core upgrade a previous app left, which must not overlap
/// an app update either.
pub(crate) async fn wait_gate() -> MutexGuard<'static, ()> {
    GATE.lock().await
}

/// Held by core upgrades and service changes for their whole run: refused while an app update
/// runs, here or in the component.
pub fn exclusive() -> Result<MutexGuard<'static, ()>> {
    let gate = take_gate()?;
    anyhow::ensure!(!update_in_progress(), "an app update is in progress");
    Ok(gate)
}

/// An update applied by the component is running, even if the component cannot be reached now. A
/// state that cannot be read counts as one.
pub fn update_in_progress() -> bool {
    client::read_state().map_or(true, |state| state.txn.is_some())
}

async fn hello(connection: &mut Connection) -> Result<Hello> {
    match connection.request(&Request::Hello { protocol: PROTOCOL }).await? {
        Response::Hello(hello) if hello.protocol == PROTOCOL => Ok(*hello),
        Response::Hello(_) => bail!("the update component speaks another protocol version"),
        other => bail!("unexpected answer {other:?}"),
    }
}

async fn connect_component() -> Result<(Connection, Hello)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut connection = Connection::connect().await?;
        let status = hello(&mut connection).await?;
        Ok((connection, status))
    })
    .await
    .context("the update component timed out")?
}

async fn component_hello() -> Result<Hello> {
    Ok(connect_component().await?.1)
}

async fn status() -> Option<Hello> {
    component_hello().await.ok()
}

/// Whether updates go through the component on this machine.
pub async fn available() -> bool {
    status().await.is_some()
}

/// What the component reports, for core upgrades; `None` where it cannot be reached.
pub(crate) async fn component_status() -> Option<Hello> {
    status().await
}

/// The component refused a core upgrade before it changed anything, so another way may take it.
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the update component refused the core upgrade: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Hands a core payload over; the component verifies it before it answers.
pub(crate) async fn stage_core(payload: &[u8], signatures: Vec<String>) -> Result<Staged> {
    let mut connection = Connection::connect().await?;
    let trigger = Trigger {
        kind: "core".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    match connection.stage(trigger, payload, signatures).await? {
        Response::Staged(staged) => Ok(staged),
        Response::Rejected { reason } => Err(Refused(reason).into()),
        Response::Busy { .. } => bail!("an update is already in progress"),
        other => bail!("unexpected answer {other:?}"),
    }
}

/// Starts a staged core upgrade. An answer lost on the way is looked up; a refusal means nothing
/// was started.
pub(crate) async fn apply_core(txn: &str) -> Result<TxnView> {
    let answer = match Connection::connect().await {
        Ok(mut connection) => {
            connection
                .request(&Request::Apply {
                    txn: txn.into(),
                    relaunch: false,
                })
                .await
        }
        Err(error) => Err(error),
    };
    match answer {
        Ok(Response::Txn(view)) => Ok(view),
        Ok(Response::Rejected { reason }) => Err(Refused(reason).into()),
        Ok(Response::Error { message }) => bail!("{message}"),
        Ok(Response::Busy { .. }) => bail!("an update is already in progress"),
        Ok(other) => bail!("unexpected answer {other:?}"),
        Err(error) => applied_anyway(txn).await.ok_or(error),
    }
}

/// How a core upgrade stands, or `None` where the component cannot be reached.
pub(crate) async fn core_view(txn: &str) -> Option<TxnView> {
    view(txn).await
}

/// Follows a core upgrade until `reached` accepts how it stands, reconnecting meanwhile.
pub(crate) async fn wait_core(txn: &str, reached: impl Fn(&TxnView) -> bool) -> Result<TxnView> {
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    loop {
        match view(txn).await {
            Some(TxnView::Unknown) => bail!("the update component lost track of the core upgrade"),
            Some(view) if reached(&view) => return Ok(view),
            _ => {}
        }
        anyhow::ensure!(Instant::now() < deadline, "the core upgrade did not finish in time");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Tells the component whether the core a core upgrade put in place runs. The first answer
/// decides; one lost on the way is sent again, and is then answered with how the upgrade stands.
pub(crate) async fn confirm_core(txn: &str, ok: bool) -> Result<TxnView> {
    let request = Request::Confirm { txn: txn.into(), ok };
    let mut last = None;
    for _ in 0..10 {
        let answer = match Connection::connect().await {
            Ok(mut connection) => connection.request(&request).await,
            Err(error) => Err(error),
        };
        match answer {
            Ok(Response::Txn(view)) => return Ok(view),
            Ok(Response::Rejected { reason } | Response::Error { message: reason }) => bail!("{reason}"),
            Ok(other) => bail!("unexpected answer {other:?}"),
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(last.unwrap_or_else(|| anyhow!("the update component did not answer")))
}

/// Runs first at startup, before the core is touched. While the component applies an update the
/// app does not start; after an installer the component starts it. A version left needing repair
/// is repaired once, without asking: the user chose that update already.
pub async fn resolve_at_startup() {
    // A core upgrade still changing files is waited for, and one left to this app to confirm is
    // confirmed once the core runs.
    let attempted = automatic::settle_previous().await.unwrap_or_else(|error| {
        logging!(warn, Type::System, "Settling the automatic update: {error:#}");
        true
    });
    let (handled, initial) = resolve_current(status().await, false).await;
    let status = if handled { initial } else { status().await };
    let installed = status
        .as_ref()
        .and_then(|h| h.installed_version.as_deref())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    if !attempted && let Err(error) = automatic::installed(installed) {
        logging!(warn, Type::System, "Clearing the installed candidate: {error:#}");
    }
    if let Some(hello) = status {
        if hello.current.is_some() {
            if !handled {
                resolve_current(Some(hello), false).await;
            }
            return;
        }
        if hello.staged || resolve_existing(&hello, repair_attempted).await {
            return;
        }
    }
    if !attempted
        && !handled
        && let Err(error) = automatic::startup().await
    {
        logging!(warn, Type::System, "Automatic update: {error:#}");
    }
}

async fn resolve_current(initial: Option<Hello>, require_idle: bool) -> (bool, Option<Hello>) {
    let (core, status) = crate::feat::settle_core_upgrade_at_startup(initial, status, require_idle).await;
    if let Some(txn) = status
        .as_ref()
        .and_then(|h| h.current.as_ref())
        .filter(|t| t.kind != Kind::Core)
    {
        // The bundle phase of this user's update interrupted here is finished or given up first;
        // the app starts once that update has ended.
        #[cfg(target_os = "macos")]
        if macos::own_user_phase(txn) && macos::resume(txn).await {
            return (true, status);
        }
        leave_for_app_update(&txn.version);
    }
    (core, status)
}

fn leave_for_app_update(version: &str) -> ! {
    logging!(warn, Type::System, "Update v{version} is being applied, not starting");
    std::process::exit(0)
}

fn repair_busy_status(previous: &Hello, latest: Option<Hello>, current: TxnView) -> Option<Hello> {
    latest.or_else(|| {
        let TxnView::Running(txn) = current else {
            return None;
        };
        let mut observed = previous.clone();
        observed.current = Some(txn);
        Some(observed)
    })
}

async fn resolve_active_update(id: &str, kind: Kind, version: &str) -> clash_verge_updater::automatic::Settlement {
    if kind == Kind::Full {
        leave_for_app_update(version);
    }
    automatic::track_transaction(id).await
}

async fn resolve_existing(status: &Hello, attempted: fn(&str) -> bool) -> bool {
    let Some(pending) = &status.needs_repair else {
        return false;
    };
    if !repairable_here(pending) || attempted(&pending.version) {
        logging!(warn, Type::System, "Update v{} still needs repair", pending.version);
        return false;
    }
    logging!(
        warn,
        Type::System,
        "Update v{} needs repair, repairing",
        pending.version
    );
    let repaired = match take_gate() {
        Ok(_gate) => repair(pending, None, true).await,
        Err(error) => Err(error),
    };
    match repaired {
        // A repaired bundle phase leaves this app as it is: it is that version already.
        Ok(Outcome::Applied) if pending.kind == Kind::Bundle => {}
        Ok(Outcome::Applied) => crate::feat::restart_after_update().await,
        Ok(_) => {}
        Err(error) => match error.downcast::<RepairBusy>() {
            Ok(RepairBusy(current)) => {
                let latest = repair_busy_status(status, component_status().await, current);
                resolve_current(latest, true).await;
            }
            Err(error) => {
                logging!(error, Type::System, "Repairing v{} failed: {error:#}", pending.version);
                // A repair whose end is not known may still run.
                if update_in_progress() {
                    std::process::exit(0);
                }
            }
        },
    }
    true
}

fn repair_marker() -> Option<std::path::PathBuf> {
    crate::utils::dirs::app_home_dir()
        .ok()
        .map(|dir| dir.join("fork-updater-repair"))
}

/// The version whose repair was asked for at startup, so a repair that keeps failing is not
/// retried on every start (a full one would restart the app each time).
fn repair_attempted(version: &str) -> bool {
    repair_marker().is_none_or(|marker| std::fs::read_to_string(marker).is_ok_and(|done| done.trim() == version))
}

fn remember_repair(version: &str) {
    if let Some(marker) = repair_marker() {
        let _ = std::fs::write(marker, version);
    }
}

fn show_marker() -> Option<std::path::PathBuf> {
    crate::utils::dirs::app_home_dir()
        .ok()
        .map(|dir| dir.join("fork-updater-show"))
}

fn hide_marker() -> Option<std::path::PathBuf> {
    crate::utils::dirs::app_home_dir()
        .ok()
        .map(|dir| dir.join("fork-updater-hide"))
}

/// Asks the app that starts after an update to show its window, even with silent start, when the
/// user began the update from the window; or to keep it hidden, even without silent start, when
/// nobody did. Withdrawn when dropped; the app leaving for the update keeps it.
pub struct WindowAfterUpdate(Option<std::path::PathBuf>);

impl WindowAfterUpdate {
    pub fn show() -> Self {
        Self::ask(show_marker(), hide_marker())
    }

    fn hide() -> Self {
        Self::ask(hide_marker(), show_marker())
    }

    pub(super) fn ask(marker: Option<std::path::PathBuf>, other: Option<std::path::PathBuf>) -> Self {
        if let Some(other) = other {
            let _ = std::fs::remove_file(other);
        }
        Self(marker.filter(|marker| std::fs::write(marker, "").is_ok()))
    }

    /// Keeps the request for the restart that follows.
    pub fn keep(mut self) {
        if let Some(marker) = self.0.take() {
            let _ = std::fs::write(marker, "");
        }
    }
}

impl Drop for WindowAfterUpdate {
    fn drop(&mut self) {
        if let Some(marker) = self.0.take() {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// Takes a request left for this start. One older than an hour is dropped: the restart it was
/// for did not happen, and this start may be another one.
fn take_request(marker: Option<std::path::PathBuf>) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    let fresh = std::fs::metadata(&marker)
        .and_then(|meta| meta.modified())
        .is_ok_and(|time| time.elapsed().is_ok_and(|age| age < Duration::from_secs(60 * 60)));
    std::fs::remove_file(&marker).is_ok() && fresh
}

/// Whether this start keeps the window hidden as with silent start, having been restarted by an
/// unattended update. Decided once: the request is taken by the first to ask, which callers do
/// before reading the silent start setting so that it is taken either way.
pub fn start_hidden() -> bool {
    static HIDDEN: OnceLock<bool> = OnceLock::new();
    *HIDDEN.get_or_init(|| take_request(hide_marker()))
}

/// Runs once startup is done.
pub async fn show_after_update() {
    if !take_request(show_marker()) {
        return;
    }
    logging!(info, Type::Window, "Showing the window after an update");
    if !super::lightweight::exit_lightweight_mode().await {
        crate::utils::window_manager::WindowManager::show_main_window().await;
    }
}

/// The layout this installation was made with, to tell whether a patch applies.
fn installed_fingerprint() -> Option<String> {
    let exe = tauri::utils::platform::current_exe().ok()?;
    let text = std::fs::read_to_string(exe.parent()?.join("resources").join("install-fingerprint")).ok()?;
    Some(text.trim().to_owned())
}

fn choose(raw_json: &serde_json::Value) -> Option<(Kind, PayloadEntry)> {
    checked_payload(raw_json).ok().flatten()
}

fn release_is_test(raw: &serde_json::Value) -> Result<bool> {
    let Some(fork) = raw.get("fork") else { return Ok(false) };
    let fork = fork.as_object().context("invalid fork metadata")?;
    fork.get("test")
        .map(|v| v.as_bool().context("invalid fork.test"))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn checked_payload(raw: &serde_json::Value) -> Result<Option<(Kind, PayloadEntry)>> {
    let Some(platform) = Platform::current() else {
        return Ok(None);
    };
    let Some(fork) = raw.get("fork") else { return Ok(None) };
    let fork = fork.as_object().context("invalid fork metadata")?;
    let Some(payloads) = fork.get("payloads") else {
        return Ok(None);
    };
    let Some(entry) = payloads
        .as_object()
        .context("invalid fork.payloads")?
        .get(platform.key())
    else {
        return Ok(None);
    };
    anyhow::ensure!(entry.is_object(), "invalid platform payloads");
    let payloads: PlatformPayloads = serde_json::from_value(entry.clone())?;
    Ok(match platform {
        Platform::WindowsX86_64 => {
            let same_layout = installed_fingerprint().is_some_and(|installed| payloads.fingerprint == Some(installed));
            if same_layout && let Some(patch) = payloads.patch {
                Some((Kind::Patch, patch))
            } else {
                payloads.full.map(|p| (Kind::Full, p))
            }
        }
        Platform::LinuxX86_64 | Platform::LinuxAarch64 => payloads.deb.map(|p| (Kind::Deb, p)),
        Platform::DarwinAarch64 => payloads.bundle.map(|p| (Kind::Bundle, p)),
    })
}

/// On macOS only the app of the version awaiting repair can repair it: the bundle is the user's,
/// and a repair redoes only the privileged part.
fn repairable_here(pending: &Finished) -> bool {
    pending.kind != Kind::Bundle || pending.version == env!("CARGO_PKG_VERSION")
}

pub(super) async fn report_fallback(version: &str, reason: &str) {
    if let Ok(mut connection) = Connection::connect().await {
        let claim = serde_json::json!({ "event": "fallback", "version": version, "reason": reason });
        let _ = connection.request(&Request::Report { claim }).await;
    }
}

/// The app's own updater may take over only when nothing is being applied.
fn fallback_unless_running(error: &anyhow::Error) -> Result<Outcome> {
    let state = client::read_state().context("unable to confirm the update component's state")?;
    anyhow::ensure!(state.txn.is_none(), "an update is already in progress");
    Ok(Outcome::Fallback(format!("{error:#}")))
}

/// Asks the running app, or this one once started, to update unattended.
pub const UNATTENDED_FLAG: &str = "--update-unattended";

pub fn unattended_requested() -> bool {
    std::env::args_os().skip(1).any(|arg| arg == UNATTENDED_FLAG)
}

pub(crate) fn restart_updated_binary() -> ! {
    use crate::core::handle::Handle;
    use tauri::Manager as _;
    Handle::global().set_is_exiting();
    let app = Handle::app_handle();
    if unattended_requested() {
        let mut env = app.env();
        remove_unattended_arg(&mut env.args_os);
        app.cleanup_before_exit();
        tauri::process::restart(&env);
    }
    app.restart()
}

fn remove_unattended_arg(args: &mut Vec<std::ffi::OsString>) {
    args.retain(|arg| arg != UNATTENDED_FLAG);
}

#[cfg(test)]
#[test]
fn update_restart_preserves_other_arguments() {
    use std::ffi::OsString;
    let mut args = ["clash-verge", "--update-unattended", "a path", "--update-unattended"]
        .map(OsString::from)
        .to_vec();
    remove_unattended_arg(&mut args);
    assert_eq!(args, ["clash-verge", "a path"].map(OsString::from));
}

static UNATTENDED_READY: parking_lot::Mutex<bool> = parking_lot::const_mutex(false);

fn app_local_dir() -> Result<std::path::PathBuf> {
    ::dirs::data_local_dir()
        .map(|dir| dir.join(crate::utils::dirs::APP_ID))
        .ok_or_else(|| anyhow::anyhow!("Local application data directory is unavailable"))
}

fn queued_command() -> Result<std::path::PathBuf> {
    Ok(app_local_dir()?.join("update-unattended.pending"))
}

pub(crate) fn forward_unattended() -> Result<()> {
    if !queue_during_startup(&UNATTENDED_READY, &queued_command()?)? {
        crate::process::AsyncHandler::spawn(update_unattended);
    }
    Ok(())
}

fn queue_during_startup(ready: &parking_lot::Mutex<bool>, path: &std::path::Path) -> Result<bool> {
    let ready = ready.lock();
    if *ready {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().context("no command directory")?)?;
    clash_verge_updater::state::write_atomic(path, b"")?;
    drop(ready);
    Ok(true)
}

fn take_queued_command(ready: &parking_lot::Mutex<bool>, path: &std::path::Path) -> Result<bool> {
    let mut ready = ready.lock();
    let result = std::fs::remove_file(path);
    *ready = result.is_err();
    drop(ready);
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn run_startup_commands() {
    if unattended_requested() && automatic::take_startup_command() {
        update_unattended().await;
    }
    loop {
        match queued_command().and_then(|p| take_queued_command(&UNATTENDED_READY, &p)) {
            Ok(true) => update_unattended().await,
            Ok(false) => break,
            Err(error) => {
                logging!(warn, Type::System, "Reading the queued update command: {error:#}");
                break;
            }
        }
    }
}

/// Updates to the version offered, if any, without asking anyone and without showing the window
/// before or after. Only through the update component: the installer the app falls back to
/// otherwise asks for elevation. On macOS, replacing a non-writable bundle still needs authorization.
pub async fn update_unattended() {
    if let Err(error) = try_update_unattended().await {
        logging!(warn, Type::System, "Unattended update: {error:#}");
    }
}

async fn try_update_unattended() -> Result<()> {
    let gate = take_gate()?;
    if !available().await {
        bail!("the update component is not available");
    }
    let Some(update) =
        super::updater::SilentUpdater::check(super::handle::Handle::app_handle(), &CancellationToken::new()).await?
    else {
        logging!(info, Type::System, "Unattended update: no update available");
        return Ok(());
    };
    if automatic::skip_test(&update)? {
        logging!(
            info,
            Type::System,
            "Unattended update: skipping test release v{}",
            update.version
        );
        return Ok(());
    }
    logging!(info, Type::System, "Unattended update: installing v{}", update.version);
    match install(&update, |_| {}, &CancellationToken::new(), TriggerKind::Unattended).await? {
        Outcome::Applied => {
            WindowAfterUpdate::hide().keep();
            drop(gate);
            crate::feat::restart_after_update().await;
        }
        Outcome::Cancelled => {}
        Outcome::Fallback(reason) => {
            logging!(
                warn,
                Type::System,
                "Unattended update: v{} not installed: {reason}",
                update.version
            );
        }
    }
    Ok(())
}

/// Who asked for an update, as the component records it.
#[derive(Clone, Copy)]
pub enum TriggerKind {
    Manual,
    Unattended,
}

impl TriggerKind {
    fn window(self) -> WindowAfterUpdate {
        match self {
            Self::Manual => WindowAfterUpdate::show(),
            Self::Unattended => WindowAfterUpdate::hide(),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Unattended => "auto",
        }
    }
}

fn repair_before_install(status: &Hello, trigger: TriggerKind) -> Option<&Finished> {
    status
        .needs_repair
        .as_ref()
        .filter(|p| matches!(trigger, TriggerKind::Manual) && repairable_here(p))
}

fn install_busy(status: &Hello, trigger: TriggerKind) -> bool {
    let can_restage = matches!(trigger, TriggerKind::Manual)
        && status.staged_by_caller
        && repair_before_install(status, trigger).is_none();
    status.current.is_some() || (status.staged && !can_restage)
}

/// The caller holds the gate (`take_gate`) for this and for any fallback that follows.
pub async fn install(
    update: &Update,
    on_event: impl Fn(DownloadEvent) + Sync,
    cancel: &CancellationToken,
    trigger: TriggerKind,
) -> Result<Outcome> {
    let status = match component_hello().await {
        Ok(status) => status,
        // macOS has no installer to set the component up; this update offers it.
        #[cfg(target_os = "macos")]
        Err(_)
            if matches!(trigger, TriggerKind::Manual)
                && interface::bootstrap_available()
                && choose(&update.raw_json).is_some()
                && !update_in_progress()
                && macos::bootstrap().await =>
        {
            match component_hello().await {
                Ok(status) => status,
                Err(error) => return fallback_unless_running(&error),
            }
        }
        Err(error) => return fallback_unless_running(&error),
    };
    if install_busy(&status, trigger) {
        bail!("an update is already in progress");
    }
    if let Some(pending) = repair_before_install(&status, trigger) {
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        logging!(warn, Type::System, "Repairing v{} before updating", pending.version);
        return repair(pending, Some(cancel), false).await;
    }

    let Some((kind, entry)) = choose(&update.raw_json) else {
        let reason = "the release carries no payload for the update component";
        return Ok(Outcome::Fallback(reason.into()));
    };
    let mut source = update.clone();
    source.download_url = entry.url.parse().context("invalid payload URL")?;
    source.signature = entry.signatures.first().cloned().context("the payload is not signed")?;
    logging!(
        info,
        Type::System,
        "Downloading the update payload for v{}",
        update.version
    );
    let bytes = tokio::select! {
        bytes = download(&source, on_event) => bytes?,
        () = cancel.cancelled() => return Ok(Outcome::Cancelled),
    };
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    anyhow::ensure!(
        digest.eq_ignore_ascii_case(&entry.sha256),
        "the payload does not match its digest"
    );
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }

    let mut connection = Connection::connect().await?;
    let stage_trigger = Trigger {
        kind: trigger.as_str().into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    #[cfg(target_os = "macos")]
    let signatures = entry.signatures.clone();
    let staged = match connection.stage(stage_trigger, &bytes, entry.signatures).await? {
        Response::Staged(staged) => staged,
        Response::Rejected { reason } => {
            logging!(
                warn,
                Type::System,
                "The update component refused v{}: {reason}",
                update.version
            );
            return Ok(Outcome::Fallback(reason));
        }
        Response::Busy { .. } => bail!("an update is already in progress"),
        other => bail!("unexpected answer {other:?}"),
    };
    logging!(
        info,
        Type::System,
        "Update v{} staged as {:?} ({})",
        staged.version,
        staged.kind,
        staged.txn
    );
    if cancel.is_cancelled() {
        let _ = connection
            .request(&Request::Cancel {
                txn: staged.txn.clone(),
            })
            .await;
        return Ok(Outcome::Cancelled);
    }

    if !staged.matches_target(&update.version, kind, &digest) {
        let _ = connection.request(&Request::Cancel { txn: staged.txn }).await;
        bail!("the staged payload does not match the offered update");
    }

    // The installer replaces the app, so the app leaves first and the component starts it again.
    let relaunch = staged.kind == Kind::Full;
    let _window = trigger.window();
    let applied = connection
        .request(&Request::Apply {
            txn: staged.txn.clone(),
            relaunch,
        })
        .await;
    drop(connection);
    match applied {
        Ok(Response::Txn(TxnView::Running(_))) => {}
        Ok(Response::Rejected { reason }) => {
            logging!(
                warn,
                Type::System,
                "The update component refused v{}: {reason}",
                update.version
            );
            return Ok(Outcome::Fallback(reason));
        }
        Ok(Response::Busy { .. }) => bail!("an update is already in progress"),
        Ok(Response::Error { message }) => bail!("{message}"),
        Ok(other) => bail!("unexpected answer {other:?}"),
        // The answer was lost on the way; the update may have started all the same.
        Err(error) => match applied_anyway(&staged.txn).await {
            Some(TxnView::Running(_)) => {}
            Some(TxnView::Finished(finished)) => return outcome_of(finished),
            _ => return Err(error),
        },
    }
    #[cfg(target_os = "macos")]
    if staged.kind == Kind::Bundle {
        macos::replace_bundle(update, bytes, signatures, &staged.txn).await?;
    }
    follow(&staged.txn, relaunch).await
}

/// How a transaction whose apply answer was lost stands, if it started after all: running, or
/// already finished. The component may be restarting meanwhile, so it is asked for a while.
async fn applied_anyway(txn: &str) -> Option<TxnView> {
    for _ in 0..30 {
        match view(txn).await {
            Some(view @ (TxnView::Running(_) | TxnView::Finished(_))) => return Some(view),
            Some(TxnView::Staged(_) | TxnView::Unknown) => return None,
            None => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    None
}

fn outcome_of(finished: Finished) -> Result<Outcome> {
    match finished.outcome {
        TxnOutcome::Applied => Ok(Outcome::Applied),
        outcome => bail!("the update ended {outcome:?}: {}", finished.error.unwrap_or_default()),
    }
}

enum Repairing {
    Running(String),
    Done,
}

/// The repair of `version` a lost answer left unknown: the component runs it as a new transaction,
/// found as the current one, or it already ended and that version is installed.
async fn repairing(version: &str) -> Option<Repairing> {
    for _ in 0..30 {
        let Some(status) = status().await else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        return match status.current {
            // Only a repair this app asked for; one started by someone else is theirs to follow.
            Some(txn) if txn.repair && txn.version == version && txn.initiator.pid == std::process::id() => {
                Some(Repairing::Running(txn.id))
            }
            Some(_) => None,
            None if status.needs_repair.is_none() && status.installed_version.as_deref() == Some(version) => {
                Some(Repairing::Done)
            }
            None => None,
        };
    }
    None
}

#[derive(Debug)]
struct RepairBusy(TxnView);

impl std::fmt::Display for RepairBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an update is already in progress")
    }
}

impl std::error::Error for RepairBusy {}

fn repair_answer(answer: Response, remember: impl FnOnce()) -> Result<String> {
    let txn = match answer {
        Response::Txn(TxnView::Running(txn)) => Ok(txn.id),
        Response::Rejected { reason } | Response::Error { message: reason } => Err(anyhow!(reason)),
        Response::Busy { current } => return Err(RepairBusy(current).into()),
        other => bail!("unexpected answer {other:?}"),
    };
    remember();
    txn
}

/// Re-applies a version that needs repair from the payload the component kept.
async fn repair(pending: &Finished, cancel: Option<&CancellationToken>, remember: bool) -> Result<Outcome> {
    let relaunch = pending.kind == Kind::Full;
    let mut connection = Connection::connect().await?;
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Ok(Outcome::Cancelled);
    }
    let request = Request::Repair {
        txn: Some(pending.id.clone()),
        relaunch,
    };
    let _window = (relaunch && !remember).then(WindowAfterUpdate::show);
    let answer = connection.request(&request).await;
    drop(connection);
    let remember = || {
        if remember {
            remember_repair(&pending.version);
        }
    };
    let txn = match answer {
        Ok(response) => repair_answer(response, remember)?,
        Err(error) => {
            let progress = repairing(&pending.version).await.ok_or(error)?;
            remember();
            match progress {
                Repairing::Running(txn) => txn,
                Repairing::Done => return Ok(Outcome::Applied),
            }
        }
    };
    // This app is the bundle of that version already; only the privileged part is redone.
    #[cfg(target_os = "macos")]
    if pending.kind == Kind::Bundle {
        macos::confirm_replaced(&txn).await?;
    }
    follow(&txn, relaunch).await
}

/// Leaves for an installer the component runs, or waits for the transaction to end.
async fn follow(txn: &str, relaunch: bool) -> Result<Outcome> {
    if relaunch {
        crate::feat::exit_for_update().await;
    }
    wait_finished(txn).await
}

async fn view(txn: &str) -> Option<TxnView> {
    let mut connection = Connection::connect().await.ok()?;
    match connection.request(&Request::Status { txn: txn.into() }).await.ok()? {
        Response::Txn(view) => Some(view),
        _ => None,
    }
}

/// Follows the transaction to its end, reconnecting when the component restarts itself.
async fn wait_finished(txn: &str) -> Result<Outcome> {
    use clash_verge_updater::automatic::Settlement;
    match automatic::track_transaction(txn).await {
        Settlement::Applied => Ok(Outcome::Applied),
        Settlement::Failed(reason) | Settlement::Rejected(reason) | Settlement::RolledBack(reason) => bail!("{reason}"),
        outcome => bail!("the update ended without confirmed success: {outcome:?}"),
    }
}

/// macOS: the app bundle is the user's, so this app replaces it itself between the component's
/// two privileged steps, and sets the component up the first time.
#[cfg(target_os = "macos")]
mod macos {
    use super::{Connection, wait_finished};
    use anyhow::{Context as _, Result, bail};
    use clash_verge_logging::{Type, logging};
    use clash_verge_updater::manifest::Role;
    use clash_verge_updater::payload::Payload;
    use clash_verge_updater::protocol::{Request, Response, TxnView, UserPhaseEvent};
    use clash_verge_updater::state::{Phase, Txn};
    use std::time::Duration;
    use tauri_plugin_updater::Update;

    /// A transaction of this user waiting on its bundle phase.
    pub fn own_user_phase(txn: &Txn) -> bool {
        matches!(txn.phase, Phase::UserPhasePending | Phase::UserPhaseBegun)
            && txn.initiator.id == unsafe { libc::getuid() }.to_string()
    }

    fn shell_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', r"'\''"))
    }

    fn applescript_quote(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', r"\\").replace('"', "\\\""))
    }

    /// Sets the component up with one administrator authorization, from the copy this app carries.
    /// False when declined or failed; the update then goes the app's own way.
    pub async fn bootstrap() -> bool {
        let installed = tokio::task::spawn_blocking(|| -> Result<bool> {
            let exe = tauri::utils::platform::current_exe()?;
            let program = exe
                .parent()
                .and_then(std::path::Path::parent)
                .context("unexpected app layout")?
                .join("Resources/resources/clash-verge-updater");
            anyhow::ensure!(program.is_file(), "the app carries no update component");
            let gid = tauri_plugin_clash_verge_sysinfo::current_gid();
            let shell = format!(
                "cd /; CLASH_VERGE_SERVICE_GID={gid} {} install",
                shell_quote(&program.to_string_lossy())
            );
            let prompt = clash_verge_i18n::t!("forkUpdater.componentInstallPrompt");
            let script = format!(
                "do shell script {} with administrator privileges with prompt {}",
                applescript_quote(&shell),
                applescript_quote(&prompt)
            );
            let output = std::process::Command::new("osascript").args(["-e", &script]).output()?;
            if output.status.success() {
                return Ok(true);
            }
            let error = String::from_utf8_lossy(&output.stderr);
            // -128: the user cancelled the authorization.
            anyhow::ensure!(error.contains("(-128)"), "{}", error.trim());
            Ok(false)
        })
        .await;
        match installed {
            Ok(Ok(installed)) => {
                logging!(info, Type::System, "Update component installed: {installed}");
                installed
            }
            Ok(Err(error)) => {
                logging!(warn, Type::System, "Installing the update component failed: {error:#}");
                false
            }
            Err(error) => {
                logging!(warn, Type::System, "Installing the update component failed: {error}");
                false
            }
        }
    }

    /// Tells the component how the bundle phase went, retrying while it restarts.
    pub async fn report(txn: &str, event: UserPhaseEvent) -> Result<()> {
        let request = Request::UserPhase { txn: txn.into(), event };
        let mut last = None;
        for _ in 0..5 {
            let answer = match Connection::connect().await {
                Ok(mut connection) => connection.request(&request).await,
                Err(error) => Err(error),
            };
            match answer {
                Ok(Response::Done) => return Ok(()),
                Ok(Response::Rejected { reason } | Response::Error { message: reason }) => bail!("{reason}"),
                Ok(other) => bail!("unexpected answer {other:?}"),
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("the update component did not answer")))
    }

    enum BundleError {
        Install(tauri_plugin_updater::Error),
        Other(anyhow::Error),
    }

    /// Replaces this app's bundle with the one in the payload, through the app's own updater,
    /// which checks the bundle's signature again. The component publishes the rest once told.
    pub async fn replace_bundle(update: &Update, bytes: Vec<u8>, signatures: Vec<String>, txn: &str) -> Result<()> {
        report(txn, UserPhaseEvent::Begin).await?;
        let mut update = update.clone();
        let installed = tokio::task::spawn_blocking(move || -> Result<(), BundleError> {
            let payload = Payload::open(bytes, &signatures).map_err(BundleError::Other)?;
            let manifest = &payload.manifest;
            let (Some(bundle), Some(meta)) = (manifest.files_with(Role::Bundle).next(), manifest.bundle.as_ref())
            else {
                return Err(BundleError::Other(anyhow::anyhow!("the payload carries no bundle")));
            };
            let archive = payload.read(&bundle.path).map_err(BundleError::Other)?;
            update.signature.clone_from(&meta.signature);
            update.install(archive).map_err(BundleError::Install)
        })
        .await
        .unwrap_or_else(|error| Err(BundleError::Other(error.into())));
        let event = match &installed {
            Ok(()) => UserPhaseEvent::Replaced,
            Err(BundleError::Install(tauri_plugin_updater::Error::PreviousAppNotRestored(_))) => {
                UserPhaseEvent::NotRestored
            }
            Err(_) => UserPhaseEvent::NotReplaced,
        };
        let error = match installed {
            Ok(()) => return confirm_replaced(txn).await,
            Err(BundleError::Install(error)) => anyhow::Error::new(error),
            Err(BundleError::Other(error)) => error,
        };
        if let Err(reported) = report(txn, event).await {
            logging!(warn, Type::System, "Reporting the bundle phase failed: {reported:#}");
        }
        logging!(warn, Type::System, "Replacing the app failed ({event:?}): {error:#}");
        Err(error.context("the app was not replaced"))
    }

    /// Tells the component the bundle is replaced. An answer lost on the way still counts once the
    /// privileged phase runs, or has ended: it is then followed like any other.
    pub async fn confirm_replaced(txn: &str) -> Result<()> {
        let Err(error) = report(txn, UserPhaseEvent::Replaced).await else {
            return Ok(());
        };
        match super::view(txn).await {
            Some(TxnView::Running(running)) if running.phase == Phase::Applying => Ok(()),
            Some(TxnView::Finished(_)) => Ok(()),
            _ => Err(error),
        }
    }

    /// Finishes the bundle phase of an update this user started and this app did not see through:
    /// this app being that version means the bundle was replaced; otherwise the update is given
    /// up, having changed nothing as root. True once the update has ended, however it ended.
    pub async fn resume(txn: &Txn) -> bool {
        let replaced = txn.version == env!("CARGO_PKG_VERSION");
        logging!(
            info,
            Type::System,
            "Finishing the bundle phase of v{} (replaced: {replaced})",
            txn.version
        );
        let finished = if replaced {
            match confirm_replaced(&txn.id).await {
                Ok(()) => wait_finished(&txn.id).await.map(drop),
                Err(error) => Err(error),
            }
        } else {
            report(&txn.id, UserPhaseEvent::NotReplaced).await
        };
        if let Err(error) = finished {
            logging!(
                warn,
                Type::System,
                "Finishing the update v{} failed: {error:#}",
                txn.version
            );
        }
        matches!(super::view(&txn.id).await, Some(TxnView::Finished(_)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod startup_tests {
    use super::*;

    #[test]
    fn a_completed_update_refreshes_its_window_request_and_abandoned_requests_expire() {
        use std::fs::{File, create_dir, remove_dir};
        let dir = std::env::temp_dir().join(format!("verge-update-window-{}", nanoid::nanoid!()));
        create_dir(&dir).unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(61 * 60);
        for (marker, opposite) in [("hide", "show"), ("show", "hide")] {
            let marker = dir.join(marker);
            let opposite = dir.join(opposite);
            File::create(&opposite).unwrap();
            let request = WindowAfterUpdate::ask(Some(marker.clone()), Some(opposite.clone()));
            assert!(!opposite.exists());
            File::options()
                .write(true)
                .open(&marker)
                .unwrap()
                .set_modified(stale)
                .unwrap();
            request.keep();
            assert!(take_request(Some(marker.clone())));
            assert!(!take_request(Some(marker.clone())));
            File::create(&marker).unwrap().set_modified(stale).unwrap();
            assert!(!take_request(Some(marker)));
        }
        remove_dir(dir).unwrap();
    }

    #[test]
    fn forwarded_command_survives_a_startup_restart_and_is_taken_once() {
        let dir = std::env::temp_dir().join(format!("verge-update-command-{}", nanoid::nanoid!()));
        let path = dir.join("pending");
        let starting = parking_lot::Mutex::new(false);
        assert!(queue_during_startup(&starting, &path).unwrap());
        let restarted = parking_lot::Mutex::new(false);
        assert!(take_queued_command(&restarted, &path).unwrap());
        assert!(!take_queued_command(&restarted, &path).unwrap());
        assert!(!queue_during_startup(&restarted, &path).unwrap());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_repair_conflict_requires_machine_state_when_the_component_stops_answering() {
        let previous: Hello = serde_json::from_value(serde_json::json!({
            "protocol": 1, "updater_version":"1", "platform":"linux-x86_64"
        }))
        .unwrap();
        let staged = TxnView::Staged(Staged {
            txn: "conflict".into(),
            version: "2.0.0".into(),
            kind: Kind::Core,
            size: 1,
            sha256: "hash".into(),
        });
        assert!(repair_busy_status(&previous, None, staged.clone()).is_none());
        let mut latest = previous.clone();
        latest.staged = true;
        assert!(repair_busy_status(&previous, Some(latest), staged).unwrap().staged);
    }

    #[tokio::test]
    async fn an_attempted_repair_allows_only_a_higher_automatic_candidate() {
        let h: Hello = serde_json::from_value(serde_json::json!({
            "protocol": 1, "updater_version":"1", "platform":"linux-x86_64",
            "needs_repair": {"id":"a","version":"2.0.0","kind":"deb","outcome":"needs_repair","finished_at":"now",
                "initiator":{"user":"u","id":"u","pid":1}}
        }))
        .unwrap();
        let remembered = std::cell::Cell::new(false);
        for (kind, phase) in [("core", "applying"), ("core", "published"), ("full", "applying")] {
            let current = serde_json::from_value(serde_json::json!({
                "state":"running","id":"conflict","version":"2.0.0","kind":kind,"phase":phase,
                "initiator":{"id":"user","pid":1,"user":"user"},"trigger":{"kind":"manual","client_version":"1"},
                "started_at":"now","payload_sha256":"hash","payload_size":1,"key_id":"key","relaunch":false
            }))
            .unwrap();
            let busy = repair_answer(Response::Busy { current }, || remembered.set(true))
                .unwrap_err()
                .downcast::<RepairBusy>()
                .unwrap();
            let txn = repair_busy_status(&h, None, busy.0).unwrap().current.unwrap();
            assert_eq!(serde_json::to_value(txn.kind).unwrap(), kind);
            assert_eq!(serde_json::to_value(txn.phase).unwrap(), phase);
        }
        assert!(!remembered.get());
        assert!(
            repair_answer(
                Response::Error {
                    message: "missing repair payload".into()
                },
                || remembered.set(true)
            )
            .is_err()
        );
        assert!(remembered.get());
        assert!(repair_before_install(&h, TriggerKind::Unattended).is_none());
        assert_eq!(repair_before_install(&h, TriggerKind::Manual).unwrap().version, "2.0.0");
        assert!(!resolve_existing(&h, |_| true).await);
        assert!(!automatic::newer_than_pending("2.0.0", h.needs_repair.as_ref()));
        assert!(!automatic::newer_than_pending("1.9.0", h.needs_repair.as_ref()));
        assert!(automatic::newer_than_pending("2.1.0", h.needs_repair.as_ref()));
    }
}
