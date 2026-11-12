//! macOS: a LaunchDaemon behind a Unix socket. The GUI replaces the app bundle itself, as the
//! user; the daemon publishes only what needs root: the service's approved cores, the service and
//! this component. The worker runs in a process group of its own, so launchd stopping the daemon
//! (to replace it, for one) does not take the worker with it.

use super::Undo;
use super::unix::{config, group_id, invoking_gid, run_output, sha};
use crate::journal::{Entry, Journal};
use anyhow::{Context as _, Result, bail, ensure};
use clash_verge_updater::audit::{AuditLog, Components, CoreChange, Record, Source, Stream};
use clash_verge_updater::manifest::{Kind, Manifest, Role};
use clash_verge_updater::paths::{LAUNCHD_LABEL, Paths};
use clash_verge_updater::payload::Payload;
use clash_verge_updater::state::{Initiator, ProcessRef, Txn, copy_synced, sync_parent, write_atomic};
use clash_verge_updater::sys;
use serde_json::json;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub use super::unix::run_service;

/// The bundle's `Info.plist` is the user's to change, so it never moves the floor.
pub const SYSTEM_VERSION_TRUSTED: bool = false;

const SERVICE_LABEL: &str = "io.github.clash-verge-rev.clash-verge-rev.service";
const SERVICE_PLIST: &str = "/Library/LaunchDaemons/io.github.clash-verge-rev.clash-verge-rev.service.plist";
const SERVICE_BINARY: &str = "/Library/PrivilegedHelperTools/io.github.clash-verge-rev.clash-verge-rev.service.bundle/Contents/MacOS/clash-verge-service";
const SERVICE_CORES: &str = "/Library/Application Support/clash-verge-service/cores";
const PLIST: &str = "/Library/LaunchDaemons/io.github.clashverge.updater.plist";
const TOOL: &str = "clash-verge-service-install";
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// The bundle belongs to the user, so no version installed on this machine is known to root.
pub const fn installed_version() -> Option<String> {
    None
}

pub const fn installed_fingerprint() -> Option<String> {
    None
}

pub fn prepare_layout(paths: &Paths) -> Result<()> {
    for dir in [
        paths.root().to_path_buf(),
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

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Starts the worker from a copy of this program, in a process group of its own.
pub fn spawn_worker(paths: &Paths, id: &str) -> Result<ProcessRef> {
    let bin = paths.txn_dir(id).join("bin");
    std::fs::create_dir_all(&bin)?;
    let copy = bin.join("clash-verge-updater");
    copy_synced(&std::env::current_exe()?, &copy)?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755))?;
    let mut command = Command::new(&copy);
    command
        .args(["--worker", id])
        .env("CLASH_VERGE_UPDATE_TXN", id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(gid) = config(paths).service_gid {
        command.env("CLASH_VERGE_SERVICE_GID", gid.to_string());
    }
    let mut child = command.spawn().context("failed to start the worker")?;
    // Reaped here while the service runs; launchd reaps it once the service has gone.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
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

const PROC_PGRP_ONLY: u32 = 2;

/// The processes in process group `pgid`, zombies included, or `None` when they cannot be listed.
fn group_members(pgid: u32) -> Option<Vec<u32>> {
    let mut pids = vec![0 as libc::pid_t; 4096];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // A failed listing returns 0 like an empty one; only errno tells them apart.
    unsafe { *libc::__error() = 0 };
    let bytes = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, pgid, pids.as_mut_ptr().cast(), size) };
    if bytes < 0 || (bytes == 0 && unsafe { *libc::__error() } != 0) {
        return None;
    }
    let count = usize::try_from(bytes).ok()? / std::mem::size_of::<libc::pid_t>();
    pids.truncate(count);
    Some(
        pids.into_iter()
            .filter_map(|pid| u32::try_from(pid).ok())
            .filter(|&pid| pid != 0)
            .collect(),
    )
}

/// The creation time of process `pid`, zombie or not.
fn creation_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (read == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// Whether the worker's process group may still hold processes of the transaction. Once the
/// worker no longer leads it, its members cannot be told from those of a later group reusing the
/// id, so they count as the worker's: recovery waits rather than overlap a writer. A group led
/// by another process with the worker's id is a later one.
fn group_active(worker: ProcessRef) -> bool {
    let Some(members) = group_members(worker.pid) else {
        return true;
    };
    if !members.contains(&worker.pid) {
        return !members.is_empty();
    }
    creation_time(worker.pid).is_none_or(|created| created == worker.created)
}

fn journal_worker(id: &str) -> Option<ProcessRef> {
    crate::journal::read(&Paths::system().txn_dir(id))
        .ok()
        .and_then(|entries| crate::journal::worker(&entries))
}

pub fn worker_scope_active(id: &str) -> bool {
    journal_worker(id).is_some_and(group_active)
}

/// Kills an overrunning worker's process group, and every registered writer still running. Only a
/// group the running worker leads is certainly the transaction's to end.
pub fn stop_worker(txn: &Txn, worker: Option<ProcessRef>) {
    let entries = crate::journal::read(&Paths::system().txn_dir(&txn.id)).unwrap_or_default();
    if let Some(worker) = worker.or_else(|| crate::journal::worker(&entries))
        && sys::is_running(worker)
    {
        unsafe { libc::killpg(worker.pid as libc::pid_t, libc::SIGKILL) };
    }
    for writer in crate::journal::writers(&entries) {
        if sys::is_running(writer) {
            unsafe { libc::kill(writer.pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

pub fn layout_trusted(paths: &Paths) -> bool {
    super::owned_by_root(paths.root()) && super::owned_by_root(&paths.logs_dir())
}

/// The GUI stays up while the daemon publishes, and restarts itself.
pub const fn relaunch_gui(_initiator: &Initiator) -> Result<bool> {
    Ok(false)
}

/// Runs a writer for `step` and registers it in the journal first: the process starts as a shell
/// waiting for the go-ahead, which comes once it is registered, and becomes the writer only then.
/// Started by a process that stops before that (an offline recovery killed midway, outside any
/// worker's process group), it never runs. Returns its exit code.
fn run_writer(journal: &Journal, step: u32, program: &Path, args: &[OsString], envs: &[(&str, String)]) -> Result<i32> {
    use std::io::Write as _;
    let (reader, mut go) = std::io::pipe()?;
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"read -r _ && exec "$0" "$@""#])
        .arg(program)
        .args(args)
        .envs(envs.iter().map(|(name, value)| (*name, value.as_str())))
        .current_dir("/")
        .stdin(reader)
        .spawn()
        .with_context(|| format!("failed to start {}", program.display()))?;
    let registered = sys::process_created(child.id())
        .context("the writer did not start")
        .and_then(|created| {
            journal.writer(
                step,
                ProcessRef {
                    pid: child.id(),
                    created,
                },
            )
        })
        .and_then(|()| go.write_all(b"\n").map_err(Into::into));
    drop(go);
    if let Err(error) = registered {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(-1));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} did not finish in time", program.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Any part of the service: a Sidecar user has none, an interrupted install leaves some.
fn service_present() -> bool {
    Path::new(SERVICE_PLIST).exists() || Path::new(SERVICE_BINARY).exists()
}

/// The service is the payload's, registered and running; otherwise it is installed again.
fn service_healthy(manifest: &Manifest) -> bool {
    manifest
        .service
        .as_ref()
        .is_none_or(|service| sha(Path::new(SERVICE_BINARY)).as_ref() == Some(service))
        && Path::new(SERVICE_PLIST).exists()
        && service_running()
}

fn approved_core(name: &str) -> PathBuf {
    Path::new(SERVICE_CORES).join(name)
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

fn tool_envs(paths: &Paths) -> Vec<(&'static str, String)> {
    config(paths)
        .service_gid
        .map(|gid| vec![("CLASH_VERGE_SERVICE_GID", gid.to_string())])
        .unwrap_or_default()
}

/// The privileged phase, once the GUI has replaced the bundle: publishes the cores and, when it
/// changed, the service with the upstream tool run from the protected transaction directory
/// (the tool looks for the service binary beside itself), then replaces this component.
pub fn apply(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Components> {
    ensure!(
        txn.kind == Kind::Bundle,
        "a {:?} payload does not apply to macOS",
        txn.kind
    );
    let manifest = &payload.manifest;
    let tools = paths.txn_dir(&txn.id).join("tools");
    std::fs::create_dir_all(&tools)?;
    let mut extracted = vec![TOOL.to_owned(), "clash-verge-service".to_owned()];
    extracted.extend(manifest.cores.iter().map(|core| core.name.clone()));
    for name in &extracted {
        let file = tools.join(name);
        payload.extract(name, &file)?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
    }

    // The bundle was replaced by the GUI; that part is the client's word.
    let mut components = Components {
        gui: true,
        ..Components::default()
    };
    // Without the service, the app runs its cores itself: there is nothing to publish.
    if service_present() {
        if !service_healthy(manifest) {
            components.cores = changed_cores(manifest);
            // Reinstalling restarts the service; the cores go along in the same run.
            let mut args = vec![OsString::from("--install-service")];
            for core in &manifest.cores {
                args.extend([
                    "--install-core".into(),
                    tools.join(&core.name).into_os_string(),
                    "--sha256".into(),
                    core.sha256.clone().into(),
                ]);
            }
            let step = journal.intent("install_service", json!({}))?;
            let code = run_writer(journal, step, &tools.join(TOOL), &args, &tool_envs(paths))?;
            ensure!(code == 0, "reinstalling the service failed ({code})");
            journal.done(step)?;
            components.service_changed = true;
        } else {
            components.cores = publish_cores(paths, txn, payload, journal, &tools)?;
        }
    }
    components.updater_changed = replace_updater(paths, payload, journal)?;
    Ok(components)
}

fn changed_cores(manifest: &Manifest) -> Vec<CoreChange> {
    manifest
        .cores
        .iter()
        .filter(|core| sha(&approved_core(&core.name)).as_deref() != Some(core.sha256.as_str()))
        .map(|core| CoreChange {
            name: core.name.clone(),
            from: None,
            to: core.version.clone(),
        })
        .collect()
}

/// Publishes each changed core to the service, after keeping the approved copy to restore.
fn publish_cores(
    paths: &Paths,
    txn: &Txn,
    payload: &Payload,
    journal: &mut Journal,
    tools: &Path,
) -> Result<Vec<CoreChange>> {
    let backup_dir = paths.txn_dir(&txn.id).join("approved-backup");
    let changes = changed_cores(&payload.manifest);
    for change in &changes {
        let core = payload
            .manifest
            .cores
            .iter()
            .find(|core| core.name == change.name)
            .context("missing core")?;
        let approved = approved_core(&core.name);
        let current = sha(&approved);
        // The tool takes the core's name from the file name, so the backup keeps it.
        let backup = current.as_ref().map(|_| backup_dir.join(&core.name));
        if let Some(backup) = &backup {
            std::fs::create_dir_all(&backup_dir)?;
            copy_synced(&approved, backup)?;
        }
        let step = journal.intent(
            "publish_core",
            json!({ "name": core.name, "old_sha": current, "new_sha": core.sha256, "backup": backup, "tool": tools.join(TOOL) }),
        )?;
        let args: Vec<OsString> = vec![
            "--install-core".into(),
            tools.join(&core.name).into_os_string(),
            "--sha256".into(),
            core.sha256.clone().into(),
        ];
        let code = run_writer(journal, step, &tools.join(TOOL), &args, &tool_envs(paths))?;
        ensure!(code == 0, "publishing {} to the service failed ({code})", core.name);
        journal.done(step)?;
    }
    Ok(changes)
}

fn launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl").args(args).status()?;
    ensure!(status.success(), "launchctl {} failed ({status})", args.join(" "));
    Ok(())
}

fn target() -> String {
    format!("system/{LAUNCHD_LABEL}")
}

/// Whether launchd has the component loaded. Only launchd saying it has no such service means it
/// is not; anything else unclear is an error, never taken as gone.
fn loaded() -> Result<bool> {
    let output = Command::new("launchctl")
        .args(["print", &target()])
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.code() == Some(113) && error.contains("Could not find service"),
        "cannot tell whether the update component is loaded: {}",
        error.trim()
    );
    Ok(false)
}

/// Restarts the resident component (or loads it) and waits until it answers.
fn restart_resident() -> Result<()> {
    if loaded()? {
        launchctl(&["kickstart", "-k", &target()])?;
    } else {
        bootstrap()?;
    }
    clash_verge_updater::client::wait_answering(Duration::from_secs(30))
}

/// `bootstrap` can fail for a moment while launchd still tears down an earlier instance.
fn bootstrap() -> Result<()> {
    let mut attempts = 0;
    loop {
        match launchctl(&["bootstrap", "system", PLIST]) {
            Ok(()) => return Ok(()),
            Err(error) if attempts >= 5 => return Err(error),
            Err(_) => {
                attempts += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Puts `new` at the resident program's path and restarts it; the previous program comes back if
/// the new one does not answer.
fn swap_program(paths: &Paths, new: &Path) -> Result<()> {
    let program = paths.program();
    if program.exists() {
        copy_synced(&program, &paths.previous_program())?;
    }
    std::fs::rename(new, &program)?;
    sync_parent(&program)?;
    restart_resident().map_err(|error| fall_back(paths, error))
}

/// Puts the kept previous program back at the fixed path and starts it. The backup is copied, not
/// moved, so it stays for a later recovery.
fn restore_previous(paths: &Paths) -> Result<()> {
    let program = paths.program();
    ensure!(paths.previous_program().exists(), "no previous program to restore");
    let restore = sibling(&program, ".restore");
    copy_synced(&paths.previous_program(), &restore)?;
    std::fs::rename(&restore, &program)?;
    sync_parent(&program)?;
    restart_resident()
}

/// The program at the fixed path did not start: the previous one is put back, and `error` still
/// fails the step.
fn fall_back(paths: &Paths, error: anyhow::Error) -> anyhow::Error {
    match restore_previous(paths) {
        Ok(()) => error.context("the update component did not start; the previous one was restored"),
        Err(restore) => error.context(format!(
            "the update component did not start, nor did the previous one: {restore:#}"
        )),
    }
}

/// The resident process started before its program last changed, or does not run: a program
/// put in place without a restart that took.
fn resident_stale(paths: &Paths) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let pid = run_output("launchctl", &["print", &target()]).and_then(|text| {
        text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("pid = ")
                .and_then(|pid| pid.parse::<u32>().ok())
        })
    });
    let started = pid.and_then(sys::process_created);
    let changed = std::fs::metadata(paths.program()).ok().and_then(|meta| {
        u64::try_from(meta.ctime())
            .ok()
            .map(|secs| secs * 1_000_000 + meta.ctime_nsec() as u64 / 1000)
    });
    match (started, changed) {
        (Some(started), Some(changed)) => started < changed,
        _ => true,
    }
}

fn replace_updater(paths: &Paths, payload: &Payload, journal: &mut Journal) -> Result<bool> {
    let Some(file) = payload.manifest.files_with(Role::Updater).next() else {
        return Ok(false);
    };
    if sha(&paths.program()).as_deref() == Some(file.sha256.as_str()) {
        if !resident_stale(paths) {
            return Ok(false);
        }
        // Put in place earlier by a transaction that stopped before the restart.
        let step = journal.intent("restart_updater", json!({}))?;
        restart_resident().map_err(|error| fall_back(paths, error))?;
        journal.done(step)?;
        return Ok(true);
    }
    let new = sibling(&paths.program(), ".new");
    payload.extract(&file.path, &new)?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    let step = journal.intent("replace_updater", json!({ "sha": file.sha256 }))?;
    swap_program(paths, &new)?;
    journal.done(step)?;
    Ok(true)
}

pub fn undo(paths: &Paths, txn: &Txn, entry: &Entry, _done: bool, writers_started: bool) -> Result<Undo> {
    let data = &entry.data;
    match entry.name.as_str() {
        "publish_core" => {
            let name = json_str(data, "name").context("no core name")?;
            let current = sha(&approved_core(&name));
            let old = json_str(data, "old_sha");
            if current == old {
                return Ok(Undo::Clean);
            }
            if current != json_str(data, "new_sha") {
                return Ok(Undo::Superseded);
            }
            let (Some(old), Some(backup)) = (old, json_str(data, "backup")) else {
                // A core the service did not have before; leaving it is harmless.
                return Ok(Undo::Clean);
            };
            // Restoring writes as much as publishing did, so it is registered alike.
            let journal = Journal::open(&paths.txn_dir(&txn.id))?;
            let tool = PathBuf::from(json_str(data, "tool").context("no tool")?);
            let args: Vec<OsString> = vec!["--install-core".into(), backup.into(), "--sha256".into(), old.into()];
            let code = run_writer(&journal, entry.step, &tool, &args, &tool_envs(paths))?;
            ensure!(code == 0, "restoring {name} to the service failed ({code})");
            Ok(Undo::Restored)
        }
        // Swapped back on the spot when the new program does not start.
        "replace_updater" | "restart_updater" => Ok(Undo::Clean),
        "install_service" if !writers_started => Ok(Undo::Clean),
        "install_service" => bail!("what the service installer changed cannot be undone; repair with the same version"),
        other => bail!("unknown step {other}"),
    }
}

fn service_running() -> bool {
    run_output("launchctl", &["print", &format!("system/{SERVICE_LABEL}")])
        .is_some_and(|text| text.lines().any(|line| line.trim() == "state = running"))
}

/// Confirms what root owns matches the payload: the service's approved cores and binary, and the
/// component itself. The bundle is the user's and is not checked here.
pub fn verify(manifest: &Manifest) -> Result<()> {
    if service_present() {
        for core in &manifest.cores {
            ensure!(
                sha(&approved_core(&core.name)).as_deref() == Some(core.sha256.as_str()),
                "the service runs a different {}",
                core.name
            );
        }
        if let Some(service) = &manifest.service {
            ensure!(
                sha(Path::new(SERVICE_BINARY)).as_ref() == Some(service),
                "the service binary differs"
            );
        }
        ensure!(
            Path::new(SERVICE_PLIST).exists() && service_running(),
            "the service is not running"
        );
    }
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
    run_output("plutil", &["-extract", "GroupName", "raw", "-o", "-", SERVICE_PLIST])
        .filter(|group| !group.is_empty())
        .and_then(|group| group_id(&group))
        .or_else(invoking_gid)
}

fn plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>service</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#,
        Paths::system().program().display()
    )
}

/// Installs this program as the resident component, or updates it, and makes sure it runs. The
/// GUI runs this once with the user's authorization; so does `recover`, from a kept copy.
pub fn install_self() -> Result<()> {
    let paths = Paths::system();
    prepare_layout(&paths)?;
    // A worker holds the lock throughout and replaces the same files; `recover` holds it already.
    let _lock = if crate::lock::held() {
        None
    } else {
        Some(crate::lock::acquire(&paths, Duration::from_secs(60))?)
    };
    let mut config = config(&paths);
    if config.service_gid.is_none() {
        config.service_gid = resolve_service_gid();
    }
    write_atomic(&paths.config_file(), &serde_json::to_vec_pretty(&config)?)?;

    let me = std::env::current_exe()?;
    let program = paths.program();
    let same = sha(&me).is_some() && sha(&me) == sha(&program);
    if !same {
        let new = sibling(&program, ".new");
        copy_synced(&me, &new)?;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        // Run from the kept copy (by `recover`), that copy is the backup already.
        let running_previous = std::fs::canonicalize(&me).ok() == std::fs::canonicalize(paths.previous_program()).ok();
        if program.exists() && !running_previous {
            copy_synced(&program, &paths.previous_program())?;
        }
        std::fs::rename(&new, &program)?;
        sync_parent(&program)?;
    }
    let plist = plist();
    if std::fs::read_to_string(PLIST).ok().as_deref() != Some(plist.as_str()) {
        // A loaded job keeps its old definition until it is booted out.
        if loaded()? {
            launchctl(&["bootout", &target()])?;
        }
        write_atomic(Path::new(PLIST), plist.as_bytes())?;
        std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))?;
    }
    // Restarted even when nothing changed on disk: the running program may still be an older one.
    if let Err(error) = restart_resident() {
        return Err(fall_back(&paths, error));
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
    if loaded()? {
        launchctl(&["kickstart", &target()])?;
    } else {
        bootstrap()?;
    }
    clash_verge_updater::client::wait_answering(Duration::from_secs(30))
}

/// Stops the resident component, so no running process owns the state any more. True if it ran.
pub fn stop_resident() -> Result<bool> {
    let was_loaded = loaded()?;
    if was_loaded {
        launchctl(&["bootout", &target()])?;
    }
    Ok(was_loaded)
}

/// Removes the component; its state and audit log stay. Refused while an update is in progress.
pub fn uninstall_self() -> Result<()> {
    let paths = Paths::system();
    let _lock = crate::lock::acquire(&paths, Duration::from_secs(60))?;
    // Stopped first, so no update starts between the check and the removal.
    let was_loaded = stop_resident()?;
    if let Some(txn) = clash_verge_updater::state::State::load(&paths.state_file())?.txn {
        if was_loaded {
            bootstrap()?;
        }
        bail!(
            "update {} (v{}) is in progress; let it finish or abandon it first",
            txn.id,
            txn.version
        );
    }
    let socket = clash_verge_updater::paths::socket_path();
    for path in [PathBuf::from(PLIST), paths.program(), paths.previous_program(), socket] {
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}
