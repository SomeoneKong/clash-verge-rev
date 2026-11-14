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
use std::time::{Duration, Instant};
use tauri_plugin_updater::Update;
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

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

/// The component's view, or `None` where it cannot be reached.
async fn status() -> Option<Hello> {
    let mut connection = Connection::connect().await.ok()?;
    hello(&mut connection).await.ok()
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
    crate::feat::settle_core_upgrade_at_startup().await;
    let Some(status) = status().await else {
        return;
    };
    if let Some(txn) = &status.current {
        if txn.kind == Kind::Core {
            return;
        }
        // The bundle phase of this user's update interrupted here is finished or given up first;
        // the app starts once that update has ended.
        #[cfg(target_os = "macos")]
        if macos::own_user_phase(txn) && macos::resume(txn).await {
            return;
        }
        logging!(
            warn,
            Type::System,
            "Update v{} is being applied, not starting",
            txn.version
        );
        std::process::exit(0);
    }
    let Some(pending) = status.needs_repair else {
        return;
    };
    if !repairable_here(&pending) || repair_attempted(&pending.version) {
        logging!(warn, Type::System, "Update v{} still needs repair", pending.version);
        return;
    }
    logging!(
        warn,
        Type::System,
        "Update v{} needs repair, repairing",
        pending.version
    );
    let repaired = match take_gate() {
        Ok(_gate) => repair(&pending, None, true).await,
        Err(error) => Err(error),
    };
    match repaired {
        // A repaired bundle phase leaves this app as it is: it is that version already.
        Ok(Outcome::Applied) if pending.kind == Kind::Bundle => {}
        Ok(Outcome::Applied) => crate::feat::restart_after_update().await,
        Ok(_) => {}
        Err(error) => {
            logging!(error, Type::System, "Repairing v{} failed: {error:#}", pending.version);
            // A repair whose end is not known may still run.
            if update_in_progress() {
                std::process::exit(0);
            }
        }
    }
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

/// Asks the app that starts after an update the user began from the window to show its window,
/// even with silent start. Withdrawn when dropped; the app leaving for the update keeps it.
pub struct ShowAfterUpdate(Option<std::path::PathBuf>);

impl ShowAfterUpdate {
    pub fn ask() -> Self {
        let marker = show_marker().filter(|marker| std::fs::write(marker, "").is_ok());
        Self(marker)
    }

    /// Keeps the request for the restart that follows.
    pub fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for ShowAfterUpdate {
    fn drop(&mut self) {
        if let Some(marker) = self.0.take() {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// Runs once startup is done. A request older than an hour is dropped: the restart it was for
/// did not happen, and this start may be an unattended one.
pub async fn show_after_update() {
    let Some(marker) = show_marker() else {
        return;
    };
    let fresh = std::fs::metadata(&marker)
        .and_then(|meta| meta.modified())
        .is_ok_and(|time| time.elapsed().is_ok_and(|age| age < Duration::from_secs(60 * 60)));
    if std::fs::remove_file(&marker).is_err() || !fresh {
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

fn choose(update: &Update) -> Option<PayloadEntry> {
    let platform = Platform::current()?;
    let payloads: PlatformPayloads =
        serde_json::from_value(update.raw_json["fork"]["payloads"][platform.key()].clone()).ok()?;
    match platform {
        Platform::WindowsX86_64 => {
            let same_layout = installed_fingerprint().is_some_and(|installed| payloads.fingerprint == Some(installed));
            if same_layout { payloads.patch } else { payloads.full }
        }
        Platform::LinuxX86_64 | Platform::LinuxAarch64 => payloads.deb,
        Platform::DarwinAarch64 => payloads.bundle,
    }
}

/// On macOS only the app of the version awaiting repair can repair it: the bundle is the user's,
/// and a repair redoes only the privileged part.
fn repairable_here(pending: &Finished) -> bool {
    pending.kind != Kind::Bundle || pending.version == env!("CARGO_PKG_VERSION")
}

async fn report_fallback(version: &str, reason: &str) {
    if let Ok(mut connection) = Connection::connect().await {
        let claim = serde_json::json!({ "event": "fallback", "version": version, "reason": reason });
        let _ = connection.request(&Request::Report { claim }).await;
    }
}

/// The app's own updater may take over only when nothing is being applied.
fn fallback_unless_running(error: &anyhow::Error) -> Result<Outcome> {
    if update_in_progress() {
        bail!("an update is already in progress");
    }
    Ok(Outcome::Fallback(format!("{error:#}")))
}

/// The caller holds the gate (`take_gate`) for this and for any fallback that follows.
pub async fn install(
    update: &Update,
    on_event: impl Fn(DownloadEvent) + Sync,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let mut connection = match Connection::connect().await {
        Ok(connection) => connection,
        // macOS has no installer to set the component up; this update offers it.
        #[cfg(target_os = "macos")]
        Err(_) if choose(update).is_some() && !update_in_progress() && macos::bootstrap().await => {
            match Connection::connect().await {
                Ok(connection) => connection,
                Err(error) => return fallback_unless_running(&error),
            }
        }
        Err(error) => return fallback_unless_running(&error),
    };
    let status = match hello(&mut connection).await {
        Ok(status) => status,
        Err(error) => return fallback_unless_running(&error),
    };
    drop(connection);
    if status.current.is_some() {
        bail!("an update is already in progress");
    }
    // The version that failed is repaired first; a newer one is offered again afterwards.
    if let Some(pending) = status.needs_repair.filter(repairable_here) {
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        logging!(warn, Type::System, "Repairing v{} before updating", pending.version);
        return repair(&pending, Some(cancel), false).await;
    }

    let Some(entry) = choose(update) else {
        let reason = "the release carries no payload for the update component";
        report_fallback(&update.version, reason).await;
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
    anyhow::ensure!(digest == entry.sha256, "the payload does not match its digest");
    if cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }

    let mut connection = Connection::connect().await?;
    let trigger = Trigger {
        kind: "manual".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    #[cfg(target_os = "macos")]
    let signatures = entry.signatures.clone();
    let staged = match connection.stage(trigger, &bytes, entry.signatures).await? {
        Response::Staged(staged) => staged,
        Response::Rejected { reason } => {
            logging!(
                warn,
                Type::System,
                "The update component refused v{}: {reason}",
                update.version
            );
            report_fallback(&update.version, &reason).await;
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

    // The installer replaces the app, so the app leaves first and the component starts it again.
    let relaunch = staged.kind == Kind::Full;
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
            report_fallback(&update.version, &reason).await;
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
    let answer = connection.request(&request).await;
    drop(connection);
    let txn = match answer {
        Ok(Response::Txn(TxnView::Running(txn))) => txn.id,
        Ok(Response::Busy { .. }) => bail!("an update is already in progress"),
        Ok(Response::Rejected { reason } | Response::Error { message: reason }) => bail!("{reason}"),
        Ok(other) => bail!("unexpected answer {other:?}"),
        Err(error) => match repairing(&pending.version).await {
            Some(Repairing::Running(txn)) => txn,
            Some(Repairing::Done) => {
                if remember {
                    remember_repair(&pending.version);
                }
                return Ok(Outcome::Applied);
            }
            None => return Err(error),
        },
    };
    // Noted once the repair has started, before the app may leave for its installer.
    if remember {
        remember_repair(&pending.version);
    }
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
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        anyhow::ensure!(Instant::now() < deadline, "the update did not finish in time");
        match view(txn).await {
            None | Some(TxnView::Staged(_) | TxnView::Running(_)) => {}
            Some(TxnView::Finished(finished)) => return outcome_of(finished),
            Some(TxnView::Unknown) => bail!("the update component lost track of the update"),
        }
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
