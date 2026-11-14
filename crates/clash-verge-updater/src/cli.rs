//! Administrative commands: reading the audit log, repair and recovery, and the records installers
//! leave when they run without the component.

use crate::txn;
use anyhow::{Context as _, Result, bail};
use clash_verge_updater::audit::{self, AuditLog, Record, Source, Stream};
use clash_verge_updater::paths::Paths;
use clash_verge_updater::protocol::{PROTOCOL, Request, Response};
use clash_verge_updater::state::{Decision, Outcome, Phase, State};
use std::io::Write as _;
use std::time::Duration;

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

/// `log [-n N] [--since <RFC 3339 time>] [--json]`: newest first.
pub fn log(args: &[String]) -> Result<()> {
    let limit = option(args, "-n").map_or(Ok(50), str::parse)?;
    let since = option(args, "--since")
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .context("--since takes an RFC 3339 time")?;
    let json = args.iter().any(|arg| arg == "--json");
    let records = audit::read_page(&Paths::system().logs_dir(), None, usize::MAX);
    let mut out = std::io::stdout().lock();
    for record in records
        .into_iter()
        .filter(|record| {
            since.is_none_or(|since| chrono::DateTime::parse_from_rfc3339(&record.time).is_ok_and(|time| time >= since))
        })
        .take(limit)
    {
        if json {
            writeln!(out, "{}", serde_json::to_string(&record)?)?;
            continue;
        }
        let who = record
            .trigger
            .as_ref()
            .map(|trigger| format!(" by {} ({})", trigger.user, trigger.kind))
            .unwrap_or_default();
        let versions = match (&record.from_version, &record.to_version) {
            (Some(from), Some(to)) => format!(" {from} -> {to}"),
            (None, Some(to)) => format!(" -> {to}"),
            _ => String::new(),
        };
        let error = record
            .error
            .as_deref()
            .map(|e| format!(" error: {e}"))
            .unwrap_or_default();
        writeln!(
            out,
            "{} #{} [{:?}] {}{versions}{who}{error}",
            record.time, record.seq, record.source, record.event
        )?;
    }
    Ok(())
}

fn ask(request: &Request) -> Result<Response> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut connection = clash_verge_updater::client::Connection::connect().await?;
        connection.request(request).await
    })
}

/// `repair [txn]`: asks the running component to re-apply a version that needs repair. On macOS
/// only the app of that version can, once it is the bundle in place: it repairs at its start.
pub fn repair(args: &[String]) -> Result<()> {
    if cfg!(target_os = "macos") {
        bail!("on macOS, start the app of the version that needs repair; it repairs itself");
    }
    let response = ask(&Request::Repair {
        txn: args.first().cloned(),
        relaunch: false,
    })
    .context("the update component is not running; use `recover`")?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

/// `recover`: run by an administrator, typically from `clash-verge-updater.prev` or a transaction
/// copy when the resident component cannot start. Finishes an interrupted transaction once its
/// writers are gone, then starts the component, installing this program only if it does not run.
pub fn recover() -> Result<()> {
    // The running component owns the state and finishes interrupted transactions itself.
    let running = "the update component is running and recovers by itself; stop it to recover by hand";
    if hello().is_ok() {
        bail!(running);
    }
    let paths = Paths::system();
    // A worker replacing the component holds the lock while the new one is not answering yet;
    // installers replace it under the same lock.
    let _lock = crate::lock::acquire(&paths, Duration::from_secs(60))?;
    if hello().is_ok() {
        bail!(running);
    }
    // Not answering is not gone: one still running owns the state, so it is stopped first.
    crate::platform::stop_resident()?;
    let recovered = (|| -> Result<()> {
        let audit = AuditLog::open(&paths.logs_dir())?;
        let mut state = State::load(&paths.state_file())?;
        // A published core upgrade still waits for its GUI; the component settles it once it runs.
        if let Some(txn) = state.txn.clone()
            && txn.phase != Phase::UserPhasePending
            && txn.phase != Phase::UserPhaseBegun
            && !(txn.phase == Phase::Published && txn.decision.is_none())
        {
            if let Some(current) = state.txn.as_mut()
                && current.phase == Phase::RecoveryBlocked
            {
                current.phase = if current.decision == Some(Decision::Revert) {
                    Phase::Reverting
                } else {
                    Phase::Applying
                };
            }
            match txn::recover(&paths, &audit, &mut state, Duration::from_secs(60))? {
                Some(finished) => println!("transaction {}: {:?}", finished.id, finished.outcome),
                None => bail!("the writers of transaction {} are still running", txn.id),
            }
        }
        Ok(())
    })();
    if let Err(error) = recovered {
        // The component goes back to watching what could not be finished here.
        if let Err(start) = crate::platform::start_resident() {
            println!("the update component does not start either: {start:#}");
        }
        return Err(error);
    }
    if let Err(error) = crate::platform::start_resident() {
        println!("the update component does not start ({error:#}); installing this program");
        crate::platform::install_self()?;
    }
    println!("the update component is running");
    Ok(())
}

/// `abandon <txn>`: gives up a macOS user phase whose initiator will not come back. The running
/// component owns the state and ends it itself; only when it is not running is the state changed
/// here.
pub fn abandon(args: &[String]) -> Result<()> {
    let id = args.first().context("abandon takes a transaction id")?;
    if hello().is_ok() {
        return match ask(&Request::Abandon { txn: id.clone() })? {
            Response::Done => Ok(()),
            Response::Rejected { reason } | Response::Error { message: reason } => bail!("{reason}"),
            other => bail!("unexpected answer {other:?}"),
        };
    }
    let paths = Paths::system();
    let audit = AuditLog::open(&paths.logs_dir())?;
    let _lock = crate::lock::acquire(&paths, Duration::from_secs(60))?;
    // Not answering is not gone: one still running owns the state, so it is stopped first and
    // started again afterwards.
    let was_running = crate::platform::stop_resident()?;
    let ended = (|| -> Result<()> {
        let mut state = State::load(&paths.state_file())?;
        let in_user_phase = state
            .txn
            .as_ref()
            .is_some_and(|txn| &txn.id == id && matches!(txn.phase, Phase::UserPhasePending | Phase::UserPhaseBegun));
        if !in_user_phase {
            bail!("{id} is not a transaction waiting on its user phase");
        }
        crate::daemon::end_user_phase(&paths, &audit, &mut state, Outcome::Abandoned, crate::daemon::ABANDONED)
    })();
    let started = if was_running {
        crate::platform::start_resident()
    } else {
        Ok(())
    };
    ended.and(started)
}

/// `record-install --source <nsis|deb> --from <v> --to <v> --result <r>`: an installer that ran
/// without the component records itself.
pub fn record_install(args: &[String]) -> Result<()> {
    let paths = Paths::system();
    let audit = AuditLog::open(&paths.logs_dir())?;
    let mut record = Record::new(Source::Installer, "install")
        .versions(option(args, "--from").filter(|v| !v.is_empty()), option(args, "--to"))
        .result(option(args, "--result").unwrap_or("unknown"));
    record.detail = Some(serde_json::json!({ "source": option(args, "--source") }));
    audit.write(Stream::Audit, record).map(drop)
}

fn hello() -> Result<Response> {
    ask(&Request::Hello { protocol: PROTOCOL })
}
