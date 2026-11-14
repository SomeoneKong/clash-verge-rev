//! The resident service: answers the GUI, verifies payloads, starts a worker per transaction and
//! finishes the transaction when the worker is gone. A core upgrade waits in between for the GUI
//! to say whether the new core runs.

use crate::txn::{self, WorkerResult};
use crate::{logger, platform};
use anyhow::{Context as _, Result};
use clash_verge_updater::audit::{self, AuditLog, Record, Source, Stream};
use clash_verge_updater::manifest::{Kind, Platform};
use clash_verge_updater::paths::Paths;
use clash_verge_updater::payload::{MAX_PAYLOAD, Payload, sha256_file};
use clash_verge_updater::protocol::{
    ApprovedCore, CAPABILITY_CORE, Hello, MAX_FRAME, PROTOCOL, Request, Response, Staged as StagedView, Trigger,
    TxnView, UserPhaseEvent, read_frame, write_frame,
};
use clash_verge_updater::state::{Decision, Initiator, Outcome, Phase, State, Txn};
use clash_verge_updater::sys;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};

const STAGE_TIMEOUT: Duration = Duration::from_secs(120);
const STAGED_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// A connection that sends nothing for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// The GUI keeps a connection only briefly, so a few at a time are plenty.
const MAX_CONNECTIONS: usize = 8;
const MAX_CLAIM: usize = 4 * 1024;
/// A worker still running this long after its transaction began is stopped; the transaction is
/// then recovered like any interrupted one. Counted from the persisted start, so restarts of the
/// service do not extend it.
const TXN_DEADLINE: chrono::Duration = chrono::Duration::minutes(45);

pub struct Daemon {
    paths: Paths,
    audit: AuditLog,
    inner: tokio::sync::Mutex<Inner>,
    /// Set while a payload is being received: one at a time, claimed before memory is set aside.
    receiving: AtomicBool,
    connections: tokio::sync::Semaphore,
    /// Transactions with a supervisor running: two would recover the same one side by side.
    supervised: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The digests of the approved cores, by name, with the size and time they were read at.
    approved: std::sync::Mutex<HashMap<String, (FileStamp, String)>>,
}

type FileStamp = (u64, Option<std::time::SystemTime>);

/// Marks a transaction supervised for as long as its supervisor runs.
struct Supervising(Arc<Daemon>, String);

impl Drop for Supervising {
    fn drop(&mut self) {
        self.0
            .supervised
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.1);
    }
}

/// Releases the receiving slot however staging ends.
struct ReceivingSlot<'a>(&'a AtomicBool);

impl Drop for ReceivingSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Inner {
    state: State,
    staged: Option<Staged>,
}

struct Staged {
    txn: String,
    payload: Arc<Payload>,
    signatures: Vec<String>,
    initiator: Initiator,
    trigger: Trigger,
    expires: Instant,
    repair: bool,
}

impl Staged {
    fn view(&self) -> StagedView {
        StagedView {
            txn: self.txn.clone(),
            version: self.payload.manifest.version.clone(),
            kind: self.payload.manifest.kind,
            size: self.payload.size,
            sha256: self.payload.sha256.clone(),
        }
    }
}

fn new_txn_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-{}-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl Daemon {
    pub fn open() -> Result<Arc<Self>> {
        let paths = Paths::system();
        platform::prepare_layout(&paths)?;
        let audit = AuditLog::open(&paths.logs_dir())?;
        let state = State::load(&paths.state_file())?;
        Ok(Arc::new(Self {
            paths,
            audit,
            inner: tokio::sync::Mutex::new(Inner { state, staged: None }),
            receiving: AtomicBool::new(false),
            connections: tokio::sync::Semaphore::new(MAX_CONNECTIONS),
            supervised: std::sync::Mutex::default(),
            approved: std::sync::Mutex::default(),
        }))
    }

    /// Picks up a transaction an earlier run left, in the background so the service answers
    /// meanwhile; otherwise notes any version change made while it was not running.
    pub async fn start(self: &Arc<Self>) {
        let pending = self
            .inner
            .lock()
            .await
            .state
            .txn
            .as_ref()
            .filter(|txn| {
                matches!(
                    txn.phase,
                    Phase::Applying | Phase::RecoveryBlocked | Phase::Published | Phase::Reverting
                )
            })
            .map(|txn| txn.id.clone());
        if let Some(id) = pending {
            self.supervise(id);
            return;
        }
        let me = Arc::clone(self);
        let observed = tokio::task::spawn_blocking(move || {
            let mut inner = me.inner.blocking_lock();
            txn::observe(&me.paths, &me.audit, &mut inner.state)
        })
        .await;
        match observed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => logger::log(&format!("startup: {error:#}")),
            Err(error) => logger::log(&format!("startup: {error}")),
        }
    }

    pub async fn handle<S>(self: Arc<Self>, mut stream: S, peer: Initiator)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let Ok(_permit) = self.connections.try_acquire() else {
            return;
        };
        loop {
            let Ok(Ok(request)) = tokio::time::timeout(IDLE_TIMEOUT, read_frame::<_, Request>(&mut stream)).await
            else {
                return;
            };
            let (response, keep) = match request {
                Request::Stage {
                    trigger,
                    size,
                    signatures,
                } => self.stage(&mut stream, &peer, trigger, size, signatures).await,
                other => (self.dispatch(other, &peer).await, true),
            };
            let written = tokio::time::timeout(IDLE_TIMEOUT, write_frame(&mut stream, &response)).await;
            if !matches!(written, Ok(Ok(()))) || !keep {
                return;
            }
        }
    }

    async fn dispatch(self: &Arc<Self>, request: Request, peer: &Initiator) -> Response {
        let result = match request {
            Request::Hello { .. } => self.hello().await,
            Request::Apply { txn, relaunch } => self.apply(&txn, relaunch, peer).await,
            Request::Status { txn } => Ok(Response::Txn(self.view(&txn).await)),
            Request::Confirm { txn, ok } => self.confirm(&txn, ok, peer).await,
            Request::Cancel { txn } => self.cancel(&txn, peer).await,
            Request::Abandon { txn } => self.abandon(&txn, peer).await,
            Request::Repair { txn, relaunch } => self.repair(txn.as_deref(), relaunch, peer).await,
            Request::UserPhase { txn, event } => self.user_phase(&txn, event, peer).await,
            Request::Report { claim } => {
                if serde_json::to_vec(&claim).map_or(true, |bytes| bytes.len() > MAX_CLAIM) {
                    Ok(self.reject(peer, "the claim is too large".into()))
                } else {
                    // Who sent it is what the system says, kept apart from what the client says.
                    let record = Record::new(Source::Claimed, "claim")
                        .detail(serde_json::json!({ "claim": claim, "peer": peer }));
                    self.audit
                        .write_limited(Stream::Claims, &peer.id, record)
                        .map(|()| Response::Done)
                }
            }
            Request::Audit { before_seq, limit } => {
                let dir = self.paths.logs_dir();
                let limit = limit.clamp(1, 500) as usize;
                let mut records = tokio::task::spawn_blocking(move || audit::read_page(&dir, before_seq, limit))
                    .await
                    .unwrap_or_default();
                // The answer must fit a frame; the client pages on from the last record it got.
                let mut budget = MAX_FRAME as usize / 2;
                let fits = records
                    .iter()
                    .take_while(|record| {
                        let size = serde_json::to_vec(record).map_or(usize::MAX, |bytes| bytes.len() + 1);
                        budget.checked_sub(size).inspect(|left| budget = *left).is_some()
                    })
                    .count();
                records.truncate(fits);
                Ok(Response::Audit { records })
            }
            Request::Stage { .. } => unreachable!("stage is handled with its payload"),
        };
        result.unwrap_or_else(|error| Response::Error {
            message: format!("{error:#}"),
        })
    }

    async fn hello(&self) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        tokio::task::block_in_place(|| {
            expire_user_phase(&self.paths, &self.audit, &mut inner.state)?;
            txn::observe(&self.paths, &self.audit, &mut inner.state)
        })?;
        let approved_cores = tokio::task::block_in_place(|| self.approved_cores());
        Ok(Response::Hello(Box::new(Hello {
            protocol: PROTOCOL,
            updater_version: env!("CARGO_PKG_VERSION").into(),
            platform: Platform::current().map(Platform::key).unwrap_or_default().into(),
            installed_version: platform::installed_version(),
            floor: inner.state.floor.clone(),
            fingerprint: platform::installed_fingerprint(),
            current: inner.state.txn.clone(),
            needs_repair: inner.state.needs_repair.clone(),
            capabilities: vec![CAPABILITY_CORE.into()],
            service_installed: platform::service_installed(),
            core_floor: inner.state.core_floor.clone(),
            core_needs_repair: inner.state.core_needs_repair.clone(),
            approved_cores,
        })))
    }

    /// The digests of the approved cores, read again only once a file's size or time changed.
    fn approved_cores(&self) -> Vec<ApprovedCore> {
        let mut cache = self.approved.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        platform::approved_cores()
            .into_iter()
            .map(|(name, path)| {
                let sha256 = std::fs::metadata(&path).ok().and_then(|meta| {
                    let stamp = (meta.len(), meta.modified().ok());
                    if let Some((cached, digest)) = cache.get(&name)
                        && *cached == stamp
                    {
                        return Some(digest.clone());
                    }
                    let digest = sha256_file(&path).ok()?;
                    cache.insert(name.clone(), (stamp, digest.clone()));
                    Some(digest)
                });
                ApprovedCore { name, sha256 }
            })
            .collect()
    }

    async fn view(&self, id: &str) -> TxnView {
        let mut inner = self.inner.lock().await;
        drop_expired(&mut inner);
        if let Some(staged) = inner.staged.as_ref().filter(|staged| staged.txn == id) {
            return TxnView::Staged(staged.view());
        }
        if let Some(txn) = inner.state.txn.as_ref().filter(|txn| txn.id == id) {
            return TxnView::Running(txn.clone());
        }
        [
            inner.state.last.as_ref(),
            inner.state.needs_repair.as_ref(),
            inner.state.core_needs_repair.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find(|finished| finished.id == id)
        .map_or(TxnView::Unknown, |finished| TxnView::Finished(finished.clone()))
    }

    fn reject(&self, peer: &Initiator, reason: String) -> Response {
        let record = Record::new(Source::Actual, "rejected")
            .error(reason.clone())
            .detail(serde_json::json!({ "peer": peer }));
        if let Err(error) = self.audit.write_limited(Stream::Rejected, &peer.id, record) {
            logger::log(&format!("audit: {error:#}"));
        }
        Response::Rejected { reason }
    }

    /// Reads and verifies a payload. The connection is dropped after a payload that could not be
    /// read in full, since the stream is then out of step.
    async fn stage<S>(
        self: &Arc<Self>,
        stream: &mut S,
        peer: &Initiator,
        trigger: Trigger,
        size: u64,
        signatures: Vec<String>,
    ) -> (Response, bool)
    where
        S: AsyncRead + Unpin,
    {
        if size > MAX_PAYLOAD {
            return (self.reject(peer, "payload is too large".into()), false);
        }
        // What the client says is recorded with the transaction, so it is kept small.
        let oversized = trigger.kind.len() > 32
            || trigger.client_version.len() > 64
            || signatures.is_empty()
            || signatures.len() > 4
            || signatures.iter().any(|signature| signature.len() > 2048);
        if oversized {
            return (self.reject(peer, "the request carries oversized fields".into()), false);
        }
        let busy = if self.receiving.swap(true, Ordering::AcqRel) {
            Some(TxnView::Unknown)
        } else {
            let mut inner = self.inner.lock().await;
            drop_expired(&mut inner);
            let busy = busy_for(&inner, peer);
            drop(inner);
            if busy.is_some() {
                self.receiving.store(false, Ordering::Release);
            }
            busy
        };
        if let Some(current) = busy {
            // Read past the payload without keeping it, so the answer reaches the client.
            let drained = tokio::time::timeout(
                STAGE_TIMEOUT,
                tokio::io::copy(&mut (&mut *stream).take(size), &mut tokio::io::sink()),
            )
            .await;
            return (
                Response::Busy { current },
                matches!(drained, Ok(Ok(read)) if read == size),
            );
        }
        let _slot = ReceivingSlot(&self.receiving);
        let mut bytes = vec![0; size as usize];
        if !matches!(
            tokio::time::timeout(STAGE_TIMEOUT, stream.read_exact(&mut bytes)).await,
            Ok(Ok(_))
        ) {
            return (self.reject(peer, "the payload did not arrive in time".into()), false);
        }
        let sigs = signatures.clone();
        let opened = tokio::task::spawn_blocking(move || Payload::open(bytes, &sigs)).await;
        let payload = match opened {
            Ok(Ok(payload)) => payload,
            Ok(Err(error)) => return (self.reject(peer, format!("{error:#}")), true),
            Err(error) => return (self.reject(peer, error.to_string()), true),
        };

        let mut inner = self.inner.lock().await;
        if let Some(current) = busy_for(&inner, peer) {
            return (Response::Busy { current }, true);
        }
        if let Err(error) = txn::admit(&inner.state, &payload.manifest, false) {
            return (self.reject(peer, format!("{error:#}")), true);
        }
        let staged = Staged {
            txn: new_txn_id(),
            payload: Arc::new(payload),
            signatures,
            initiator: peer.clone(),
            trigger,
            expires: Instant::now() + STAGED_LIFETIME,
            repair: false,
        };
        let view = staged.view();
        let mut record = Record::new(Source::Actual, "staged")
            .txn(&staged.txn)
            .versions(platform::installed_version().as_deref(), Some(&view.version));
        record.detail = Some(serde_json::json!({ "initiator": peer, "kind": view.kind }));
        if let Err(error) = self.audit.write(Stream::Audit, record) {
            return (
                Response::Error {
                    message: format!("{error:#}"),
                },
                true,
            );
        }
        inner.staged = Some(staged);
        drop(inner);
        (Response::Staged(view), true)
    }

    async fn apply(self: &Arc<Self>, id: &str, relaunch: bool, peer: &Initiator) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        drop_expired(&mut inner);
        if let Some(txn) = inner.state.txn.as_ref() {
            return Ok(if txn.id == id {
                Response::Txn(TxnView::Running(txn.clone()))
            } else {
                Response::Busy {
                    current: TxnView::Running(txn.clone()),
                }
            });
        }
        let Some(staged) = inner
            .staged
            .take_if(|staged| staged.txn == id && staged.initiator.id == peer.id)
        else {
            return Ok(Response::Rejected {
                reason: "no such staged update for this user".into(),
            });
        };
        // A refusal here changes nothing, so the app may still install the update on its own.
        let ready = tokio::task::block_in_place(|| {
            let manifest = &staged.payload.manifest;
            txn::admit(&inner.state, manifest, staged.repair).and_then(|()| {
                if manifest.kind == Kind::Core {
                    platform::check_ready_core(&self.paths, &inner.state)
                } else {
                    platform::check_ready(manifest)
                }
            })
        });
        if let Err(error) = ready {
            return Ok(self.reject(peer, format!("{error:#}")));
        }
        let begun = tokio::task::block_in_place(|| self.begin(&mut inner.state, &staged, relaunch));
        let started = applying(&inner.state, &staged.txn);
        drop(inner);
        // Once the marker exists the transaction is watched to its end, whatever `begin` returned;
        // a bundle's from the replaced report on, when its privileged phase starts.
        if started {
            self.supervise(staged.txn);
        }
        Ok(Response::Txn(TxnView::Running(begun?)))
    }

    /// Persists the transaction, records it, then starts its worker. Nothing has changed if this
    /// fails before the worker starts; a worker that did not start ends the transaction as failed.
    fn begin(&self, state: &mut State, staged: &Staged, relaunch: bool) -> Result<Txn> {
        let manifest = &staged.payload.manifest;
        txn::save_payload(&self.paths, &staged.txn, &staged.payload, &staged.signatures)?;
        let txn_dir = self.paths.txn_dir(&staged.txn);
        std::fs::create_dir_all(&txn_dir).with_context(|| format!("failed to create {}", txn_dir.display()))?;
        clash_verge_updater::state::sync_parent(&txn_dir)?;
        let txn = Txn {
            id: staged.txn.clone(),
            version: manifest.version.clone(),
            // The version of the cores in place is not something the component knows.
            from_version: if manifest.kind == Kind::Core {
                None
            } else {
                platform::installed_version()
            },
            kind: manifest.kind,
            phase: if manifest.kind == Kind::Bundle {
                Phase::UserPhasePending
            } else {
                Phase::Applying
            },
            initiator: staged.initiator.clone(),
            trigger: staged.trigger.clone(),
            started_at: audit::now(),
            payload_sha256: staged.payload.sha256.clone(),
            payload_size: staged.payload.size,
            key_id: staged.payload.key_id.clone(),
            relaunch,
            repair: staged.repair,
            external_mark: txn::external_mark(&self.paths),
            worker: None,
            confirm_by: None,
            decision: None,
        };
        state.txn = Some(txn.clone());
        if let Err(error) = state.save(&self.paths.state_file()) {
            state.txn = None;
            return Err(error.context("the update was not started"));
        }
        if let Err(error) = self.audit.write(Stream::Audit, txn::txn_record(&txn, "applying")) {
            state.txn = None;
            state.save(&self.paths.state_file())?;
            return Err(error.context("the update was not started: it could not be recorded"));
        }
        if txn.phase == Phase::Applying {
            match platform::spawn_worker(&self.paths, &txn.id) {
                Ok(worker) => {
                    if let Some(current) = state.txn.as_mut() {
                        current.worker = Some(worker);
                    }
                    // The worker runs either way; supervision finds it through its scope or journal.
                    if let Err(error) = state.save(&self.paths.state_file()) {
                        logger::log(&format!("transaction {}: {error:#}", txn.id));
                    }
                }
                // The worker may run all the same (its start was not confirmed in time), so the
                // transaction stays open; supervision ends it once nothing of it runs.
                Err(error) => return Err(error.context("the update worker did not confirm its start")),
            }
        }
        Ok(state.txn.clone().unwrap_or(txn))
    }

    /// Waits for the transaction's writers to stop, then finishes it and restarts the GUI if the
    /// GUI left for the installer. A published core upgrade is watched on until it is settled.
    pub fn supervise(self: &Arc<Self>, id: String) {
        let fresh = self
            .supervised
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone());
        if !fresh {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let _supervising = Supervising(Arc::clone(&me), id.clone());
            let mut stop_requested: Option<Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let state = me.inner.lock().await.state.clone();
                let Some(txn) = state.txn.clone().filter(|txn| {
                    txn.id == id
                        && matches!(
                            txn.phase,
                            Phase::Applying | Phase::RecoveryBlocked | Phase::Published | Phase::Reverting
                        )
                }) else {
                    return;
                };
                if txn.phase == Phase::Published {
                    me.settle_if_due(&txn).await;
                    continue;
                }
                // A worker the service did not get to note down is found in the journal.
                let worker = txn.worker.or_else(|| {
                    crate::journal::read(&me.paths.txn_dir(&id))
                        .ok()
                        .and_then(|entries| crate::journal::worker(&entries))
                });
                if worker.is_some_and(sys::is_running) || platform::worker_scope_active(&id) {
                    let overdue = chrono::DateTime::parse_from_rfc3339(&txn.started_at)
                        .is_ok_and(|started| chrono::Local::now().fixed_offset() - started > TXN_DEADLINE);
                    // Stopping is asked again every half minute until the worker is gone.
                    if overdue && stop_requested.is_none_or(|at| at.elapsed() > Duration::from_secs(30)) {
                        if stop_requested.is_none() {
                            logger::log(&format!("transaction {id}: the worker overran, stopping it"));
                            let record = txn::txn_record(&txn, "worker_stop_requested");
                            if let Err(error) = me.audit.write(Stream::Audit, record) {
                                logger::log(&format!("audit: {error:#}"));
                            }
                        }
                        stop_requested = Some(Instant::now());
                        platform::stop_worker(&txn, worker);
                    }
                    continue;
                }
                // Recovered on a copy, so the service keeps answering while it waits; the
                // transaction stays open until the copy is put back.
                let worker = Arc::clone(&me);
                let recovered = tokio::task::spawn_blocking(move || {
                    let mut state = state;
                    let result = txn::recover(&worker.paths, &worker.audit, &mut state, Duration::from_secs(30));
                    (state, result)
                })
                .await;
                let finished = match recovered {
                    Ok((state, result)) => {
                        let mut inner = me.inner.lock().await;
                        if inner.state.txn.as_ref().is_some_and(|current| current.id == id) {
                            inner.state = state;
                        }
                        drop(inner);
                        Ok(result)
                    }
                    Err(error) => Err(error),
                };
                match finished {
                    Ok(Ok(Some(finished))) => {
                        if txn.relaunch && txn.kind == Kind::Full {
                            me.relaunch(&txn);
                        }
                        logger::log(&format!("transaction {}: {:?}", finished.id, finished.outcome));
                        return;
                    }
                    Ok(Ok(None)) => {}
                    // The transaction stays open; finishing it is tried again.
                    Ok(Err(error)) => {
                        logger::log(&format!("transaction {id}: {error:#}"));
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    Err(error) => {
                        logger::log(&format!("transaction {id}: {error}"));
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
            }
        });
    }

    /// Keeps a published core upgrade the GUI did not answer for in time, and ends one decided to be
    /// kept whose end did not reach the disk.
    async fn settle_if_due(&self, txn: &Txn) {
        let due = txn.decision.is_some()
            || txn
                .confirm_by
                .as_deref()
                .and_then(|by| chrono::DateTime::parse_from_rfc3339(by).ok())
                .is_none_or(|by| chrono::Local::now().fixed_offset() >= by);
        if !due {
            return;
        }
        let mut inner = self.inner.lock().await;
        let settled = tokio::task::block_in_place(|| -> Result<()> {
            // The answer may have come meanwhile.
            let Some(current) = inner
                .state
                .txn
                .clone()
                .filter(|current| current.id == txn.id && current.phase == Phase::Published)
            else {
                return Ok(());
            };
            if current.decision.is_none() {
                txn::decide(&self.paths, &self.audit, &mut inner.state, None)?;
            }
            txn::keep(&self.paths, &self.audit, &mut inner.state).map(drop)
        });
        drop(inner);
        if let Err(error) = settled {
            logger::log(&format!("transaction {}: {error:#}", txn.id));
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// The GUI tells whether the core a published upgrade put in place runs. The first answer
    /// decides; one that comes again or late gets how the upgrade stands.
    async fn confirm(self: &Arc<Self>, id: &str, ok: bool, peer: &Initiator) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        let Some(txn) = inner.state.txn.clone().filter(|txn| txn.id == id) else {
            drop(inner);
            return Ok(Response::Txn(self.view(id).await));
        };
        if txn.initiator.id != peer.id {
            return Ok(self.reject(peer, "only the user who started the upgrade confirms it".into()));
        }
        if txn.phase != Phase::Published || txn.decision.is_some() {
            return Ok(Response::Txn(TxnView::Running(txn)));
        }
        let decision =
            tokio::task::block_in_place(|| txn::decide(&self.paths, &self.audit, &mut inner.state, Some(ok)))?;
        if decision == Decision::Keep {
            // An end that did not reach the disk is retried by the supervisor.
            let finished = tokio::task::block_in_place(|| txn::keep(&self.paths, &self.audit, &mut inner.state));
            return Ok(Response::Txn(match finished {
                Ok(finished) => TxnView::Finished(finished),
                Err(error) => {
                    logger::log(&format!("transaction {id}: {error:#}"));
                    inner.state.txn.clone().map_or(TxnView::Unknown, TxnView::Running)
                }
            }));
        }
        // The decision is on disk; the supervisor sees the worker that undoes the upgrade once the
        // state is released, by which time it is registered.
        let worker = tokio::task::block_in_place(|| platform::spawn_worker(&self.paths, id));
        if let (Ok(worker), Some(current)) = (&worker, inner.state.txn.as_mut()) {
            current.worker = Some(*worker);
            if let Err(error) = inner.state.save(&self.paths.state_file()) {
                logger::log(&format!("transaction {id}: {error:#}"));
            }
        }
        let view = inner.state.txn.clone().map_or(TxnView::Unknown, TxnView::Running);
        drop(inner);
        self.supervise(id.to_owned());
        worker.context("the worker undoing the upgrade did not confirm its start")?;
        Ok(Response::Txn(view))
    }

    fn relaunch(&self, txn: &Txn) {
        let event = match platform::relaunch_gui(&txn.initiator) {
            Ok(true) => "relaunched",
            Ok(false) => "relaunch_skipped",
            Err(error) => {
                logger::log(&format!("relaunch: {error:#}"));
                "relaunch_failed"
            }
        };
        if let Err(error) = self.audit.write(Stream::Audit, txn::txn_record(txn, event)) {
            logger::log(&format!("audit: {error:#}"));
        }
    }

    async fn cancel(&self, id: &str, peer: &Initiator) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        if inner
            .staged
            .take_if(|staged| staged.txn == id && staged.initiator.id == peer.id)
            .is_some()
        {
            drop(inner);
            self.audit
                .write(Stream::Audit, Record::new(Source::Actual, "cancelled").txn(id))?;
            return Ok(Response::Done);
        }
        let pending = inner
            .state
            .txn
            .as_ref()
            .is_some_and(|txn| txn.id == id && txn.initiator.id == peer.id && txn.phase == Phase::UserPhasePending);
        if !pending {
            return Ok(Response::Rejected {
                reason: "nothing of this user to cancel".into(),
            });
        }
        tokio::task::block_in_place(|| {
            end_user_phase(&self.paths, &self.audit, &mut inner.state, Outcome::Failed, "cancelled")
        })?;
        drop(inner);
        Ok(Response::Done)
    }

    /// macOS: the GUI reports the bundle phase. Its results are the client's word, recorded as such;
    /// only the privileged phase that follows a replaced bundle changes anything as root.
    async fn user_phase(self: &Arc<Self>, id: &str, event: UserPhaseEvent, peer: &Initiator) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        let Some(txn) = inner
            .state
            .txn
            .clone()
            .filter(|txn| txn.id == id && txn.initiator.id == peer.id)
        else {
            return Ok(Response::Rejected {
                reason: "no such transaction of this user".into(),
            });
        };
        // A begin repeated after its answer was lost is confirmed again.
        if txn.phase == Phase::UserPhaseBegun && event == UserPhaseEvent::Begin {
            return Ok(Response::Done);
        }
        let valid = matches!(
            (txn.phase, event),
            (Phase::UserPhasePending, UserPhaseEvent::Begin)
                | (
                    Phase::UserPhasePending | Phase::UserPhaseBegun,
                    UserPhaseEvent::Replaced | UserPhaseEvent::NotReplaced | UserPhaseEvent::NotRestored
                )
        );
        if !valid {
            return Ok(self.reject(peer, "the transaction is not in its user phase".into()));
        }
        let mut record = txn::txn_record(&txn, "user_phase");
        record.source = Source::Claimed;
        record.detail = Some(serde_json::json!({ "event": event }));
        self.audit.write(Stream::Audit, record)?;
        match (txn.phase, event) {
            (Phase::UserPhasePending, UserPhaseEvent::Begin) => {
                let mut next = inner.state.clone();
                if let Some(current) = next.txn.as_mut() {
                    current.phase = Phase::UserPhaseBegun;
                }
                next.save(&self.paths.state_file())?;
                inner.state = next;
            }
            (Phase::UserPhasePending | Phase::UserPhaseBegun, UserPhaseEvent::Replaced) => {
                // The worker reads the phase from disk, so it is persisted before the worker starts.
                let mut next = inner.state.clone();
                if let Some(current) = next.txn.as_mut() {
                    current.phase = Phase::Applying;
                    // The watchdog counts from here: the user phase before may have waited long.
                    current.started_at = audit::now();
                }
                next.save(&self.paths.state_file())?;
                inner.state = next;
                let worker = tokio::task::block_in_place(|| platform::spawn_worker(&self.paths, id));
                if let (Ok(worker), Some(current)) = (&worker, inner.state.txn.as_mut()) {
                    current.worker = Some(*worker);
                    // The worker runs either way; supervision finds it through its scope or journal.
                    if let Err(error) = inner.state.save(&self.paths.state_file()) {
                        logger::log(&format!("transaction {id}: {error:#}"));
                    }
                }
                drop(inner);
                // Once the phase is on disk the transaction is watched to its end, whatever the
                // worker's start returned.
                self.supervise(id.to_owned());
                worker.context("the update worker did not confirm its start")?;
            }
            (Phase::UserPhasePending | Phase::UserPhaseBegun, UserPhaseEvent::NotReplaced) => {
                tokio::task::block_in_place(|| {
                    end_user_phase(
                        &self.paths,
                        &self.audit,
                        &mut inner.state,
                        Outcome::Failed,
                        "the app was not replaced",
                    )
                })?;
            }
            (Phase::UserPhasePending | Phase::UserPhaseBegun, UserPhaseEvent::NotRestored) => {
                tokio::task::block_in_place(|| {
                    end_user_phase(
                        &self.paths,
                        &self.audit,
                        &mut inner.state,
                        Outcome::NeedsRepair,
                        "the app was neither replaced nor restored; reinstall it",
                    )
                })?;
            }
            _ => unreachable!("checked above"),
        }
        Ok(Response::Done)
    }

    /// Ends a user phase an administrator gives up; the state is this service's, so it is ended
    /// here rather than on disk behind its back.
    async fn abandon(&self, id: &str, peer: &Initiator) -> Result<Response> {
        if peer.id != "0" {
            return Ok(self.reject(peer, "only root can abandon an update".into()));
        }
        let mut inner = self.inner.lock().await;
        let in_user_phase =
            inner.state.txn.as_ref().is_some_and(|txn| {
                txn.id == id && matches!(txn.phase, Phase::UserPhasePending | Phase::UserPhaseBegun)
            });
        if !in_user_phase {
            return Ok(Response::Rejected {
                reason: format!("{id} is not an update waiting on its user phase"),
            });
        }
        let ended = tokio::task::block_in_place(|| {
            end_user_phase(
                &self.paths,
                &self.audit,
                &mut inner.state,
                Outcome::Abandoned,
                ABANDONED,
            )
        });
        drop(inner);
        ended?;
        Ok(Response::Done)
    }

    /// Re-applies the version of a transaction that needs repair, from its kept payload.
    async fn repair(self: &Arc<Self>, id: Option<&str>, relaunch: bool, peer: &Initiator) -> Result<Response> {
        let mut inner = self.inner.lock().await;
        drop_expired(&mut inner);
        // A newer installation since then gives the repair up.
        tokio::task::block_in_place(|| txn::observe(&self.paths, &self.audit, &mut inner.state))?;
        let Some(pending) = inner
            .state
            .needs_repair
            .clone()
            .filter(|pending| id.is_none_or(|id| pending.id == id))
        else {
            return Ok(Response::Rejected {
                reason: "nothing needs repair".into(),
            });
        };
        if let Some(busy) = busy_view(&inner) {
            return Ok(Response::Busy { current: busy });
        }
        let paths = self.paths.clone();
        let version = pending.version.clone();
        let payload = tokio::task::spawn_blocking(move || txn::load_payload(&paths, &version)).await??;
        let signatures = serde_json::from_slice(&std::fs::read(
            self.paths.payloads_dir().join(&pending.version).join("signatures.json"),
        )?)?;
        let staged = Staged {
            txn: new_txn_id(),
            payload: Arc::new(payload),
            signatures,
            initiator: peer.clone(),
            trigger: Trigger {
                kind: "repair".into(),
                client_version: String::new(),
            },
            expires: Instant::now() + STAGED_LIFETIME,
            repair: true,
        };
        let ready = tokio::task::block_in_place(|| {
            txn::admit(&inner.state, &staged.payload.manifest, true)
                .and_then(|()| platform::check_ready(&staged.payload.manifest))
        });
        if let Err(error) = ready {
            return Ok(self.reject(peer, format!("{error:#}")));
        }
        let relaunch = relaunch && staged.payload.manifest.kind == Kind::Full;
        let begun = tokio::task::block_in_place(|| self.begin(&mut inner.state, &staged, relaunch));
        let started = applying(&inner.state, &staged.txn);
        drop(inner);
        if started {
            self.supervise(staged.txn);
        }
        Ok(Response::Txn(TxnView::Running(begun?)))
    }
}

pub const ABANDONED: &str = "given up by an administrator; the user phase result is unknown";

/// Ends a user phase that changed nothing as root with `outcome`.
pub fn end_user_phase(paths: &Paths, audit: &AuditLog, state: &mut State, outcome: Outcome, why: &str) -> Result<()> {
    let result = WorkerResult::new(outcome, Some(why.into()));
    txn::finish(paths, audit, state, result).map(drop)
}

/// A user phase never begun within a day is given up; nothing was changed.
fn expire_user_phase(paths: &Paths, audit: &AuditLog, state: &mut State) -> Result<()> {
    let stale = state.txn.as_ref().is_some_and(|txn| {
        txn.phase == Phase::UserPhasePending
            && chrono::DateTime::parse_from_rfc3339(&txn.started_at)
                .is_ok_and(|started| chrono::Local::now().fixed_offset() - started > chrono::Duration::hours(24))
    });
    if stale {
        end_user_phase(
            paths,
            audit,
            state,
            Outcome::Failed,
            "the app was not replaced within a day",
        )?;
    }
    Ok(())
}

/// The transaction `id` is persisted in its privileged phase.
fn applying(state: &State, id: &str) -> bool {
    state
        .txn
        .as_ref()
        .is_some_and(|txn| txn.id == id && txn.phase == Phase::Applying)
}

fn drop_expired(inner: &mut Inner) {
    let _ = inner.staged.take_if(|staged| staged.expires <= Instant::now());
}

fn busy_view(inner: &Inner) -> Option<TxnView> {
    if let Some(txn) = inner.state.txn.as_ref() {
        return Some(TxnView::Running(txn.clone()));
    }
    inner.staged.as_ref().map(|staged| TxnView::Staged(staged.view()))
}

/// Busy for staging unless nothing runs and any staged update is this user's own: a new one
/// replaces it, so an answer lost on the way does not lock the user out until it expires.
fn busy_for(inner: &Inner, peer: &Initiator) -> Option<TxnView> {
    if let Some(txn) = inner.state.txn.as_ref() {
        return Some(TxnView::Running(txn.clone()));
    }
    inner
        .staged
        .as_ref()
        .filter(|staged| staged.initiator.id != peer.id)
        .map(|staged| TxnView::Staged(staged.view()))
}
