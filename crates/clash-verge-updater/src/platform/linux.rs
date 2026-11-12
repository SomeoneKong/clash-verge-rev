//! Linux: a systemd service behind a Unix socket. A `deb` payload is installed with dpkg while the
//! worker holds dpkg's frontend lock; the worker runs in its own transient unit, so restarting the
//! component during the install does not stop it, and the unit takes its writers down with it.

use super::Undo;
use super::unix::{config, group_id, invoking_gid, run_output, sha};
use crate::journal::{Entry, Journal};
use anyhow::{Context as _, Result, bail, ensure};
use clash_verge_updater::audit::{AuditLog, Components, CoreChange, Record, Source, Stream};
use clash_verge_updater::manifest::{Kind, Manifest, Role};
use clash_verge_updater::paths::{Paths, SYSTEMD_UNIT};
use clash_verge_updater::payload::Payload;
use clash_verge_updater::state::{Initiator, ProcessRef, Txn, write_atomic};
use clash_verge_updater::sys;
use serde_json::json;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub use super::unix::run_service;

/// dpkg's database is root's, so the installed version can move the floor.
pub const SYSTEM_VERSION_TRUSTED: bool = true;

const PACKAGE: &str = "clash-verge";
const SERVICE_ROOT: &str = "/var/lib/clash-verge-service";
const UNIT_FILE: &str = "/etc/systemd/system/clash-verge-updater.service";
const DPKG_TIMEOUT: Duration = Duration::from_secs(600);

/// `(status, version)` of the package, as dpkg records it.
fn package_state() -> Option<(String, String)> {
    let line = run_output("dpkg-query", &["-W", "-f=${Status}|${Version}", PACKAGE])?;
    let (status, version) = line.split_once('|')?;
    Some((status.to_owned(), version.to_owned()))
}

pub fn installed_version() -> Option<String> {
    package_state()
        .map(|(_, version)| version)
        .filter(|version| !version.is_empty())
}

pub const fn installed_fingerprint() -> Option<String> {
    None
}

pub fn prepare_layout(paths: &Paths) -> Result<()> {
    for dir in [
        paths.root().to_path_buf(),
        paths.bin_dir(),
        paths.logs_dir(),
        paths.payloads_dir(),
        paths.root().join("txn"),
    ] {
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub const fn check_ready(_manifest: &Manifest) -> Result<()> {
    Ok(())
}

fn unit_name(id: &str) -> String {
    format!("clash-verge-update-{id}")
}

/// Starts the worker in a transient unit of its own, from a copy of this program.
pub fn spawn_worker(paths: &Paths, id: &str) -> Result<ProcessRef> {
    let bin = paths.txn_dir(id).join("bin");
    std::fs::create_dir_all(&bin)?;
    let copy = bin.join("clash-verge-updater");
    clash_verge_updater::state::copy_synced(&std::env::current_exe()?, &copy)?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755))?;
    let mut command = Command::new("systemd-run");
    command.args(["--quiet", &format!("--unit={}", unit_name(id))]);
    command.arg(format!("--setenv=CLASH_VERGE_UPDATE_TXN={id}"));
    if let Some(gid) = config(paths).service_gid {
        command.arg(format!("--setenv=CLASH_VERGE_SERVICE_GID={gid}"));
    }
    command.arg(&copy).args(["--worker", id]);
    let status = command.status().context("failed to run systemd-run")?;
    ensure!(status.success(), "systemd-run failed ({status})");
    // The worker registers itself first thing; wait for it to learn its identity.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(entries) = crate::journal::read(&paths.txn_dir(id))
            && let Some(worker) = crate::journal::writers(&entries).into_iter().next()
        {
            return Ok(worker);
        }
        ensure!(Instant::now() < deadline, "the worker did not start");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Stops an overrunning worker's unit, and with it every writer it started.
pub fn stop_worker(txn: &Txn, _worker: Option<ProcessRef>) {
    let unit = format!("{}.service", unit_name(&txn.id));
    let _ = Command::new("systemctl").args(["stop", &unit]).status();
}

pub fn layout_trusted(paths: &Paths) -> bool {
    super::owned_by_root(paths.root()) && super::owned_by_root(&paths.logs_dir())
}

pub fn worker_scope_active(id: &str) -> bool {
    let unit = format!("{}.service", unit_name(id));
    run_output("systemctl", &["show", "-p", "ActiveState", "--value", &unit])
        .is_some_and(|state| matches!(state.as_str(), "active" | "activating" | "deactivating" | "reloading"))
}

/// The GUI stays up while the package installs and restarts itself.
pub const fn relaunch_gui(_initiator: &Initiator) -> Result<bool> {
    Ok(false)
}

/// Runs a writer for `step` and registers it in the journal. It runs inside the worker's unit,
/// whose processes systemd ends when the worker ends, so it cannot outlive the transaction even
/// in the moment before it is registered. Returns its exit code.
fn run_writer(journal: &Journal, step: u32, program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<i32> {
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    let registered = journal.writer(
        step,
        ProcessRef {
            pid: child.id(),
            created: sys::process_created(child.id()).unwrap_or_default(),
        },
    );
    if let Err(error) = registered {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let deadline = Instant::now() + DPKG_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(-1));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!("{program} did not finish in time");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn apply(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Components> {
    ensure!(
        txn.kind == Kind::Deb,
        "a {:?} payload does not apply to Linux",
        txn.kind
    );
    let manifest = &payload.manifest;
    let package = manifest.files_with(Role::Package).next().context("no package")?;
    let dir = paths.txn_dir(&txn.id).join("package");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let deb = dir.join(&package.path);
    payload.extract(&package.path, &deb)?;
    std::fs::set_permissions(&deb, std::fs::Permissions::from_mode(0o644))?;
    let deb_arg = deb.to_string_lossy().into_owned();

    // The plan must touch only our package: a missing dependency fails here, before unpacking.
    // A plan that does not come within two minutes is taken as a failure.
    let plan = Command::new("timeout")
        .args(["120", "apt-get", "-s", "--no-download", "install", &deb_arg])
        .env("LC_ALL", "C")
        .output()
        .context("failed to run apt-get")?;
    let text = String::from_utf8_lossy(&plan.stdout);
    let foreign = text
        .lines()
        .filter(|line| line.starts_with("Inst ") || line.starts_with("Remv ") || line.starts_with("Conf "))
        .any(|line| line.split_whitespace().nth(1) != Some(PACKAGE));
    ensure!(
        plan.status.success() && !foreign,
        "the package needs other packages installed or removed; install it with install-linux.sh"
    );

    let service_before = sha(&Path::new(SERVICE_ROOT).join("bin/clash-verge-service"));
    let updater_before = sha(&Paths::system().program());
    let step = journal.intent(
        "dpkg_install",
        json!({ "deb": deb, "from_version": installed_version() }),
    )?;
    let gid = config(paths).service_gid.map(|gid| gid.to_string()).unwrap_or_default();
    let mut envs = vec![
        ("DPKG_FRONTEND_LOCKED", "1"),
        ("DEBIAN_FRONTEND", "noninteractive"),
        ("CLASH_VERGE_UPDATE_TXN", txn.id.as_str()),
    ];
    if !gid.is_empty() {
        envs.push(("CLASH_VERGE_SERVICE_GID", gid.as_str()));
    }
    let code = run_writer(journal, step, "dpkg", &["-i", &deb_arg], &envs)?;
    ensure!(code == 0, "dpkg failed ({code})");
    journal.done(step)?;
    Ok(Components {
        gui: true,
        cores: manifest
            .cores
            .iter()
            .map(|core| CoreChange {
                name: core.name.clone(),
                from: None,
                to: core.version.clone(),
            })
            .collect(),
        service_changed: manifest.service.is_some() && service_before != manifest.service,
        updater_changed: manifest.updater.is_some() && updater_before != manifest.updater,
    })
}

pub fn undo(_paths: &Paths, txn: &Txn, entry: &Entry, _done: bool, _writers_started: bool) -> Result<Undo> {
    match entry.name.as_str() {
        // dpkg may have run even with no writer registered (it starts a moment before), so only
        // the package state tells.
        "dpkg_install" => {
            let from = entry.data["from_version"].as_str().map(str::to_owned);
            match package_state() {
                Some((status, version)) if status == "install ok installed" && Some(&version) == from.as_ref() => {
                    Ok(Undo::Clean)
                }
                Some((_, version)) if version != txn.version && Some(&version) != from.as_ref() => Ok(Undo::Superseded),
                _ => bail!("dpkg changed the package; repair with the same version"),
            }
        }
        other => bail!("unknown step {other}"),
    }
}

pub fn verify(manifest: &Manifest) -> Result<()> {
    let (status, version) = package_state().context("the package is not installed")?;
    ensure!(status == "install ok installed", "the package is {status}");
    ensure!(version == manifest.version, "the package version is {version}");
    let service = Path::new(SERVICE_ROOT).join("bin/clash-verge-service");
    // The package installs the service every time.
    ensure!(service.exists(), "the service is not installed");
    for core in &manifest.cores {
        let approved = Path::new(SERVICE_ROOT).join("cores").join(&core.name);
        ensure!(
            sha(&approved).as_deref() == Some(core.sha256.as_str()),
            "the service runs a different {}",
            core.name
        );
    }
    if let Some(expected) = &manifest.service {
        ensure!(sha(&service).as_ref() == Some(expected), "the service binary differs");
    }
    ensure!(
        run_output("systemctl", &["is-active", "clash-verge-service"]).as_deref() == Some("active"),
        "the service is not running"
    );
    if let Some(updater) = &manifest.updater {
        ensure!(
            sha(&Paths::system().program()).as_ref() == Some(updater),
            "the update component differs"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- the component's own program

/// The group of the existing service, else the group of whoever authorized this install.
fn resolve_service_gid() -> Option<u32> {
    run_output("systemctl", &["show", "-p", "Group", "--value", "clash-verge-service"])
        .filter(|group| !group.is_empty())
        .and_then(|group| group_id(&group))
        .or_else(invoking_gid)
}

const UNIT: &str = "[Unit]
Description=Clash Verge Updater
After=network.target

[Service]
ExecStart=/var/lib/clash-verge-updater/bin/clash-verge-updater service
Restart=on-failure
RuntimeDirectory=clash-verge-updater
RuntimeDirectoryMode=0755

[Install]
WantedBy=multi-user.target
";

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl").args(args).status()?;
    ensure!(status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}

pub fn install_self() -> Result<()> {
    let paths = Paths::system();
    prepare_layout(&paths)?;
    let mut config = config(&paths);
    if config.service_gid.is_none() {
        config.service_gid = resolve_service_gid();
    }
    write_atomic(&paths.config_file(), &serde_json::to_vec_pretty(&config)?)?;

    let me = std::env::current_exe()?;
    let program = paths.program();
    let same = sha(&me).is_some() && sha(&me) == sha(&program);
    if !same {
        let new = program.with_extension("new");
        clash_verge_updater::state::copy_synced(&me, &new)?;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        // Run from the kept copy (by `recover`), that copy is the backup already and cannot be written.
        let running_previous = std::fs::canonicalize(&me).ok() == std::fs::canonicalize(paths.previous_program()).ok();
        if program.exists() && !running_previous {
            clash_verge_updater::state::copy_synced(&program, &paths.previous_program())?;
        }
        std::fs::rename(&new, &program)?;
        clash_verge_updater::state::sync_parent(&program)?;
    }
    let unit_changed = std::fs::read_to_string(UNIT_FILE).ok().as_deref() != Some(UNIT);
    if unit_changed {
        write_atomic(Path::new(UNIT_FILE), UNIT.as_bytes())?;
        systemctl(&["daemon-reload"])?;
    }
    systemctl(&["enable", SYSTEMD_UNIT])?;
    // Restarted even when nothing changed on disk: the running program or its socket group may
    // still be an older one.
    let started = systemctl(&["restart", SYSTEMD_UNIT])
        .and_then(|()| clash_verge_updater::client::wait_answering(Duration::from_secs(30)));
    if let Err(error) = started {
        let restored = (|| -> Result<()> {
            ensure!(
                !same && paths.previous_program().exists(),
                "no previous program to restore"
            );
            let restore = program.with_extension("restore");
            clash_verge_updater::state::copy_synced(&paths.previous_program(), &restore)?;
            std::fs::rename(&restore, &program)?;
            clash_verge_updater::state::sync_parent(&program)?;
            systemctl(&["restart", SYSTEMD_UNIT])?;
            clash_verge_updater::client::wait_answering(Duration::from_secs(30))
        })();
        return Err(match restored {
            Ok(()) => error.context("the update component did not start; the previous one was restored"),
            Err(restore) => error.context(format!(
                "the update component did not start, nor did the previous one: {restore:#}"
            )),
        });
    }
    let audit = AuditLog::open(&paths.logs_dir())?;
    audit.write(
        Stream::Audit,
        Record::new(Source::Actual, if same { "installed_unchanged" } else { "installed" }).detail(
            json!({ "version": env!("CARGO_PKG_VERSION"), "sha256": sha(&program), "service_gid": config.service_gid }),
        ),
    )?;
    Ok(())
}

/// Starts the resident component and waits until it answers.
pub fn start_resident() -> Result<()> {
    systemctl(&["start", SYSTEMD_UNIT])?;
    clash_verge_updater::client::wait_answering(Duration::from_secs(30))
}

/// Stops the resident component, so no running process owns the state any more. True if it ran.
/// Only a unit systemd neither knows nor runs is none: one whose file was removed still runs.
pub fn stop_resident() -> Result<bool> {
    let show = |property: &str| {
        run_output("systemctl", &["show", "-p", property, "--value", SYSTEMD_UNIT])
            .context("cannot tell whether the update component runs")
    };
    let running = show("ActiveState")? != "inactive";
    if show("LoadState")? == "not-found" && !running {
        return Ok(false);
    }
    systemctl(&["stop", SYSTEMD_UNIT])?;
    Ok(running)
}

pub fn uninstall_self() -> Result<()> {
    let paths = Paths::system();
    let _ = systemctl(&["disable", "--now", SYSTEMD_UNIT]);
    let _ = std::fs::remove_file(UNIT_FILE);
    let _ = systemctl(&["daemon-reload"]);
    let _ = std::fs::remove_dir_all(paths.bin_dir());
    Ok(())
}
