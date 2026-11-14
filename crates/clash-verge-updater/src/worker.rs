//! The worker: applies one transaction while holding the update lock, then leaves its result for
//! the service. It runs from a copy of the program in the transaction directory, so the resident
//! program can be replaced while it works. A core upgrade decided to be undone gets a worker of
//! its own for that.

use crate::journal::{self, Journal, Mark};
use crate::txn::{self, WorkerResult};
use crate::{lock, logger, platform};
use anyhow::{Context as _, Result, ensure};
use clash_verge_updater::paths::Paths;
use clash_verge_updater::state::{Outcome, Phase, State};
use clash_verge_updater::sys;
use std::path::Path;
use std::time::Duration;

pub fn run(id: &str) -> Result<()> {
    let paths = Paths::system();
    let state = State::load(&paths.state_file())?;
    let txn = state
        .txn
        .filter(|txn| txn.id == id && matches!(txn.phase, Phase::Applying | Phase::Reverting))
        .context("no such transaction in progress")?;
    let reverting = txn.phase == Phase::Reverting;
    let dir = paths.txn_dir(id);
    let mut journal = Journal::open(&dir)?;
    journal.writer(u32::MAX, sys::current_process())?;
    logger::log(&format!(
        "worker {id}: {} {} ({:?})",
        if reverting { "undoing" } else { "applying" },
        txn.version,
        txn.kind
    ));

    let _lock = lock::acquire(&paths, Duration::from_secs(300))?;
    // An installer that ran while the lock was awaited leaves its own state; this update steps
    // back before writing anything, so recovery never mistakes its writes for that installer's.
    if txn::externally_installed(&paths, &txn) {
        let result = WorkerResult::new(
            Outcome::Superseded,
            Some("another installation ran before the update started".into()),
        );
        logger::log(&format!("worker {id}: {:?}", result.outcome));
        return txn::write_result(&paths, id, reverting, &result);
    }
    if reverting {
        return revert(&paths, id, &txn, &dir);
    }
    let applied = (|| -> Result<_> {
        // An installer may have run while the lock was awaited: admit the update again under it.
        let state = State::load(&paths.state_file())?;
        ensure!(
            state
                .txn
                .as_ref()
                .is_some_and(|current| current.id == id && current.phase == Phase::Applying),
            "the transaction is no longer in progress"
        );
        let payload = txn::load_txn_payload(&paths, &txn)?;
        ensure!(payload.sha256 == txn.payload_sha256, "the kept payload changed");
        txn::admit(&state, &payload.manifest, txn.repair)?;
        platform::apply(&paths, &txn, &payload, &mut journal)
    })();

    let result = match applied {
        Ok(components) => WorkerResult {
            outcome: Outcome::Applied,
            error: None,
            components,
            wrote: wrote(&dir),
        },
        // Leaves without a result: the service waits for the writers, then undoes.
        Err(error) if txn::writers_running(&error) => return Err(error),
        Err(error) => {
            logger::log(&format!("worker {id}: failed: {error:#}"));
            let (outcome, undo_error) = match journal::read(&dir) {
                Ok(entries) => txn::rollback(&paths, &txn, &entries)?,
                Err(read) => (Outcome::NeedsRepair, Some(format!("{read:#}"))),
            };
            WorkerResult::new(
                outcome,
                Some(match undo_error {
                    Some(undo) => format!("{error:#}; undoing: {undo}"),
                    None => format!("{error:#}"),
                }),
            )
        }
    };
    logger::log(&format!("worker {id}: {:?}", result.outcome));
    txn::write_result(&paths, id, false, &result)
}

/// Whether the transaction recorded any change it set out to make.
fn wrote(dir: &Path) -> bool {
    journal::read(dir).map_or(true, |entries| entries.iter().any(|entry| entry.mark == Mark::Intent))
}

/// Puts back what a published core upgrade changed, from what its journal recorded.
fn revert(paths: &Paths, id: &str, txn: &clash_verge_updater::state::Txn, dir: &Path) -> Result<()> {
    let undone = (|| -> Result<_> {
        let state = State::load(&paths.state_file())?;
        ensure!(
            state
                .txn
                .as_ref()
                .is_some_and(|current| current.id == id && current.phase == Phase::Reverting),
            "the transaction is no longer being undone"
        );
        txn::rollback(paths, txn, &journal::read(dir)?)
    })();
    let result = match undone {
        // Nothing left to undo means the previous cores are in place again.
        Ok((Outcome::Failed, error)) => WorkerResult::new(Outcome::RolledBack, error),
        Ok((outcome, error)) => WorkerResult::new(outcome, error),
        Err(error) if txn::writers_running(&error) => return Err(error),
        Err(error) => WorkerResult::new(Outcome::NeedsRepair, Some(format!("{error:#}"))),
    };
    logger::log(&format!("worker {id}: {:?}", result.outcome));
    txn::write_result(paths, id, true, &result)
}
