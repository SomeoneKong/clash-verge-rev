use super::{
    Connection, Request, Response, Trigger, TxnView, Update, checked_payload, component_hello, release_is_test,
};
use crate::core::updater::{MIN_PROGRESS, PROGRESS_WINDOW, download_sources, legacy::with_legacy_cache_cleanup};
use crate::{
    config::Config,
    core::{SilentUpdater, handle::Handle},
};
use anyhow::{Context as _, Result, bail, ensure};
use chrono::Utc;
use clash_verge_logging::{Type, logging};
use clash_verge_updater::{
    automatic::{Attempt, Candidate, Eligibility, Identity, Settlement, State, audit_terminal, settle, terminal},
    manifest::Kind,
    state::write_atomic,
    version,
};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri_plugin_updater::UpdaterExt as _;
use tokio_util::sync::CancellationToken;

static CHECK: AtomicU64 = AtomicU64::new(1);
static LAST_CHECK: parking_lot::Mutex<(u64, Option<bool>)> = parking_lot::const_mutex((0, None));
static STARTUP_COMMAND_CONSUMED: AtomicBool = AtomicBool::new(false);

pub(super) fn take_startup_command() -> bool {
    !STARTUP_COMMAND_CONSUMED.swap(true, Ordering::AcqRel)
}

pub(super) fn now() -> i64 {
    Utc::now().timestamp()
}

pub(crate) fn test_mode() -> Result<bool> {
    change(|_, mode| mode)
}

pub(crate) fn skip_test(update: &Update) -> Result<bool> {
    Ok(release_is_test(&update.raw_json)? && !test_mode()?)
}

pub(crate) async fn enabled() -> bool {
    enabled_for(&Config::verge().await.latest_arc())
}

pub(in crate::core) fn enabled_for(verge: &crate::config::IVerge) -> bool {
    cfg!(any(windows, target_os = "linux"))
        && verge.auto_check_update.unwrap_or(true)
        && verge.auto_install_update.unwrap_or(false)
}

pub(super) fn cached() -> bool {
    directory().is_ok_and(|dir| dir.join("payload.zip").is_file())
}

fn directory() -> Result<PathBuf> {
    Ok(super::app_local_dir()?.join("automatic-update"))
}

fn load(dir: &Path) -> Result<State> {
    match fs::read(dir.join("state.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn snapshot() -> Result<State> {
    change(|s, _| s.clone())
}

fn change<T>(f: impl FnOnce(&mut State, bool) -> T) -> Result<T> {
    let mut lock = LAST_CHECK.lock();
    let local = super::app_local_dir()?;
    change_in(
        &local.join("automatic-update"),
        &mut lock,
        local.join("update-test").is_file(),
        f,
    )
}

fn change_in<T>(
    dir: &Path,
    lock: &mut (u64, Option<bool>),
    mode: bool,
    f: impl FnOnce(&mut State, bool) -> T,
) -> Result<T> {
    let mut state = load(dir)?;
    let before = serde_json::to_vec(&state)?;
    let previous = state.candidate.as_ref().map(|c| c.acquisition.clone());
    state.applied_check = if lock.1.is_some_and(|old| old != mode) {
        CHECK.load(Ordering::Relaxed) - 1
    } else {
        lock.0
    };
    state.normalize(now(), mode);
    let value = f(&mut state, mode);
    let after = serde_json::to_vec(&state)?;
    if before != after {
        fs::create_dir_all(dir)?;
        write_atomic(&dir.join("state.json"), &after)?;
    }
    if previous.as_ref() != state.candidate.as_ref().map(|c| &c.acquisition) {
        let _ = fs::remove_file(dir.join("payload.zip"));
    }
    *lock = (state.applied_check, Some(mode));
    Ok(value)
}

pub(crate) fn installed(version: &str) -> Result<()> {
    change(|s, _| {
        if s.candidate
            .as_ref()
            .is_some_and(|c| c.eligibility(now(), Some(version)) == Eligibility::Installed)
        {
            s.withdraw();
        }
    })
}

struct Check {
    ordinal: u64,
    started: i64,
    mode: bool,
}

impl Check {
    async fn fetch(
        updater: &tauri_plugin_updater::Updater,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(Result<Self>, Option<Update>)> {
        use tauri_plugin_updater::Error;
        let check = change(|_, mode| Self {
            ordinal: CHECK.fetch_add(1, Ordering::Relaxed),
            started: now(),
            mode,
        });
        let result = bounded_check(updater.check(), timeout, cancel).await;
        let end = change(|_, _| ());
        let update = match result? {
            Err(Error::TargetNotFound(_) | Error::TargetsNotFound(_)) => None,
            other => other?,
        };
        Ok((end.and(check), update))
    }

    fn observe(&self, update: Option<&Update>, installed: Option<&str>, acquire: bool) -> Result<Option<Candidate>> {
        let offer = match update {
            Some(update) => match candidate_offer(&update.version, &update.raw_json, installed) {
                Ok(offer) => offer,
                Err(error) => {
                    logging!(warn, Type::System, "Invalid automatic update metadata: {error:#}");
                    return Ok(None);
                }
            },
            None => None,
        };
        change(|s, mode| {
            if self.mode == mode
                && s.observe(
                    self.ordinal,
                    self.started,
                    now(),
                    mode,
                    offer,
                    acquire.then(|| nanoid::nanoid!()),
                )
                && acquire
            {
                s.candidate.clone()
            } else {
                None
            }
        })
    }
}

pub(crate) async fn inspect_offer(
    updater: &tauri_plugin_updater::Updater,
    cancel: &CancellationToken,
    local_target: bool,
) -> Result<Option<Update>> {
    let (check, update) = Check::fetch(updater, Duration::from_secs(60), cancel).await?;
    // A missing entry for an explicitly different target says nothing about the local one.
    if (local_target || update.is_some())
        && let Err(error) = check.and_then(|c| c.observe(update.as_ref(), None, false))
    {
        logging!(warn, Type::System, "Checking the cached candidate: {error:#}");
    }
    Ok(update)
}

async fn bounded_check<T>(
    request: impl Future<Output = T>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("update check cancelled"),
        result = tokio::time::timeout(timeout, request) => Ok(result.context("update check timed out")?),
    }
}

fn candidate_offer(
    version: &str,
    raw: &serde_json::Value,
    installed: Option<&str>,
) -> Result<Option<(Identity, Vec<String>)>> {
    let test = release_is_test(raw)?;
    let Some((kind, entry)) = checked_payload(raw)? else {
        return Ok(None);
    };
    ensure!(
        entry.sha256.len() == 64 && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid payload digest"
    );
    tauri::Url::parse(&entry.url).context("invalid payload URL")?;
    if let Some(installed) = installed
        && !version::compare(version, installed)?.is_gt()
    {
        return Ok(None);
    }
    Ok(Some((
        Identity {
            version: version.into(),
            kind,
            sha256: entry.sha256.to_ascii_lowercase(),
            test,
        },
        entry.signatures,
    )))
}

fn invalidate(candidate: &Candidate) -> Result<()> {
    change(|s, _| {
        if s.candidate
            .as_ref()
            .is_some_and(|c| c.acquisition == candidate.acquisition)
        {
            s.withdraw();
        }
    })
}

fn bytes_for(candidate: &Candidate) -> Option<Vec<u8>> {
    read_payload(&directory().ok()?.join("payload.zip"), candidate)
}

fn read_payload(path: &Path, candidate: &Candidate) -> Option<Vec<u8>> {
    use clash_verge_updater::payload::MAX_PAYLOAD;
    use std::io::Read as _;
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_PAYLOAD {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_PAYLOAD + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_PAYLOAD {
        return None;
    }
    verified_payload(bytes, &candidate.identity.sha256).ok()
}

fn verified_payload(bytes: Vec<u8>, sha256: &str) -> Result<Vec<u8>> {
    ensure!(
        clash_verge_updater::payload::hex(&Sha256::digest(&bytes)) == sha256,
        "the payload does not match its digest"
    );
    Ok(bytes)
}

pub(crate) async fn background_offer(
    app: &tauri::AppHandle,
    cancel: &CancellationToken,
    legacy_cache_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<(Option<Update>, bool)> {
    let updater = app.updater_builder().header("Cache-Control", "no-cache")?.build()?;
    let (check, offered) = Check::fetch(&updater, Duration::from_secs(60), cancel).await?;
    let component = super::component_status().await;
    let available = component.is_some();
    with_legacy_cache_cleanup(available, cancel, legacy_cache_dir, async {
        match check {
            Ok(check) => {
                let installed = component
                    .as_ref()
                    .and_then(|h| h.installed_version.as_deref())
                    .unwrap_or(env!("CARGO_PKG_VERSION"));
                background(&check, offered.as_ref(), Some(installed), available, cancel).await
            }
            Err(error) => {
                logging!(warn, Type::System, "Reading the automatic update state: {error:#}");
                Ok(())
            }
        }
    })
    .await?;
    Ok((offered, available))
}

async fn background(
    check: &Check,
    update: Option<&Update>,
    installed: Option<&str>,
    download_allowed: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    let acquire = enabled().await && !cancel.is_cancelled();
    let Some(mut candidate) = check.observe(update, installed, acquire)? else {
        return Ok(());
    };
    if !download_allowed {
        return Ok(());
    }
    if candidate.downloaded {
        if clash_verge_updater::payload::sha256_file(&directory()?.join("payload.zip"))
            .is_ok_and(|digest| digest == candidate.identity.sha256)
        {
            return Ok(());
        }
        let reset = change(|s, _| {
            if s.applied_check != check.ordinal {
                return None;
            }
            let c = s
                .candidate
                .as_mut()
                .filter(|c| c.acquisition == candidate.acquisition)?;
            c.acquisition = nanoid::nanoid!();
            c.downloaded = false;
            c.first_seen = now();
            c.confirmed = None;
            c.failures = 0;
            s.last = None;
            Some(c.clone())
        })?;
        let Some(fresh) = reset else {
            return Ok(());
        };
        candidate = fresh;
    }
    let Some(update) = update else {
        return Ok(());
    };
    let (_, entry) = checked_payload(&update.raw_json)?.context("the update has no component payload")?;
    let bytes = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(()),
        result = download_payload(update, &entry.url, &candidate.identity.sha256) => result?,
    };
    if cancel.is_cancelled() || !enabled().await {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || cache_payload(&candidate, &bytes)).await??;
    Ok(())
}

pub(in crate::core) async fn download_with_sources<F: Future<Output = Result<Vec<u8>>>>(
    version: &str,
    url: &tauri::Url,
    fetch: impl Fn(tauri::Url, Arc<AtomicU64>) -> F,
) -> Result<Vec<u8>> {
    let sources = download_sources(url);
    let mut last_error = None;
    for floor in [MIN_PROGRESS, 1] {
        for url in &sources {
            logging!(info, Type::System, "Downloading update v{version} from {url}");
            let received = Arc::new(AtomicU64::new(0));
            match with_progress(fetch(url.clone(), Arc::clone(&received)), &received, floor).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    logging!(warn, Type::System, "Update download from {url} failed: {e:#}");
                    last_error = Some(e);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no update download source")))
}

async fn with_progress(
    fetch: impl Future<Output = Result<Vec<u8>>>,
    received: &AtomicU64,
    floor: u64,
) -> Result<Vec<u8>> {
    tokio::pin!(fetch);
    let mut seen = 0;
    loop {
        tokio::select! {
            result = &mut fetch => return result,
            () = tokio::time::sleep(PROGRESS_WINDOW) => {
                let total = received.load(Ordering::Relaxed);
                anyhow::ensure!(total - seen >= floor, "only {} bytes in {PROGRESS_WINDOW:?}", total - seen);
                seen = total;
            }
        }
    }
}

async fn download_payload(update: &Update, url: &str, sha256: &str) -> Result<Vec<u8>> {
    let mut client = reqwest::Client::builder();
    if update.no_proxy {
        client = client.no_proxy();
    } else if let Some(proxy) = &update.proxy {
        client = client.proxy(reqwest::Proxy::all(proxy.as_str())?);
    }
    let client = &client.build()?;
    download_with_sources(&update.version, &url.parse()?, |url, received| async move {
        let mut response = client.get(url).send().await?.error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                body.len() + chunk.len() <= clash_verge_updater::payload::MAX_PAYLOAD as usize,
                "payload is too large"
            );
            body.extend_from_slice(&chunk);
            received.store(body.len() as u64, Ordering::Relaxed);
        }
        verified_payload(body, sha256)
    })
    .await
}

fn cache_payload(candidate: &Candidate, bytes: &[u8]) -> Result<()> {
    let dir = directory()?;
    change(|s, mode| -> Result<()> {
        let Some(current) = s.download_target(&candidate.acquisition, mode) else {
            return Ok(());
        };
        publish_payload(&dir, current, bytes)
    })?
}

fn publish_payload(dir: &Path, current: &mut Candidate, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let temp = dir.join("payload.tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if clash_verge_updater::payload::sha256_file(&temp)? != current.identity.sha256 {
        let _ = fs::remove_file(temp);
        bail!("the downloaded payload does not match its digest");
    }
    fs::rename(temp, dir.join("payload.zip"))?;
    current.downloaded = true;
    Ok(())
}

async fn finish(attempt: &Attempt, observation: Settlement) -> Result<()> {
    let reason = change(|s, _| {
        if s.attempt
            .as_ref()
            .is_some_and(|a| a.acquisition == attempt.acquisition && a.txn == attempt.txn)
        {
            settle(s, observation, now())
        } else {
            None
        }
    })?;
    if let Some(reason) = reason
        && let Ok(mut connection) = Connection::connect().await
    {
        let claim = serde_json::json!({"event":"auto_install_failed", "version":attempt.identity.version, "txn":attempt.txn, "reason":reason});
        let _ = connection.request(&Request::Report { claim }).await;
    }
    Ok(())
}

async fn lookup(id: &str, staged: Settlement) -> Result<Settlement> {
    let hello = component_hello().await?;
    if let Some(txn) = hello.current.filter(|t| t.id == id) {
        return Ok(Settlement::Active(txn.kind));
    }
    let mut connection = Connection::connect().await?;
    match connection.request(&Request::Status { txn: id.into() }).await? {
        Response::Txn(TxnView::Finished(f)) => return Ok(terminal(f.outcome, f.error)),
        Response::Txn(TxnView::Running(txn)) => return Ok(Settlement::Active(txn.kind)),
        Response::Txn(TxnView::Staged(_)) => return Ok(staged),
        Response::Txn(TxnView::Unknown) => {}
        other => bail!("unexpected transaction status {other:?}"),
    }
    let mut before = None;
    loop {
        let records = super::interface::audit(before).await.map_err(anyhow::Error::msg)?;
        let Some(last) = records.last() else {
            return Ok(Settlement::Interrupted);
        };
        before = Some(last.seq);
        if let Some(outcome) = audit_terminal(id, records) {
            return Ok(outcome);
        }
    }
}

pub(super) async fn settle_previous() -> Result<bool> {
    let Some(attempt) = snapshot()?.attempt else {
        return Ok(false);
    };
    let observation = match &attempt.txn {
        None => Settlement::Interrupted,
        Some(id) => {
            let outcome = recover(id).await;
            if let Settlement::Active(kind) = outcome {
                super::resolve_active_update(id, kind, &attempt.identity.version).await
            } else {
                outcome
            }
        }
    };
    let restart = restart_for_settlement(&observation, &attempt.identity.version, env!("CARGO_PKG_VERSION"));
    finish(&attempt, observation).await?;
    if restart {
        restart_before_core();
    }
    Ok(true)
}

fn restart_for_settlement(outcome: &Settlement, target: &str, running: &str) -> bool {
    matches!(outcome, Settlement::Applied) && version::compare(target, running).is_ok_and(|order| order.is_gt())
}

fn restart_before_core() -> ! {
    crate::utils::server::shutdown_embedded_server();
    super::restart_updated_binary()
}

pub(super) async fn startup() -> Result<()> {
    if !enabled().await {
        return Ok(());
    }
    let Some(c) = snapshot()?.candidate else { return Ok(()) };
    if !c.downloaded || c.failures >= 3 {
        return Ok(());
    }
    if !cached() {
        invalidate(&c)?;
        return Ok(());
    }
    let app = Handle::app_handle();
    let cancel = SilentUpdater::global().background_token();
    let updater = app
        .updater_builder()
        .no_proxy()
        .header("Cache-Control", "no-cache")?
        .build()?;
    if let Ok((check, update)) = Check::fetch(&updater, Duration::from_secs(5), &cancel).await {
        check?.observe(
            update.as_ref(),
            Some(env!("CARGO_PKG_VERSION")),
            enabled().await && !cancel.is_cancelled(),
        )?;
    }
    if cancel.is_cancelled() || !enabled().await {
        return Ok(());
    }
    let Some(candidate) = snapshot()?.candidate else {
        return Ok(());
    };
    if candidate.acquisition != c.acquisition || candidate.eligibility(now(), None) != Eligibility::Ready {
        return Ok(());
    }
    let (mut connection, status) = match super::connect_component().await {
        Ok(pair) => pair,
        Err(error) => {
            change(|s, _| s.component_unavailable(&candidate.acquisition, format!("{error:#}"), now()))?;
            return Ok(());
        }
    };
    if status.current.is_some() {
        super::resolve_current(Some(status), false).await;
        return Ok(());
    }
    if status.staged
        || super::resolve_existing(&status, super::repair_attempted).await
        || !newer_than_pending(&candidate.identity.version, status.needs_repair.as_ref())
    {
        return Ok(());
    }
    let gate = super::take_gate()?;
    let Some(installed_version) = status.installed_version else {
        return Ok(());
    };
    if !version::compare(&candidate.identity.version, &installed_version)?.is_gt() {
        installed(&installed_version)?;
        return Ok(());
    }
    let cached_candidate = candidate.clone();
    let Some(bytes) = tokio::task::spawn_blocking(move || bytes_for(&cached_candidate)).await? else {
        invalidate(&candidate)?;
        return Ok(());
    };
    if cancel.is_cancelled() || !enabled().await {
        return Ok(());
    }
    let Some(mut attempt) = change(|s, _| {
        let current = s.candidate.as_ref().filter(|c| {
            c.acquisition == candidate.acquisition
                && c.eligibility(now(), Some(&installed_version)) == Eligibility::Ready
        })?;
        let attempt = current.attempt(now());
        s.attempt = Some(attempt.clone());
        s.last = None;
        Some(attempt)
    })?
    else {
        return Ok(());
    };
    STARTUP_COMMAND_CONSUMED.store(true, Ordering::Release);
    let splash = !Config::verge().await.latest_arc().enable_silent_start.unwrap_or(false) && !super::start_hidden();
    if splash {
        SilentUpdater::show_update_splash(app, &candidate.identity.version);
    }
    let outcome = apply(
        &mut connection,
        &candidate,
        &bytes,
        &mut attempt,
        &installed_version,
        &cancel,
    )
    .await?;
    if matches!(outcome, Settlement::Applied) {
        restart_before_core();
    }
    let needs_repair = matches!(outcome, Settlement::NeedsRepair);
    let unavailable = matches!(outcome, Settlement::Unavailable);
    finish(&attempt, outcome).await?;
    drop(gate);
    if !unavailable {
        let (_, status) = super::resolve_current(component_hello().await.ok(), true).await;
        if needs_repair
            && let Some(status) = status
            && status.current.is_none()
            && !status.staged
        {
            super::resolve_existing(&status, super::repair_attempted).await;
        }
    }
    Ok(())
}

pub(super) fn newer_than_pending(version: &str, pending: Option<&super::Finished>) -> bool {
    pending.is_none_or(|p| version::compare(version, &p.version).is_ok_and(|order| order.is_gt()))
}

async fn apply(
    connection: &mut Connection,
    candidate: &Candidate,
    bytes: &[u8],
    attempt: &mut Attempt,
    installed: &str,
    cancel: &CancellationToken,
) -> Result<Settlement> {
    let trigger = Trigger {
        kind: "auto".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    let staged = match connection.stage(trigger, bytes, candidate.signatures.clone()).await? {
        Response::Staged(staged) => staged,
        Response::Rejected { reason } => return Ok(Settlement::Rejected(reason)),
        Response::Busy { .. } => return Ok(Settlement::Interrupted),
        other => bail!("unexpected stage response {other:?}"),
    };
    attempt.txn = Some(staged.txn.clone());
    let enabled = enabled().await;
    let authorized = change(|s, _| s.authorize_staged(attempt, installed, enabled && !cancel.is_cancelled(), now()));
    let matches = staged.matches_target(
        &candidate.identity.version,
        candidate.identity.kind,
        &candidate.identity.sha256,
    );
    if !matches!(authorized, Ok(true)) || cancel.is_cancelled() || !matches {
        let _ = connection.request(&Request::Cancel { txn: staged.txn }).await;
        return Ok(if !authorized? || cancel.is_cancelled() {
            Settlement::Interrupted
        } else {
            Settlement::Rejected("the staged payload does not match the candidate".into())
        });
    }
    let relaunch = staged.kind == Kind::Full;
    let view = match connection
        .request(&Request::Apply {
            txn: staged.txn.clone(),
            relaunch,
        })
        .await
    {
        Ok(Response::Txn(view)) => Some(view),
        Ok(Response::Rejected { reason }) => return Ok(Settlement::Rejected(reason)),
        Ok(Response::Busy { .. }) => return Ok(Settlement::Interrupted),
        Ok(Response::Error { .. }) | Err(_) => super::applied_anyway(&staged.txn).await,
        Ok(other) => bail!("unexpected apply response {other:?}"),
    };
    let observation = match view {
        Some(TxnView::Running(txn)) => Settlement::Active(txn.kind),
        Some(TxnView::Finished(f)) => terminal(f.outcome, f.error),
        _ => recover(&staged.txn).await,
    };
    if let Settlement::Active(kind) = observation {
        return Ok(super::resolve_active_update(&staged.txn, kind, &staged.version).await);
    }
    Ok(observation)
}

pub(super) async fn track_transaction(id: &str) -> Settlement {
    follow_own_transaction(|| async {
        match super::view(id).await {
            Some(TxnView::Running(txn)) => Settlement::Active(txn.kind),
            Some(TxnView::Finished(f)) => terminal(f.outcome, f.error),
            _ => recover(id).await,
        }
    })
    .await
}

async fn follow_own_transaction<F: Future<Output = Settlement>>(mut poll: impl FnMut() -> F) -> Settlement {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45 * 60);
    loop {
        match poll().await {
            Settlement::Active(_) => {}
            Settlement::Unavailable if tokio::time::Instant::now() < deadline => {}
            result => return result,
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn recover(id: &str) -> Settlement {
    if let Ok(outcome) = lookup(id, Settlement::Interrupted).await {
        return outcome;
    }
    clash_verge_updater::client::read_state().map_or(Settlement::Unavailable, |state| recover_from_state(id, state))
}

fn recover_from_state(id: &str, state: clash_verge_updater::state::State) -> Settlement {
    if let Some(txn) = state.txn.filter(|txn| txn.id == id)
        && txn.worker.is_some_and(clash_verge_updater::sys::is_running)
    {
        return Settlement::Active(txn.kind);
    }
    if let Some(done) = state.last.filter(|f| f.id == id) {
        return terminal(done.outcome, done.error);
    }
    Settlement::Unavailable
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn bad_payload_sources_leave_the_candidate_intact_and_the_legacy_cache_cleared() {
        use std::cell::Cell;
        let url = tauri::Url::parse("https://github.com/example/project/releases/download/v2/payload.zip").unwrap();
        let identity = Identity {
            version: "2.0.0".into(),
            kind: Kind::Deb,
            sha256: clash_verge_updater::payload::hex(&Sha256::digest(b"valid")),
            test: false,
        };
        let mut state = State::default();
        state.observe(1, 1, 1, false, Some((identity, vec![])), Some("first".into()));
        let candidate = state.candidate.as_mut().unwrap();
        let dir = std::env::temp_dir().join(format!("verge-auto-sources-{}", nanoid::nanoid!()));
        let legacy = dir.join("legacy");
        let cancel = CancellationToken::new();
        for has_mirror in [false, true] {
            fs::create_dir_all(&legacy).unwrap();
            fs::write(legacy.join("pending_update.bin"), b"old").unwrap();
            fs::write(legacy.join("pending_update.json"), b"{}").unwrap();
            let calls = Cell::new(0);
            let bytes = with_legacy_cache_cleanup(
                true,
                &cancel,
                || Ok(legacy.clone()),
                download_with_sources("2", &url, |source, _| {
                    assert!(!legacy.exists());
                    calls.set(calls.get() + 1);
                    let body = if has_mirror && source != url {
                        b"valid".as_slice()
                    } else {
                        b"error page"
                    };
                    std::future::ready(verified_payload(body.to_vec(), &candidate.identity.sha256))
                }),
            )
            .await;
            assert!(!legacy.exists());
            if has_mirror {
                publish_payload(&dir, candidate, &bytes.unwrap()).unwrap();
                assert_eq!(calls.get(), 2);
                assert_eq!(fs::read(dir.join("payload.zip")).unwrap(), b"valid");
            } else {
                assert!(bytes.is_err());
                assert!(calls.get() > 1);
                assert!(publish_payload(&dir, candidate, b"corrupt on disk").is_err());
                assert!(!dir.join("payload.zip").exists());
            }
            assert_eq!(candidate.acquisition, "first");
            assert_eq!(candidate.first_seen, 1);
            assert_eq!(candidate.downloaded, has_mirror);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn offline_recovery_requires_the_same_transaction_and_live_worker() {
        let worker = clash_verge_updater::sys::current_process();
        for kind in [Kind::Patch, Kind::Deb, Kind::Full] {
            let mut state: clash_verge_updater::state::State = serde_json::from_value(json!({
                "format": 1, "txn": {"id":"own","version":"2.0.0","kind":kind,"phase":"applying",
                "initiator":{"id":"user","pid":1,"user":"user"},"trigger":{"kind":"auto","client_version":"1"},
                "started_at":"now","payload_sha256":"hash","payload_size":1,"key_id":"key","relaunch":kind == Kind::Full,
                "worker":worker}
            })).unwrap();
            assert!(matches!(
                recover_from_state("other", state.clone()),
                Settlement::Unavailable
            ));
            assert!(
                matches!(recover_from_state("own", state.clone()), Settlement::Active(observed) if observed == kind)
            );
            state.txn.as_mut().unwrap().worker.as_mut().unwrap().created ^= 1;
            assert!(matches!(recover_from_state("own", state), Settlement::Unavailable));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn manifest_timeout_and_cancellation_leave_the_next_check_runnable() {
        let cancel = CancellationToken::new();
        let limit = Duration::from_secs(5);
        let start = tokio::time::Instant::now();
        assert!(
            bounded_check(std::future::pending::<()>(), limit, &cancel)
                .await
                .is_err()
        );
        assert_eq!(start.elapsed(), limit);
        let (result, ()) = tokio::join!(
            bounded_check(std::future::pending::<()>(), Duration::from_secs(60), &cancel),
            async {
                tokio::task::yield_now().await;
                cancel.cancel()
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(start.elapsed(), limit);
        assert!(bounded_check(async {}, limit, &CancellationToken::new()).await.is_ok());
    }

    #[test]
    fn malformed_fork_metadata_is_not_a_withdrawal() {
        let platform = super::super::Platform::current().unwrap().key();
        let entry = json!({"url":"https://example.org/update.zip","sha256":"a".repeat(64),"signatures":["s"]});
        let mut raw = json!({"fork":{"payloads":{(platform):{"full":entry,"deb":entry,"bundle":entry}}}});
        let lower = candidate_offer("2.0.0", &raw, None).unwrap().unwrap().0;
        for kind in ["full", "deb", "bundle"] {
            raw["fork"]["payloads"][platform][kind]["sha256"] = json!("A".repeat(64));
        }
        assert_eq!(candidate_offer("2.0.0", &raw, None).unwrap().unwrap().0, lower);
        raw["fork"]["test"] = json!("true");
        assert!(candidate_offer("2.0.0", &raw, None).is_err());
        raw["fork"]["test"] = json!(false);
        raw["fork"]["payloads"][platform]["full"]["signatures"] = json!("s");
        assert!(candidate_offer("2.0.0", &raw, None).is_err());
        raw["fork"]["payloads"] = json!({});
        assert!(candidate_offer("2.0.0", &raw, None).unwrap().is_none());
    }

    #[test]
    fn oversized_disk_payload_is_refused_before_hashing() {
        let path = std::env::temp_dir().join(format!("verge-auto-cache-{}", nanoid::nanoid!()));
        fs::write(&path, b"ok").unwrap();
        let mut s = State::default();
        let identity = Identity {
            kind: Kind::Deb,
            version: "2.0.0".into(),
            test: false,
            sha256: clash_verge_updater::payload::hex(&Sha256::digest(b"ok")),
        };
        s.observe(1, 1, 1, false, Some((identity, vec![])), Some("id".into()));
        let c = s.candidate.unwrap();
        assert_eq!(read_payload(&path, &c).unwrap(), b"ok");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(clash_verge_updater::payload::MAX_PAYLOAD + 1)
            .unwrap();
        assert!(read_payload(&path, &c).is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn observing_test_mode_persists_withdrawal_even_without_a_manifest() {
        let dir = std::env::temp_dir().join(format!("verge-auto-mode-{}", nanoid::nanoid!()));
        let identity = Identity {
            version: "2.0.0".into(),
            kind: Kind::Deb,
            sha256: "digest".into(),
            test: false,
        };
        let mut lock = (0, None);
        change_in(&dir, &mut lock, false, |s, _| {
            s.observe(1, 1, 1, false, Some((identity.clone(), vec![])), Some("old".into()));
            s.candidate.as_mut().unwrap().confirmed = Some(100000);
        })
        .unwrap();
        fs::write(dir.join("payload.zip"), b"old").unwrap();
        change_in(&dir, &mut lock, true, |_, _| ()).unwrap();
        assert!(load(&dir).unwrap().candidate.is_none());
        assert!(!dir.join("payload.zip").exists());
        change_in(&dir, &mut lock, false, |s, _| {
            assert!(s.candidate.is_none());
            s.observe(
                s.applied_check + 1,
                100001,
                100001,
                false,
                Some((identity, vec![])),
                Some("new".into()),
            );
        })
        .unwrap();
        let candidate = load(&dir).unwrap().candidate.unwrap();
        assert_eq!(candidate.acquisition, "new");
        assert!(candidate.confirmed.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_outlasts_twenty_minutes_but_offline_state_has_a_deadline() {
        use tokio::time::Instant;
        let start = Instant::now();
        let outcome = follow_own_transaction(|| {
            std::future::ready(if start.elapsed() >= Duration::from_secs(21 * 60) {
                Settlement::Applied
            } else {
                Settlement::Unavailable
            })
        })
        .await;
        assert!(restart_for_settlement(&outcome, "2.0.0", "1.9.0"));
        assert!(!restart_for_settlement(&outcome, "2.0.0", "2.0.0"));
        let start = Instant::now();
        assert!(matches!(
            follow_own_transaction(|| std::future::ready(Settlement::Unavailable)).await,
            Settlement::Unavailable
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(45 * 60));
    }
}
