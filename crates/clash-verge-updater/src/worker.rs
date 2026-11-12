//! The worker: applies one transaction while holding the update lock, then leaves its result for
//! the service. It runs from a copy of the program in the transaction directory, so the resident
//! program can be replaced while it works.

use crate::journal::{self, Journal};
use crate::txn::{self, WorkerResult};
use crate::{lock, logger, platform};
use anyhow::{Context as _, Result, ensure};
use clash_verge_updater::paths::Paths;
use clash_verge_updater::state::{Outcome, Phase, State};
use clash_verge_updater::sys;
use std::time::Duration;

pub fn run(id: &str) -> Result<()> {
    let paths = Paths::system();
    let state = State::load(&paths.state_file())?;
    let txn = state
        .txn
        .filter(|txn| txn.id == id && txn.phase == Phase::Applying)
        .context("no such transaction in progress")?;
    let dir = paths.txn_dir(id);
    let mut journal = Journal::open(&dir)?;
    journal.writer(u32::MAX, sys::current_process())?;
    logger::log(&format!("worker {id}: applying {} ({:?})", txn.version, txn.kind));

    let _lock = lock::acquire(&paths, Duration::from_secs(300))?;
    // An installer that ran while the lock was awaited leaves its own state; this update steps
    // back before writing anything, so recovery never mistakes its writes for that installer's.
    if txn::externally_installed(&paths, &txn) {
        let result = WorkerResult {
            outcome: Outcome::Superseded,
            error: Some("another installation ran before the update started".into()),
            components: Default::default(),
        };
        logger::log(&format!("worker {id}: {:?}", result.outcome));
        return txn::write_result(&paths, id, &result);
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
        let payload = txn::load_payload(&paths, &txn.version)?;
        ensure!(payload.sha256 == txn.payload_sha256, "the kept payload changed");
        txn::admit(&state, &payload.manifest, txn.repair)?;
        platform::apply(&paths, &txn, &payload, &mut journal)
    })();

    let result = match applied {
        Ok(components) => WorkerResult {
            outcome: Outcome::Applied,
            error: None,
            components,
        },
        // Leaves without a result: the service waits for the writers, then undoes.
        Err(error) if txn::writers_running(&error) => return Err(error),
        Err(error) => {
            logger::log(&format!("worker {id}: failed: {error:#}"));
            let (outcome, undo_error) = match journal::read(&dir) {
                Ok(entries) => txn::rollback(&paths, &txn, &entries)?,
                Err(read) => (Outcome::NeedsRepair, Some(format!("{read:#}"))),
            };
            WorkerResult {
                outcome,
                error: Some(match undo_error {
                    Some(undo) => format!("{error:#}; undoing: {undo}"),
                    None => format!("{error:#}"),
                }),
                components: Default::default(),
            }
        }
    };
    logger::log(&format!("worker {id}: {:?}", result.outcome));
    txn::write_result(&paths, id, &result)
}
