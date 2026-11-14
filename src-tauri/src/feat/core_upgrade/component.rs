//! Core upgrades through the update component. It publishes a signed core payload, without
//! elevation, to the service's approved copies and, on Windows and Linux, beside the app; this app
//! then runs the new core and tells the component whether it runs, and the component puts the
//! previous cores back if not. On macOS the bundle is the user's, so this app matches the bundle's
//! cores to the service's itself.

use super::{
    CoreUpgradeReport, MAX_PACKAGE_BYTES, PACKAGE_TIMEOUT_SECS, RELEASE_DOWNLOAD_URL, VERSION_TIMEOUT_SECS,
    asset_base_name, managed_core_path, read_core_version, resolve_latest_version,
};
use crate::core::{
    CoreManager, fork_updater,
    handle::Handle,
    manager::{ConfigOwnership, ConfigUpdateGuard, RunningMode, runtime_is_fallback},
    runstate::RUN_STATE,
    service::SERVICE_MANAGER,
};
use crate::process::AsyncHandler;
use crate::utils::network::{NetworkManager, ProxyType};
use anyhow::{Context as _, Result, bail, ensure};
use clash_verge_logging::{Type, logging};
use clash_verge_updater::manifest::{Kind, Platform};
use clash_verge_updater::payload::Payload;
use clash_verge_updater::protocol::{ApprovedCore, CAPABILITY_CORE, Hello, TxnView};
use clash_verge_updater::state::{Finished, Outcome, Phase, Txn};
use sha2::{Digest as _, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::MutexGuard;

/// The lock app updates, core upgrades and service changes take turns on.
type Gate = MutexGuard<'static, ()>;

const CORE_NAMES: [&str; 2] = ["verge-mihomo", "verge-mihomo-alpha"];
/// How long a running configuration update is waited for before the upgrade takes over.
const CONFIG_UPDATE_WAIT: Duration = Duration::from_secs(60);
/// How long a restarted core has to run its version.
const RUN_CHECK: Duration = Duration::from_secs(15);
/// How long startup, before any window, waits for a core upgrade that still changes files.
const STARTUP_WAIT: Duration = Duration::from_secs(2 * 60);
const MAX_SIGNATURE_BYTES: usize = 64 * 1024;

/// The component's view when it upgrades the cores here; `None` leaves them to the app's own way.
async fn route() -> Option<Hello> {
    let hello = fork_updater::component_status().await?;
    let upgrades_cores = hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_CORE);
    // Without the service, macOS has only the user's bundle to change, which needs no elevation.
    let needed = cfg!(not(target_os = "macos")) || hello.service_installed;
    (upgrades_cores && needed).then_some(hello)
}

/// Whether the component refused the upgrade before changing anything, so the app's own way may
/// take it.
fn refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<fork_updater::Refused>())
}

/// The upgrade through the component, or `None` where the component does not take it. `gate` is
/// taken over when the upgrade's end is unknown, to be held until it ends.
pub(super) async fn try_upgrade(
    gate: &mut Option<Gate>,
    alpha: bool,
    force: bool,
) -> Option<Result<CoreUpgradeReport>> {
    let hello = route().await?;
    match upgrade(gate, hello, alpha, force).await {
        Err(error) if refused(&error) => {
            logging!(
                warn,
                Type::Core,
                "core upgrade: {error:#}; upgrading without the update component"
            );
            None
        }
        upgraded => Some(upgraded),
    }
}

// The ownership spans the whole upgrade and is handed on to its end, never dropped early.
#[allow(clippy::significant_drop_tightening)]
async fn upgrade(gate: &mut Option<Gate>, hello: Hello, alpha: bool, force: bool) -> Result<CoreUpgradeReport> {
    let (proxy, latest) = resolve_latest_version(alpha).await?;
    if !force && up_to_date(&hello, &latest).await {
        return Ok(CoreUpgradeReport { upgraded: false });
    }
    let payload = download(proxy, &latest).await?;
    #[cfg(target_os = "macos")]
    bundle::keep(&payload).await?;
    let ownership = CoreManager::global().own_config_waiting(CONFIG_UPDATE_WAIT).await?;
    let staged = fork_updater::stage_core(&payload.bytes, payload.signatures.clone()).await?;
    logging!(info, Type::Core, "core upgrade: {latest} staged as {}", staged.txn);
    let service = hello.service_installed;
    let mut restarted = false;
    let driven = match fork_updater::apply_core(&staged.txn).await {
        Ok(_) => drive(&ownership.update, &staged.txn, service, &latest, &mut restarted).await,
        Err(error) if refused(&error) => return Err(error),
        // The upgrade may have started all the same.
        Err(error) => Err(error),
    };
    let upgrade = Upgrade {
        txn: staged.txn,
        service,
        target: latest,
        restarted,
    };
    conclude(ownership, gate.take(), upgrade, driven).await?;
    Ok(CoreUpgradeReport { upgraded: true })
}

/// A started core upgrade, as the app follows it.
struct Upgrade {
    txn: std::string::String,
    service: bool,
    target: std::string::String,
    /// The core was started anew into the new one.
    restarted: bool,
}

/// Nothing to do only when the core runs that version as it should, both cores beside the app are
/// it, the service's copies are the same files, and no core upgrade awaits repair.
async fn up_to_date(hello: &Hello, latest: &str) -> bool {
    if hello.core_needs_repair.is_some() || !runs(latest, hello.service_installed, Duration::ZERO).await {
        return false;
    }
    let approved = hello.approved_cores.clone();
    let service = hello.service_installed;
    let latest = latest.to_owned();
    tokio::task::spawn_blocking(move || {
        CORE_NAMES.iter().all(|name| {
            let Ok(path) = managed_core_path(name) else {
                return false;
            };
            read_core_version(&path).is_ok_and(|version| version == latest)
                && (!service
                    || approved_sha(&approved, name).is_some_and(|sha| file_sha(&path).as_deref() == Some(sha)))
        })
    })
    .await
    .unwrap_or(false)
}

struct Downloaded {
    bytes: Vec<u8>,
    signatures: Vec<std::string::String>,
    /// The digest of the core inside.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    core: std::string::String,
}

async fn download(proxy: ProxyType, version: &str) -> Result<Downloaded> {
    let url = format!(
        "{RELEASE_DOWNLOAD_URL}/{version}/{}-{version}.core.zip",
        asset_base_name(false)?
    );
    logging!(info, Type::Core, "core upgrade: downloading {url}");
    let bytes = NetworkManager::new()
        .get_bytes(&url, proxy, Some(PACKAGE_TIMEOUT_SECS), MAX_PACKAGE_BYTES)
        .await
        .with_context(|| format!("failed to download {url}"))?;
    let signature = NetworkManager::new()
        .get_bytes(
            &format!("{url}.sig"),
            proxy,
            Some(VERSION_TIMEOUT_SECS),
            MAX_SIGNATURE_BYTES,
        )
        .await
        .with_context(|| format!("failed to download the signature of {url}"))?;
    // One signature per line.
    let signatures: Vec<std::string::String> = std::string::String::from_utf8(signature)
        .context("the signature is not text")?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let target = version.to_owned();
    // Checked as the component will check it: one it refuses is not worth handing over.
    tokio::task::spawn_blocking(move || {
        let payload = Payload::open(bytes, &signatures)?;
        let manifest = &payload.manifest;
        ensure!(
            manifest.kind == Kind::Core && Platform::current() == Some(manifest.platform) && manifest.version == target,
            "the core payload is not the one asked for"
        );
        let core = manifest
            .files
            .first()
            .context("the core payload holds no core")?
            .sha256
            .clone();
        Ok(Downloaded {
            bytes: payload.bytes().to_vec(),
            signatures,
            core,
        })
    })
    .await?
}

const fn finished(view: &TxnView) -> bool {
    matches!(view, TxnView::Finished(_))
}

fn published(view: &TxnView) -> bool {
    matches!(view, TxnView::Running(txn) if txn.phase == Phase::Published)
}

async fn finished_view(txn: &str, view: TxnView) -> Result<Finished> {
    let view = if finished(&view) {
        view
    } else {
        fork_updater::wait_core(txn, finished).await?
    };
    match view {
        TxnView::Finished(done) => Ok(done),
        other => bail!("unexpected state {other:?}"),
    }
}

/// Runs a started core upgrade until it ends: once published, the bundle is matched (macOS), the
/// core is started anew, and whether it runs is told to the component.
async fn drive(
    update: &ConfigUpdateGuard<'_>,
    txn: &str,
    service: bool,
    target: &str,
    restarted: &mut bool,
) -> Result<Finished> {
    let view = fork_updater::wait_core(txn, |view| finished(view) || published(view)).await?;
    if let TxnView::Finished(done) = view {
        return Ok(done);
    }
    align_bundle().await;
    *restarted = true;
    let ok = restart_into(update, service, target).await;
    logging!(info, Type::Core, "core upgrade: {target} runs as it should: {ok}");
    let view = fork_updater::confirm_core(txn, ok).await?;
    finished_view(txn, view).await
}

/// Finishes the app's side once the upgrade has ended. While its end is unknown the component may
/// still be writing: nothing is touched, and the configuration and the gate stay owned by a task
/// that waits for the end, however long, and then finishes it.
async fn conclude(
    ownership: ConfigOwnership,
    gate: Option<Gate>,
    upgrade: Upgrade,
    driven: Result<Finished>,
) -> Result<()> {
    let done = match driven {
        Ok(done) => done,
        Err(error) => match fork_updater::wait_core(&upgrade.txn, finished).await {
            Ok(TxnView::Finished(done)) => {
                logging!(warn, Type::Core, "core upgrade: {error:#}");
                done
            }
            _ => {
                see_out(ownership, gate, upgrade);
                return Err(error.context("the core upgrade has not ended; the core is left alone until it does"));
            }
        },
    };
    settle(&ownership.update, &upgrade, done).await
}

fn see_out(ownership: ConfigOwnership, gate: Option<Gate>, upgrade: Upgrade) {
    AsyncHandler::spawn(move || async move {
        let done = loop {
            match fork_updater::core_view(&upgrade.txn).await {
                Some(TxnView::Finished(done)) => break Some(done),
                // The component no longer knows it: nothing of it writes any more.
                Some(TxnView::Unknown) => break None,
                _ => tokio::time::sleep(Duration::from_secs(5)).await,
            }
        };
        if let Some(done) = done
            && let Err(error) = settle(&ownership.update, &upgrade, done).await
        {
            logging!(warn, Type::Core, "core upgrade {}: {error:#}", upgrade.txn);
        }
        drop(ownership);
        drop(gate);
    });
}

/// The app's side of an ended upgrade: the bundle is matched (macOS) and the core now in place
/// runs.
async fn settle(update: &ConfigUpdateGuard<'_>, upgrade: &Upgrade, done: Finished) -> Result<()> {
    let (service, target, restarted) = (upgrade.service, upgrade.target.as_str(), upgrade.restarted);
    align_bundle().await;
    if done.outcome == Outcome::Applied {
        ensure!(
            runs(target, service, Duration::ZERO).await || restart_into(update, service, target).await,
            "the core {target} is in place but does not run as it should"
        );
        return Ok(());
    }
    // The previous cores are back where the component could put them; one restarted into the new
    // core runs them again.
    if restarted && let (_, Err(error)) = restart_again(update, service).await {
        logging!(
            warn,
            Type::Core,
            "core upgrade: restarting the previous core: {error:#}"
        );
    }
    bail!(
        "the core upgrade to {target} ended {:?}{}",
        done.outcome,
        done.error.map(|error| format!(": {error}")).unwrap_or_default()
    )
}

/// Starts the core afresh as startup decides it: with a runtime the core beside the app accepts,
/// without this session's fallback to Sidecar and with the service probed again where it is
/// installed. The caller owns the configuration update. Returns whether a core started, with how
/// the restart went.
async fn restart_again(update: &ConfigUpdateGuard<'_>, service: bool) -> (bool, Result<()>) {
    let manager = CoreManager::global();
    manager
        .restart_with_checked_runtime(update, || async move {
            if service {
                SERVICE_MANAGER.withdraw_sidecar_allowance();
                RUN_STATE.observe_current_health().await;
            }
            manager.restart_core_during_config_update().await
        })
        .await
}

/// Restarts into `target` and tells whether it runs as it should. A step after the core started,
/// such as setting the system proxy, may fail on its own; what the core does is what counts, and a
/// restart that failed before it stopped the previous core proves nothing of the new one.
async fn restart_into(update: &ConfigUpdateGuard<'_>, service: bool, target: &str) -> bool {
    let (started, restarted) = restart_again(update, service).await;
    if let Err(error) = restarted {
        logging!(warn, Type::Core, "core upgrade: restarting into {target}: {error:#}");
    }
    started && runs(target, service, RUN_CHECK).await
}

/// Whether the core runs `target` within `within` with the full runtime: by the service where it
/// is installed, over a session the service answers on (a Sidecar taking over does not count).
async fn runs(target: &str, service: bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let mode = *CoreManager::global().get_running_mode();
        let owner_ok = if service {
            matches!(mode, RunningMode::Service)
                && crate::core::service::active_service_session().is_ok()
                && SERVICE_MANAGER.confirm_ready().await.is_ok()
        } else {
            !matches!(mode, RunningMode::NotRunning)
        };
        if owner_ok && !runtime_is_fallback().await && running_version().await.as_deref() == Some(target) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn running_version() -> Option<std::string::String> {
    match tokio::time::timeout(Duration::from_secs(3), Handle::mihomo().get_version()).await {
        Ok(Ok(version)) => Some(version.version),
        _ => None,
    }
}

fn approved_sha<'a>(approved: &'a [ApprovedCore], name: &str) -> Option<&'a str> {
    approved
        .iter()
        .find(|core| core.name == name)
        .and_then(|core| core.sha256.as_deref())
}

fn sha_of(bytes: &[u8]) -> std::string::String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_sha(path: &Path) -> Option<std::string::String> {
    std::fs::read(path).ok().map(|bytes| sha_of(&bytes))
}

/// A transaction of the user running this app.
fn own(txn: &Txn) -> bool {
    use clash_verge_service_ipc::OwnerIdentity;
    match crate::core::owner_identity::current_owner_identity() {
        Ok(OwnerIdentity::Unix { uid, .. }) => txn.initiator.id == uid.to_string(),
        Ok(OwnerIdentity::Windows { sid }) => txn.initiator.id == sid,
        Err(_) => false,
    }
}

/// A published core upgrade of this user that awaits its answer: nothing writes meanwhile.
fn awaiting_own_answer(txn: &Txn) -> bool {
    txn.kind == Kind::Core && txn.phase == Phase::Published && txn.decision.is_none() && own(txn)
}

/// The core upgrade a previous app published but did not see through, answered once the core has
/// started.
struct Resume {
    txn: std::string::String,
    version: std::string::String,
    service: bool,
}

static RESUME: std::sync::Mutex<Option<Resume>> = std::sync::Mutex::new(None);

/// Runs first at startup. A core upgrade that still changes files is waited for before the core is
/// touched; one of this user that awaits its answer is answered once the core has started. On
/// macOS the bundle's cores are matched to the service's before anything reads them.
pub async fn settle_core_upgrade_at_startup() {
    let deadline = Instant::now() + STARTUP_WAIT;
    let mut told = false;
    loop {
        match fork_updater::component_status().await {
            Some(hello) => match hello.current.clone().filter(|txn| txn.kind == Kind::Core) {
                None => {
                    align_bundle_with(hello).await;
                    return;
                }
                Some(txn) if awaiting_own_answer(&txn) => {
                    *RESUME.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Resume {
                        txn: txn.id,
                        version: txn.version,
                        service: hello.service_installed,
                    });
                    align_bundle_with(hello).await;
                    return;
                }
                Some(_) => {}
            },
            // A component that does not answer may have left a worker writing; its state tells.
            None => match clash_verge_updater::client::read_state() {
                Ok(state) if state.txn.as_ref().is_some_and(|txn| txn.kind == Kind::Core) => {}
                Ok(_) => return,
                Err(error) => {
                    logging!(warn, Type::System, "Reading the update state failed: {error:#}");
                    return;
                }
            },
        }
        if !told {
            logging!(
                info,
                Type::System,
                "A core upgrade is under way; the core starts once it settles"
            );
            told = true;
        }
        if Instant::now() >= deadline {
            logging!(
                warn,
                Type::System,
                "The core upgrade did not settle in time; starting anyway"
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Runs once the core has started: answers the core upgrade a previous app left published.
pub async fn confirm_core_upgrade_after_startup() {
    let Some(resume) = RESUME.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() else {
        return;
    };
    // Held to the end, so no app update runs before the bundle and the core are settled.
    let gate = fork_updater::wait_gate().await;
    let ownership = match CoreManager::global().own_config_waiting(CONFIG_UPDATE_WAIT).await {
        Ok(ownership) => ownership,
        Err(error) => {
            logging!(warn, Type::Core, "core upgrade {}: {error:#}", resume.txn);
            return;
        }
    };
    let ok = runs(&resume.version, resume.service, RUN_CHECK).await;
    logging!(
        info,
        Type::Core,
        "core upgrade {}: {} runs as it should: {ok}",
        resume.txn,
        resume.version
    );
    let driven = match fork_updater::confirm_core(&resume.txn, ok).await {
        Ok(view) => finished_view(&resume.txn, view).await,
        Err(error) => Err(error),
    };
    let upgrade = Upgrade {
        txn: resume.txn,
        service: resume.service,
        target: resume.version,
        // The core started at startup is the new one already.
        restarted: true,
    };
    let txn = upgrade.txn.clone();
    match conclude(ownership, Some(gate), upgrade, driven).await {
        Ok(()) => logging!(info, Type::Core, "core upgrade {txn}: kept"),
        Err(error) => logging!(warn, Type::Core, "core upgrade {txn}: {error:#}"),
    }
}

async fn align_bundle() {
    if let Some(hello) = fork_updater::component_status().await {
        align_bundle_with(hello).await;
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)]
async fn align_bundle_with(_hello: Hello) {}

#[cfg(target_os = "macos")]
async fn align_bundle_with(hello: Hello) {
    match tokio::task::spawn_blocking(move || bundle::align(&hello)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => logging!(warn, Type::Core, "core upgrade: matching the bundle failed: {error:#}"),
        Err(error) => logging!(warn, Type::Core, "core upgrade: matching the bundle failed: {error}"),
    }
}

/// macOS: the bundle's cores are made the service's approved ones, only from what is known to
/// hold the same bytes: a kept payload, or what the bundle had before. Both are kept by the
/// digest of the core, so a later upgrade never replaces what an earlier one may still need.
#[cfg(target_os = "macos")]
mod bundle {
    use super::{
        CORE_NAMES, Downloaded, Hello, Payload, approved_sha, awaiting_own_answer, file_sha, managed_core_path, sha_of,
    };
    use anyhow::{Context as _, Result};
    use clash_verge_logging::{Type, logging};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    fn kept_dir() -> Result<PathBuf> {
        Ok(crate::utils::dirs::app_home_dir()?.join("core-upgrade"))
    }

    fn sync_dir(dir: &Path) -> Result<()> {
        std::fs::File::open(dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("failed to flush {}", dir.display()))
    }

    /// Creates `dir` below the kept directory, durably: it and the kept directory are entered in
    /// their parents.
    fn make_dir(dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut parent = dir.parent();
        for _ in 0..2 {
            if let Some(dir) = parent {
                sync_dir(dir)?;
                parent = dir.parent();
            }
        }
        Ok(())
    }

    /// Writes `bytes` to `path` so that a crash leaves either nothing or the whole file.
    fn write_atomic(path: &Path, bytes: &[u8], modified: Option<SystemTime>) -> Result<()> {
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        {
            let mut file =
                std::fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
            file.write_all(bytes)?;
            if let Some(modified) = modified {
                file.set_modified(modified)?;
            }
            file.sync_all()?;
        }
        std::fs::rename(&temp, path).with_context(|| format!("failed to write {}", path.display()))?;
        match path.parent() {
            Some(dir) => sync_dir(dir),
            None => Ok(()),
        }
    }

    pub(super) async fn keep(payload: &Downloaded) -> Result<()> {
        let dir = kept_dir()?.join("payloads");
        let bytes = payload.bytes.clone();
        let signatures = serde_json::to_vec(&payload.signatures)?;
        let core = payload.core.clone();
        tokio::task::spawn_blocking(move || {
            make_dir(&dir)?;
            write_atomic(&dir.join(format!("{core}.json")), &signatures, None)?;
            write_atomic(&dir.join(format!("{core}.zip")), &bytes, None)
        })
        .await?
    }

    /// The bytes with digest `want` and the time they carry, from what is kept.
    fn source(dir: &Path, want: &str) -> Result<Option<(Vec<u8>, SystemTime)>> {
        let payloads = dir.join("payloads");
        if let Ok(signatures) = std::fs::read(payloads.join(format!("{want}.json")))
            && let Ok(bytes) = std::fs::read(payloads.join(format!("{want}.zip")))
        {
            let signatures: Vec<std::string::String> = serde_json::from_slice(&signatures)?;
            let payload = Payload::open(bytes, &signatures)?;
            let file = payload
                .manifest
                .files
                .first()
                .context("the kept payload holds no core")?;
            if file.sha256 == want {
                let built =
                    chrono::DateTime::parse_from_rfc3339(&payload.manifest.built_at).context("invalid build time")?;
                return Ok(Some((payload.read(&file.path)?, built.into())));
            }
        }
        let backup = dir.join("backups").join(want);
        if let Ok(bytes) = std::fs::read(&backup)
            && sha_of(&bytes) == want
        {
            let modified = std::fs::metadata(&backup)?.modified()?;
            return Ok(Some((bytes, modified)));
        }
        Ok(None)
    }

    /// Keeps the bundle's core with digest `sha`, and its time, before it is replaced.
    fn back_up(dir: &Path, target: &Path, sha: &str) -> Result<()> {
        let backups = dir.join("backups");
        let kept = backups.join(sha);
        let payload = dir.join("payloads").join(format!("{sha}.zip"));
        if kept.exists() || payload.exists() {
            return Ok(());
        }
        make_dir(&backups)?;
        let modified = std::fs::metadata(target)?.modified()?;
        write_atomic(&kept, &std::fs::read(target)?, Some(modified))
    }

    /// Replaces `target` with `bytes` in one rename from beside it.
    fn write_core(target: &Path, bytes: &[u8], modified: SystemTime) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = target.parent().context("the core has no directory")?;
        let name = target.file_name().context("the core has no file name")?;
        let (path, mut file) = super::super::create_staging_file(directory, name)?;
        let written = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
            file.set_modified(modified)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&path, target).with_context(|| format!("failed to replace {}", target.display()))?;
            // What is kept is let go only once the bundle is durably the service's.
            sync_dir(directory)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        written
    }

    pub(super) fn align(hello: &Hello) -> Result<()> {
        // Matched only while nothing writes the service's copies.
        if hello.current.as_ref().is_some_and(|txn| !awaiting_own_answer(txn)) {
            return Ok(());
        }
        let dir = kept_dir()?;
        let mut matched = true;
        for name in CORE_NAMES {
            let Some(want) = approved_sha(&hello.approved_cores, name) else {
                matched = false;
                continue;
            };
            let target = managed_core_path(name)?;
            let have = file_sha(&target);
            if have.as_deref() == Some(want) {
                continue;
            }
            let Some((bytes, modified)) = source(&dir, want)? else {
                matched = false;
                logging!(
                    warn,
                    Type::Core,
                    "core upgrade: the bundle's {name} differs from the service's and nothing kept matches it"
                );
                continue;
            };
            if let Some(have) = &have {
                back_up(&dir, &target, have)?;
            }
            write_core(&target, &bytes, modified)?;
            logging!(
                info,
                Type::Core,
                "core upgrade: matched the bundle's {name} to the service's"
            );
        }
        // What is kept goes only once the bundle is durably the service's, even where an earlier
        // flush failed.
        if matched && hello.current.is_none() {
            let bundle = managed_core_path(CORE_NAMES[0])?;
            sync_dir(bundle.parent().context("the core has no directory")?)?;
            let _ = std::fs::remove_dir_all(&dir);
        }
        Ok(())
    }
}
