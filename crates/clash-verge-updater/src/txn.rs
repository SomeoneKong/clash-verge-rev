//! Ending and recovering transactions, shared by the service, the worker and the recovery command.

use crate::journal::{self, Entry};
use crate::platform::{self, Undo};
use anyhow::{Context as _, Result};
use clash_verge_updater::audit::{AuditLog, Components, PayloadInfo, Record, Source, Stream, TriggerInfo};
use clash_verge_updater::manifest::{Kind, Manifest, Platform};
use clash_verge_updater::paths::Paths;
use clash_verge_updater::payload::Payload;
use clash_verge_updater::state::{Decision, Finished, Outcome, Phase, State, Txn, write_atomic};
use clash_verge_updater::{sys, version};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a published core upgrade waits for the GUI before it is kept.
pub const CONFIRM_WINDOW: chrono::Duration = chrono::Duration::minutes(10);

/// Writers of a transaction may still be running: nothing may be undone, checked or finished
/// until they are gone.
#[derive(Debug)]
pub struct WritersRunning;

impl std::fmt::Display for WritersRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("writers of the transaction may still be running")
    }
}

impl std::error::Error for WritersRunning {}

pub fn writers_running(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<WritersRunning>())
}

/// What the worker leaves for the service to finish the transaction with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub outcome: Outcome,
    pub error: Option<String>,
    #[serde(default)]
    pub components: Components,
    /// Whether anything was written. A core upgrade that found every file in place already ends
    /// without waiting for the GUI.
    #[serde(default = "wrote_by_default")]
    pub wrote: bool,
}

const fn wrote_by_default() -> bool {
    true
}

impl WorkerResult {
    pub fn new(outcome: Outcome, error: Option<String>) -> Self {
        Self {
            outcome,
            error,
            components: Components::default(),
            wrote: true,
        }
    }
}

/// The publishing worker and the one undoing a core upgrade leave their results apart.
fn result_path(paths: &Paths, id: &str, reverting: bool) -> PathBuf {
    paths
        .txn_dir(id)
        .join(if reverting { "revert-result.json" } else { "result.json" })
}

pub fn write_result(paths: &Paths, id: &str, reverting: bool, result: &WorkerResult) -> Result<()> {
    write_atomic(&result_path(paths, id, reverting), &serde_json::to_vec_pretty(result)?)
}

pub fn read_result(paths: &Paths, id: &str, reverting: bool) -> Option<WorkerResult> {
    serde_json::from_slice(&std::fs::read(result_path(paths, id, reverting)).ok()?).ok()
}

fn payload_dir(paths: &Paths, version: &str) -> PathBuf {
    paths.payloads_dir().join(version)
}

/// Keeps the payload of a transaction before anything changes, so it can be repaired offline. An
/// app payload is kept by version; a core payload only with its transaction.
pub fn save_payload(paths: &Paths, txn: &str, payload: &Payload, signatures: &[String]) -> Result<()> {
    let dir = if payload.manifest.kind == Kind::Core {
        paths.txn_dir(txn)
    } else {
        payload_dir(paths, &payload.manifest.version)
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    clash_verge_updater::state::sync_parent(&dir)?;
    write_atomic(&dir.join("signatures.json"), &serde_json::to_vec(signatures)?)?;
    write_atomic(&dir.join("payload.zip"), payload.bytes())
}

/// Reads a kept payload back, verifying it again: what is on disk is never trusted unchecked.
fn load_from(dir: &Path) -> Result<Payload> {
    let signatures: Vec<String> = serde_json::from_slice(&std::fs::read(dir.join("signatures.json"))?)?;
    let bytes = std::fs::read(dir.join("payload.zip")).context("the payload of this version was not kept")?;
    Payload::open(bytes, &signatures)
}

/// The kept app payload of `version`.
pub fn load_payload(paths: &Paths, version: &str) -> Result<Payload> {
    load_from(&payload_dir(paths, version))
}

pub fn load_txn_payload(paths: &Paths, txn: &Txn) -> Result<Payload> {
    if txn.kind == Kind::Core {
        load_from(&paths.txn_dir(&txn.id))
    } else {
        load_payload(paths, &txn.version)
    }
}

/// Drops kept payloads no longer needed: those of the floor (the latest applied version), of the
/// version awaiting repair and of the running transaction stay.
fn prune_payloads(paths: &Paths, state: &State) {
    let keep: Vec<&str> = [
        state.floor.as_deref(),
        state.needs_repair.as_ref().map(|pending| pending.version.as_str()),
        state.txn.as_ref().map(|txn| txn.version.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let Ok(entries) = std::fs::read_dir(paths.payloads_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name.as_str()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

const KEPT_TXN_DIRS: usize = 20;

/// A finished transaction keeps its journal and result for diagnosis; its program copies,
/// extracted files and backups go. Only the newest transaction directories are kept.
fn prune_txn_dirs(paths: &Paths, finished: &str) {
    // `recover` may run from a transaction's program copy; that directory stays.
    let running = std::env::current_exe().ok();
    let in_use = |name: &str| running.as_ref().is_some_and(|exe| exe.starts_with(paths.txn_dir(name)));
    if !in_use(finished) {
        let dir = paths.txn_dir(finished);
        for sub in [
            "bin",
            "tools",
            "installer",
            "package",
            "approved-backup",
            "cores",
            "backup",
        ] {
            let _ = std::fs::remove_dir_all(dir.join(sub));
        }
        let _ = std::fs::remove_file(dir.join("payload.zip"));
    }
    let Ok(entries) = std::fs::read_dir(paths.root().join("txn")) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    // Identifiers begin with the time they were made.
    names.sort();
    let excess = names.len().saturating_sub(KEPT_TXN_DIRS);
    for name in names.into_iter().take(excess) {
        if !in_use(&name) {
            let _ = std::fs::remove_dir_all(paths.txn_dir(&name));
        }
    }
}

/// Whether a verified payload may be applied here and now: right platform, a newer version than
/// both the floor and what is installed (the same version only to repair), the expected layout.
pub fn admit(state: &State, manifest: &Manifest, repair: bool) -> Result<()> {
    anyhow::ensure!(
        Platform::current() == Some(manifest.platform),
        "the payload is for {}",
        manifest.platform.key()
    );
    // A core payload has its own floor; the same version may be published again, to retry or
    // repair, and nothing of the app bounds it.
    if manifest.kind == Kind::Core {
        if let Some(floor) = state.core_floor.as_deref() {
            anyhow::ensure!(
                version::compare(&manifest.version, floor)?.is_ge(),
                "core {} is older than {floor}",
                manifest.version
            );
        }
        return Ok(());
    }
    // The kept payload of a version awaiting repair is what repairs it; it is not replaced.
    if !repair
        && let Some(pending) = &state.needs_repair
        && pending.version == manifest.version
    {
        anyhow::bail!("v{} awaits repair; repair it instead", manifest.version);
    }
    let installed = platform::installed_version();
    for bound in [state.floor.as_deref(), installed.as_deref()].into_iter().flatten() {
        let order = version::compare(&manifest.version, bound)?;
        let same_for_repair = repair && order.is_eq();
        anyhow::ensure!(
            order.is_gt() || same_for_repair,
            "v{} is not newer than v{bound}",
            manifest.version
        );
    }
    if let Some(base) = manifest.base_fingerprint.as_deref() {
        anyhow::ensure!(
            platform::installed_fingerprint().as_deref() == Some(base),
            "the installed layout differs from what this patch expects"
        );
    }
    Ok(())
}

pub fn external_mark(paths: &Paths) -> u64 {
    std::fs::metadata(paths.external_installs()).map_or(0, |meta| meta.len())
}

/// True when an installer outside the component ran after the transaction started.
pub fn externally_installed(paths: &Paths, txn: &Txn) -> bool {
    external_mark(paths) > txn.external_mark
}

pub fn trigger_info(txn: &Txn) -> TriggerInfo {
    TriggerInfo {
        kind: txn.trigger.kind.clone(),
        user: txn.initiator.user.clone(),
        id: txn.initiator.id.clone(),
        pid: txn.initiator.pid,
        client_version: txn.trigger.client_version.clone(),
    }
}

pub fn txn_record(txn: &Txn, event: &str) -> Record {
    let mut record = Record::new(Source::Actual, event)
        .txn(&txn.id)
        .versions(txn.from_version.as_deref(), Some(&txn.version));
    record.trigger = Some(trigger_info(txn));
    record.payload = Some(PayloadInfo {
        kind: txn.kind,
        sha256: txn.payload_sha256.clone(),
        size: txn.payload_size,
        key_id: txn.key_id.clone(),
    });
    record
}

/// Ends the running transaction with `result`: an applied one must also pass the check of the
/// resulting state, then the floor rises. The marker is cleared last.
pub fn finish(paths: &Paths, audit: &AuditLog, state: &mut State, result: WorkerResult) -> Result<Finished> {
    let txn = state.txn.clone().context("no transaction to finish")?;
    let core = txn.kind == Kind::Core;
    let (mut outcome, mut error) = (result.outcome, result.error);
    if outcome == Outcome::Applied
        && let Err(check) = load_txn_payload(paths, &txn).and_then(|payload| {
            if core {
                platform::verify_core(&payload.manifest)
            } else {
                platform::verify(&payload.manifest)
            }
        })
    {
        outcome = if core && (externally_installed(paths, &txn) || changed_by_others(paths, &txn)) {
            Outcome::Superseded
        } else {
            Outcome::NeedsRepair
        };
        error = Some(format!("the installed state does not match: {check:#}"));
    }
    // A bundle is the new one already once its privileged phase starts, and that app is offered no
    // update to retry with: what failed there is left for a repair, with the payload kept.
    let privileged_bundle =
        txn.kind == Kind::Bundle && !matches!(txn.phase, Phase::UserPhasePending | Phase::UserPhaseBegun);
    if privileged_bundle && matches!(outcome, Outcome::Failed | Outcome::RolledBack) {
        outcome = Outcome::NeedsRepair;
    }

    let finished = Finished {
        id: txn.id.clone(),
        version: txn.version.clone(),
        from_version: txn.from_version.clone(),
        kind: txn.kind,
        outcome,
        finished_at: clash_verge_updater::audit::now(),
        error: error.clone(),
        initiator: txn.initiator.clone(),
    };

    let mut record = txn_record(&txn, outcome_event(outcome));
    record.components = Some(result.components);
    record.error = error;
    record.duration_ms = chrono::DateTime::parse_from_rfc3339(&txn.started_at)
        .ok()
        .and_then(|started| u64::try_from((chrono::Local::now().fixed_offset() - started).num_milliseconds()).ok());
    if txn.repair {
        record.detail = Some(serde_json::json!({ "repair": true }));
    }
    audit.write(Stream::Audit, record)?;

    // The state in memory changes only once the new one is on disk; until then the transaction
    // stays open and its end is tried again.
    let mut next = state.clone();
    if core {
        if outcome == Outcome::Applied {
            if next
                .core_floor
                .as_deref()
                .is_none_or(|floor| version::compare(&txn.version, floor).is_ok_and(|order| order.is_gt()))
            {
                next.core_floor = Some(txn.version.clone());
            }
            next.core_needs_repair = None;
        } else if outcome == Outcome::NeedsRepair {
            next.core_needs_repair = Some(finished.clone());
        }
    } else if outcome == Outcome::Applied {
        // The app's cores are published with it, so a core upgrade awaiting repair is moot.
        next.core_needs_repair = None;
        if next
            .floor
            .as_deref()
            .is_none_or(|floor| version::compare(&txn.version, floor).is_ok_and(|order| order.is_gt()))
        {
            next.floor = Some(txn.version.clone());
        }
        next.observed_version = Some(txn.version.clone());
        if next
            .needs_repair
            .as_ref()
            .is_some_and(|pending| version::compare(&pending.version, &txn.version).is_ok_and(|order| order.is_le()))
        {
            next.needs_repair = None;
        }
    } else if outcome == Outcome::NeedsRepair {
        next.needs_repair = Some(finished.clone());
    }
    next.last = Some(finished.clone());
    next.txn = None;
    next.save(&paths.state_file())?;
    *state = next;
    prune_payloads(paths, state);
    prune_txn_dirs(paths, &txn.id);
    Ok(finished)
}

/// A core upgrade whose worker published its files waits for the GUI to tell whether the new core
/// runs; one that found every file in place already ends here.
pub fn publish(paths: &Paths, audit: &AuditLog, state: &mut State, result: WorkerResult) -> Result<Option<Finished>> {
    if !result.wrote {
        return finish(paths, audit, state, result).map(Some);
    }
    let mut next = state.clone();
    let txn = next.txn.as_mut().context("no transaction to publish")?;
    txn.phase = Phase::Published;
    txn.confirm_by =
        Some((chrono::Local::now() + CONFIRM_WINDOW).to_rfc3339_opts(chrono::SecondsFormat::Millis, false));
    let mut record = txn_record(txn, "published");
    record.components = Some(result.components);
    audit.write(Stream::Audit, record)?;
    next.save(&paths.state_file())?;
    *state = next;
    Ok(None)
}

/// Settles a published core upgrade. The decision is on disk before anything acts on it, and
/// nothing changes it afterwards. `ok` is what the GUI said, if it answered in time.
pub fn decide(paths: &Paths, audit: &AuditLog, state: &mut State, ok: Option<bool>) -> Result<Decision> {
    let txn = state
        .txn
        .clone()
        .filter(|txn| txn.phase == Phase::Published && txn.decision.is_none())
        .context("no published core upgrade awaits a decision")?;
    // An answer past the deadline does not count: the upgrade is kept as it would have been.
    let overdue = txn
        .confirm_by
        .as_deref()
        .and_then(|by| chrono::DateTime::parse_from_rfc3339(by).ok())
        .is_none_or(|by| chrono::Local::now().fixed_offset() >= by);
    let ok = if overdue { None } else { ok };
    let decision = if ok == Some(false) {
        Decision::Revert
    } else {
        Decision::Keep
    };
    let mut next = state.clone();
    if let Some(current) = next.txn.as_mut() {
        current.decision = Some(decision);
        if decision == Decision::Revert {
            current.phase = Phase::Reverting;
            current.worker = None;
            // The watchdog counts from here: the wait for the GUI came before.
            current.started_at = clash_verge_updater::audit::now();
        }
    }
    next.save(&paths.state_file())?;
    *state = next;
    let mut record = match ok {
        Some(ok) => {
            let mut record = txn_record(&txn, "confirmed");
            record.source = Source::Claimed;
            record.detail = Some(serde_json::json!({ "ok": ok }));
            record
        }
        None => txn_record(&txn, "confirm_timeout")
            .detail(serde_json::json!({ "note": "kept as published; whether the new core runs was not confirmed" })),
    };
    record.result = Some(if decision == Decision::Keep { "keep" } else { "revert" }.into());
    audit.write(Stream::Audit, record)?;
    Ok(decision)
}

/// Ends a core upgrade decided to be kept, under the lock, once its files are checked to be the
/// payload's still. An installation that ran meanwhile supersedes it, whatever it left in place.
pub fn keep(paths: &Paths, audit: &AuditLog, state: &mut State) -> Result<Finished> {
    let _lock = if crate::lock::held() {
        None
    } else {
        Some(crate::lock::acquire(paths, Duration::from_secs(5))?)
    };
    let txn = state.txn.clone().context("no transaction to keep")?;
    let on_disk = State::load(&paths.state_file())?;
    anyhow::ensure!(
        on_disk.txn.as_ref().is_some_and(|current| current.id == txn.id),
        "the transaction changed meanwhile"
    );
    let result = if externally_installed(paths, &txn) {
        WorkerResult::new(
            Outcome::Superseded,
            Some("another installation ran during the update".into()),
        )
    } else {
        WorkerResult::new(Outcome::Applied, None)
    };
    finish(paths, audit, state, result)
}

const fn outcome_event(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Applied => "applied",
        Outcome::RolledBack => "rolled_back",
        Outcome::Failed => "failed",
        Outcome::NeedsRepair => "needs_repair",
        Outcome::Superseded => "superseded",
        Outcome::Abandoned => "abandoned",
    }
}

/// Whether something else changed a file a core upgrade wrote: it then belongs to that change.
fn changed_by_others(paths: &Paths, txn: &Txn) -> bool {
    journal::read(&paths.txn_dir(&txn.id)).is_ok_and(|entries| {
        journal::started_steps(&entries)
            .iter()
            .any(|(entry, _)| platform::foreign(entry).unwrap_or(false))
    })
}

/// Undoes what a transaction started, newest first. Anything changed by another installation since
/// is left alone, and the transaction is superseded instead. An error means writers may still be
/// running, so nothing can be concluded yet.
pub fn rollback(paths: &Paths, txn: &Txn, entries: &[Entry]) -> Result<(Outcome, Option<String>)> {
    if externally_installed(paths, txn) {
        return Ok((Outcome::Superseded, None));
    }
    let steps = journal::started_steps(entries);
    if steps.is_empty() {
        return Ok((Outcome::Failed, None));
    }
    // A core upgrade is undone as a whole or not at all: one file another installation changed
    // leaves every file as it is, so no copy is put back apart from the others.
    if txn.kind == Kind::Core {
        for (entry, _) in &steps {
            match platform::foreign(entry) {
                Ok(false) => {}
                Ok(true) => return Ok((Outcome::Superseded, None)),
                Err(error) => return Ok((Outcome::NeedsRepair, Some(format!("{}: {error:#}", entry.name)))),
            }
        }
    }
    let mut changed = false;
    for (entry, done) in steps {
        let writers_started = entries
            .iter()
            .any(|writer| writer.mark == journal::Mark::Writer && writer.step == entry.step);
        match platform::undo(paths, txn, &entry, done, writers_started) {
            Ok(Undo::Restored) => changed = true,
            Ok(Undo::Clean) => {}
            Ok(Undo::Superseded) => return Ok((Outcome::Superseded, None)),
            Err(error) if writers_running(&error) => return Err(error),
            Err(error) => return Ok((Outcome::NeedsRepair, Some(format!("{}: {error:#}", entry.name)))),
        }
    }
    Ok((if changed { Outcome::RolledBack } else { Outcome::Failed }, None))
}

/// Waits until every writer the journal registered has stopped. False if they did not in time.
pub fn wait_writers_stopped(paths: &Paths, txn: &Txn, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        // A journal that cannot be read may name writers still running.
        let running = journal::read(&paths.txn_dir(&txn.id)).map_or(true, |entries| {
            journal::writers(&entries).into_iter().any(sys::is_running)
        }) || txn.worker.is_some_and(sys::is_running)
            || platform::worker_scope_active(&txn.id);
        if !running {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Finishes an interrupted transaction once its writers are gone: from the worker's result if it
/// left one, otherwise by undoing what the journal shows was started.
pub fn recover(paths: &Paths, audit: &AuditLog, state: &mut State, timeout: Duration) -> Result<Option<Finished>> {
    let Some(txn) = state.txn.clone() else {
        return Ok(None);
    };
    match txn.phase {
        Phase::UserPhasePending | Phase::UserPhaseBegun => return Ok(None),
        // Nothing runs while the GUI is to answer; a decision to keep is carried out here.
        Phase::Published => {
            return if txn.decision == Some(Decision::Keep) {
                keep(paths, audit, state).map(Some)
            } else {
                Ok(None)
            };
        }
        _ => {}
    }
    if !wait_writers_stopped(paths, &txn, timeout) {
        if txn.phase != Phase::RecoveryBlocked {
            if let Some(current) = state.txn.as_mut() {
                current.phase = Phase::RecoveryBlocked;
            }
            state.save(&paths.state_file())?;
            audit.write(Stream::Audit, txn_record(&txn, "recovery_blocked"))?;
        }
        return Ok(None);
    }
    // Held briefly: the caller tries again later rather than keep answering on hold. `recover`
    // holds it already.
    let _lock = if crate::lock::held() {
        None
    } else {
        Some(crate::lock::acquire(paths, Duration::from_secs(5))?)
    };
    // Another process may have finished the transaction while the lock was awaited.
    let on_disk = State::load(&paths.state_file())?;
    if on_disk.txn.as_ref().is_none_or(|current| current.id != txn.id) {
        *state = on_disk;
        return Ok(None);
    }
    // Undoing a core upgrade goes on from wherever it stopped; the result of its publishing says
    // nothing about that.
    let reverting = txn.decision == Some(Decision::Revert);
    let mut result = if externally_installed(paths, &txn) {
        WorkerResult::new(
            Outcome::Superseded,
            Some("another installation ran during the update".into()),
        )
    } else if let Some(result) = read_result(paths, &txn.id, reverting) {
        result
    } else {
        let entries = journal::read(&paths.txn_dir(&txn.id))?;
        let (outcome, error) = rollback(paths, &txn, &entries)?;
        WorkerResult::new(
            outcome,
            Some(error.unwrap_or_else(|| "the worker stopped before finishing".into())),
        )
    };
    // Nothing left to undo means the previous cores are in place again.
    if reverting && result.outcome == Outcome::Failed {
        result.outcome = Outcome::RolledBack;
    }
    let mut record = txn_record(&txn, "reconciled");
    record.result = Some(outcome_event(result.outcome).into());
    audit.write(Stream::Audit, record)?;
    if txn.kind == Kind::Core && !reverting && result.outcome == Outcome::Applied {
        return publish(paths, audit, state, result);
    }
    finish(paths, audit, state, result).map(Some)
}

/// Records a version change made outside the component and moves the floor with it. A version
/// awaiting repair that a newer installation has replaced is given up.
pub fn observe(paths: &Paths, audit: &AuditLog, current: &mut State) -> Result<()> {
    if current.txn.is_some() {
        return Ok(());
    }
    // Changes reach the state in memory only once they are on disk.
    let mut state = current.clone();
    let installed = platform::installed_version();
    let changed = if let (Some(pending), Some(now)) = (state.needs_repair.clone(), installed.as_deref())
        && version::compare(now, &pending.version).is_ok_and(std::cmp::Ordering::is_gt)
    {
        audit.write(
            Stream::Audit,
            Record::new(Source::Actual, "repair_superseded")
                .txn(&pending.id)
                .versions(Some(&pending.version), Some(now)),
        )?;
        state.needs_repair = None;
        true
    } else {
        false
    };
    if installed.is_none() || installed == state.observed_version {
        if changed {
            state.save(&paths.state_file())?;
            *current = state;
        }
        return Ok(());
    }
    let mut record = Record::new(Source::Observed, "observed_external_change")
        .versions(state.observed_version.as_deref(), installed.as_deref())
        .detail(serde_json::json!({ "note": "seen at this time; when, how and by whom it changed is unknown" }));
    record.result = Some("observed".into());
    audit.write(Stream::Audit, record)?;
    if let Some(installed) = installed.as_deref() {
        match state.floor.as_deref().map(|floor| version::compare(installed, floor)) {
            Some(Ok(order)) if order.is_lt() && platform::SYSTEM_VERSION_TRUSTED => {
                audit.write(
                    Stream::Audit,
                    Record::new(Source::Actual, "floor_reset").versions(state.floor.as_deref(), Some(installed)),
                )?;
                state.floor = Some(installed.to_owned());
            }
            Some(Ok(order)) if order.is_le() => {}
            _ if platform::SYSTEM_VERSION_TRUSTED => state.floor = Some(installed.to_owned()),
            _ => {}
        }
    }
    state.observed_version = installed;
    state.save(&paths.state_file())?;
    *current = state;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clash_verge_updater::manifest::{CoreEntry, FORMAT, FileEntry, Role};
    use clash_verge_updater::protocol::Trigger;
    use clash_verge_updater::state::Initiator;

    const D: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn core_manifest(version: &str) -> Manifest {
        let platform = Platform::current().unwrap();
        let core = |name: &str| CoreEntry {
            name: name.into(),
            version: version.into(),
            sha256: D.into(),
        };
        let program = if platform == Platform::WindowsX86_64 {
            "verge-mihomo.exe"
        } else {
            "verge-mihomo"
        };
        Manifest {
            format: FORMAT,
            version: version.into(),
            platform,
            kind: Kind::Core,
            base_fingerprint: None,
            fingerprint: None,
            files: vec![FileEntry {
                path: program.into(),
                sha256: D.into(),
                role: Role::Core,
            }],
            cores: vec![core("verge-mihomo"), core("verge-mihomo-alpha")],
            service: None,
            updater: None,
            bundle: None,
            commit: "c".into(),
            built_at: "2026-11-14T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_core_payload_is_bounded_by_the_core_floor_alone() {
        let mut state = State {
            floor: Some("9.0.0".into()),
            ..State::default()
        };
        admit(&state, &core_manifest("v1.19.34-custom.9"), false).unwrap();
        state.core_floor = Some("v1.19.34-custom.9".into());
        admit(&state, &core_manifest("v1.19.34-custom.9"), false).unwrap();
        admit(&state, &core_manifest("v1.19.34-custom.10"), false).unwrap();
        assert!(admit(&state, &core_manifest("v1.19.34-custom.8"), false).is_err());
    }

    fn published(dir: &Path) -> (Paths, AuditLog, State) {
        let paths = Paths::at(dir.to_path_buf());
        std::fs::create_dir_all(paths.root()).unwrap();
        let audit = AuditLog::open(&paths.logs_dir()).unwrap();
        let mut state = State {
            txn: Some(Txn {
                id: "t".into(),
                version: "v1.19.34-custom.9".into(),
                from_version: None,
                kind: Kind::Core,
                phase: Phase::Applying,
                initiator: Initiator {
                    user: "u".into(),
                    id: "1".into(),
                    pid: 1,
                    session: None,
                },
                trigger: Trigger {
                    kind: "manual".into(),
                    client_version: String::new(),
                },
                started_at: clash_verge_updater::audit::now(),
                payload_sha256: D.into(),
                payload_size: 0,
                key_id: "k".into(),
                relaunch: false,
                repair: false,
                external_mark: 0,
                worker: None,
                confirm_by: None,
                decision: None,
            }),
            ..State::default()
        };
        assert!(
            publish(&paths, &audit, &mut state, WorkerResult::new(Outcome::Applied, None))
                .unwrap()
                .is_none()
        );
        (paths, audit, state)
    }

    #[test]
    fn a_published_core_upgrade_waits_and_is_decided_once() {
        let dir = std::env::temp_dir().join(format!("clash-verge-updater-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (paths, audit, mut state) = published(&dir);
        let txn = state.txn.clone().unwrap();
        assert_eq!(txn.phase, Phase::Published);
        assert!(txn.confirm_by.is_some());
        assert_eq!(
            State::load(&paths.state_file()).unwrap().txn.unwrap().phase,
            Phase::Published
        );

        assert_eq!(
            decide(&paths, &audit, &mut state, Some(false)).unwrap(),
            Decision::Revert
        );
        let on_disk = State::load(&paths.state_file()).unwrap().txn.unwrap();
        assert_eq!(on_disk.phase, Phase::Reverting);
        assert_eq!(on_disk.decision, Some(Decision::Revert));
        assert!(
            decide(&paths, &audit, &mut state, Some(true)).is_err(),
            "a decision is final"
        );

        let (paths, audit, mut state) = published(&dir.join("timeout"));
        assert_eq!(decide(&paths, &audit, &mut state, None).unwrap(), Decision::Keep);
        let txn = state.txn.clone().unwrap();
        assert_eq!((txn.phase, txn.decision), (Phase::Published, Some(Decision::Keep)));

        let (paths, audit, mut state) = published(&dir.join("late"));
        if let Some(txn) = state.txn.as_mut() {
            txn.confirm_by = Some((chrono::Local::now() - chrono::Duration::seconds(1)).to_rfc3339());
        }
        assert_eq!(
            decide(&paths, &audit, &mut state, Some(false)).unwrap(),
            Decision::Keep,
            "an answer past the deadline does not undo the upgrade"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
