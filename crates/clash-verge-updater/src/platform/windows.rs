//! Windows: a LocalSystem service behind a named pipe. `patch` swaps files in the install directory
//! and publishes cores to the service; `full` runs the NSIS installer silently. Writers run in a
//! kill-on-close job, so they stop when the worker does.

use super::{Undo, command_line, windows_trust};
use crate::daemon::Daemon;
use crate::journal::{Entry, Journal};
use crate::logger;
use anyhow::{Context as _, Result, bail, ensure};
use clash_verge_updater::audit::{AuditLog, Components, CoreChange, Record, Source, Stream};
use clash_verge_updater::manifest::{Kind, Manifest, Role};
use clash_verge_updater::paths::{PIPE_NAME, Paths, SERVICE_NAME};
use clash_verge_updater::payload::{Payload, sha256_file};
use clash_verge_updater::state::{Initiator, ProcessRef, Txn};
use clash_verge_updater::sys::{self, ProcessInfo};
use serde_json::json;
use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_DELAY_UNTIL_REBOOT, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeClientSessionId};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_SZ, RegGetValueW, RegSetKeyValueW};
use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION,
    ResumeThread, STARTUPINFOW, WaitForSingleObject,
};

/// The registered version comes from the protected registry, so it can move the floor.
pub const SYSTEM_VERSION_TRUSTED: bool = true;

const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Clash Verge";
const ROOT_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
const LOCK_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";
/// Owned by SYSTEM, which clients check. SYSTEM and Administrators in full; interactive users may
/// read and write but not create instances, so they cannot take requests meant for the component.
const PIPE_SDDL: &str = "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12018b;;;IU)";
const GUI_EXE: &str = "clash-verge.exe";
const INSTALLER_TIMEOUT: Duration = Duration::from_secs(600);
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ---------------------------------------------------------------- installed state

fn read_registry(value: &str) -> Option<String> {
    let key = wide(OsStr::new(UNINSTALL_KEY));
    let name = wide(OsStr::new(value));
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buffer[..len]))
}

fn write_registry(value: &str, data: &str) -> Result<()> {
    let key = wide(OsStr::new(UNINSTALL_KEY));
    let name = wide(OsStr::new(value));
    let data = wide(OsStr::new(data));
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    ensure!(status == 0, "failed to write {value} (Win32 {status})");
    Ok(())
}

pub fn install_dir() -> Option<PathBuf> {
    let location = read_registry("InstallLocation")?;
    let location = location.trim().trim_matches('"');
    (!location.is_empty()).then(|| PathBuf::from(location))
}

pub fn installed_version() -> Option<String> {
    read_registry("DisplayVersion").filter(|version| !version.is_empty())
}

pub fn installed_fingerprint() -> Option<String> {
    let text = std::fs::read_to_string(install_dir()?.join("resources").join("install-fingerprint")).ok()?;
    Some(text.trim().to_owned())
}

fn service_root() -> PathBuf {
    let data = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    data.join("clash-verge-service")
}

fn approved_core(file: &str) -> PathBuf {
    service_root().join("cores").join(file)
}

fn service_binary() -> PathBuf {
    service_root().join("bin").join("clash-verge-service.exe")
}

// ---------------------------------------------------------------- layout and ACLs

fn set_dacl(path: &Path, sddl: &str) -> Result<()> {
    let sddl = wide(OsStr::new(sddl));
    let mut descriptor = std::ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "invalid security descriptor"
    );
    let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    let target = wide(path.as_os_str());
    let status = unsafe {
        SetNamedSecurityInfoW(
            target.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
    ensure!(status == 0, "failed to protect {} (Win32 {status})", path.display());
    Ok(())
}

/// Creates `path` with `sddl` from the start, so it is never open to others in between. A
/// directory already there is left for the caller to judge.
fn create_protected_dir(path: &Path, sddl: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, LocalFree};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let sddl_w = wide(OsStr::new(sddl));
    let mut descriptor = std::ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "invalid security descriptor"
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let target = wide(path.as_os_str());
    let created = unsafe { CreateDirectoryW(target.as_ptr(), &attributes) } != 0;
    let error = std::io::Error::last_os_error();
    unsafe { LocalFree(descriptor) };
    if !created && error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
        return Err(error).with_context(|| format!("failed to create {}", path.display()));
    }
    Ok(())
}

/// Creates the state directory with its protected ACL; the lock file is closed even to readers.
/// ProgramData lets any user create a directory, so one found there that SYSTEM or
/// Administrators do not own is moved aside unread: its owner could open it up again.
pub fn prepare_layout(paths: &Paths) -> Result<()> {
    let root = paths.root();
    // Someone may create the directory between any check and the creation, so whatever is there
    // afterwards is checked again; one SYSTEM or Administrators own cannot be replaced by users.
    let mut attempt = 0;
    loop {
        if std::fs::symlink_metadata(root).is_ok() {
            match windows_trust::check_owner(root) {
                Ok(()) => break,
                Err(error) => {
                    attempt += 1;
                    ensure!(attempt <= 3, "{} keeps being taken: {error:#}", root.display());
                    let aside = root.with_file_name(format!(
                        "clash-verge-updater.untrusted-{}-{attempt}",
                        chrono::Local::now().format("%Y%m%d%H%M%S")
                    ));
                    move_file(root, &aside, 0)?;
                    logger::log(&format!(
                        "moved {} aside to {}: {error:#}",
                        root.display(),
                        aside.display()
                    ));
                }
            }
        }
        create_protected_dir(root, ROOT_SDDL)?;
    }
    set_dacl(root, ROOT_SDDL)?;
    // A subdirectory made by someone else before the root was protected is not used either.
    for dir in [
        paths.bin_dir(),
        paths.logs_dir(),
        paths.payloads_dir(),
        paths.root().join("txn"),
    ] {
        if std::fs::symlink_metadata(&dir).is_ok()
            && let Err(error) = windows_trust::check_owner(&dir)
        {
            let mut aside = dir.clone().into_os_string();
            aside.push(format!(".untrusted-{}", chrono::Local::now().format("%Y%m%d%H%M%S")));
            move_file(&dir, Path::new(&aside), 0)?;
            logger::log(&format!("moved {} aside: {error:#}", dir.display()));
        }
        std::fs::create_dir_all(&dir)?;
    }
    let lock = paths.lock_file();
    if !lock.exists() {
        std::fs::write(&lock, b"")?;
    }
    set_dacl(&lock, LOCK_SDDL)
}

/// SYSTEM writes into and runs from the install directory only if nobody else can change it.
pub fn check_ready(_manifest: &Manifest) -> Result<()> {
    let dir = install_dir().context("Clash Verge is not registered as installed")?;
    windows_trust::check(&dir).context("the install directory is not protected")?;
    // Files keep their own ACL when an installer writes over them, so those that run as SYSTEM
    // afterwards are checked one by one.
    for relative in [
        GUI_EXE,
        "verge-mihomo.exe",
        "verge-mihomo-alpha.exe",
        "resources",
        r"resources\clash-verge-updater.exe",
        r"resources\clash-verge-service.exe",
        r"resources\clash-verge-service-install.exe",
    ] {
        let path = dir.join(relative);
        if path.exists() {
            windows_trust::check(&path).with_context(|| format!("{} is not protected", path.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- processes

fn spawn(program: &Path, args: &[&OsStr], flags: u32, cwd: Option<&Path>) -> std::io::Result<PROCESS_INFORMATION> {
    let application = wide(program.as_os_str());
    let mut line = wide(&command_line(program, args));
    let cwd = cwd.map(|dir| wide(dir.as_os_str()));
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags,
            std::ptr::null(),
            cwd.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info)
}

fn process_ref(pid: u32) -> ProcessRef {
    ProcessRef {
        pid,
        created: sys::process_created(pid).unwrap_or_default(),
    }
}

/// Starts the worker from a copy of this program in the transaction directory, outside any job
/// so stopping the service leaves it running.
pub fn spawn_worker(paths: &Paths, id: &str) -> Result<ProcessRef> {
    let bin = paths.txn_dir(id).join("bin");
    std::fs::create_dir_all(&bin)?;
    let copy = bin.join("clash-verge-updater.exe");
    std::fs::copy(std::env::current_exe()?, &copy)?;
    let args = [OsStr::new("--worker"), OsStr::new(id)];
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    let info = spawn(&copy, &args, flags | CREATE_BREAKAWAY_FROM_JOB, None)
        .or_else(|_| spawn(&copy, &args, flags, None))
        .context("failed to start the worker")?;
    let process = process_ref(info.dwProcessId);
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(process)
}

/// Whether processes this service started for recovery are still running: they are in its own
/// writer job, which stays open as long as the service runs.
pub fn worker_scope_active(_id: &str) -> bool {
    WRITER_JOB.get().is_some_and(|job| !job_empty(*job as HANDLE))
}

/// Stops an overrunning worker; its writer job closes with it and takes the writers down.
pub fn stop_worker(_txn: &Txn, worker: Option<ProcessRef>) {
    if let Some(job) = WRITER_JOB.get() {
        let _ = stop_job(*job as HANDLE, Duration::from_secs(30));
    }
    if let Some(worker) = worker {
        sys::terminate(worker);
    }
}

/// The log directory is the component's own: no reparse point on the way, nobody else can add to it.
pub fn layout_trusted(paths: &Paths) -> bool {
    windows_trust::check(&paths.logs_dir()).is_ok()
}

static WRITER_JOB: OnceLock<usize> = OnceLock::new();

/// The job every writer runs in; it is closed, and its processes killed, when the worker exits.
fn writer_job() -> Result<HANDLE> {
    if let Some(job) = WRITER_JOB.get() {
        return Ok(*job as HANDLE);
    }
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    ensure!(!job.is_null(), "failed to create the writer job");
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    ensure!(
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0,
        "failed to configure the writer job"
    );
    Ok(*WRITER_JOB.get_or_init(|| job as usize) as HANDLE)
}

/// Runs a writer for `step`: created suspended, put in the job, registered in the journal, only
/// then let run. Returns its exit code; on timeout the whole job is terminated.
fn run_writer(journal: &Journal, step: u32, program: &Path, args: &[&OsStr], timeout: Duration) -> Result<u32> {
    let job = writer_job()?;
    let info = spawn(program, args, CREATE_NO_WINDOW | CREATE_SUSPENDED, program.parent())
        .with_context(|| format!("failed to start {}", program.display()))?;
    let process = OwnedHandle(info.hProcess);
    let thread = OwnedHandle(info.hThread);
    if unsafe { AssignProcessToJobObject(job, process.0) } == 0 {
        unsafe { windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1) };
        bail!("failed to confine {}", program.display());
    }
    if let Err(error) = journal.writer(step, process_ref(info.dwProcessId)) {
        unsafe { windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1) };
        return Err(error);
    }
    unsafe { ResumeThread(thread.0) };
    let wait = unsafe { WaitForSingleObject(process.0, u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)) };
    if wait != WAIT_OBJECT_0 {
        stop_job(job, Duration::from_secs(30))?;
        bail!("{} did not finish in time", program.display());
    }
    // Processes the writer started may outlive it; they are stopped too.
    if wait_job_empty(job, Duration::from_secs(30)).is_err() {
        stop_job(job, Duration::from_secs(30))?;
    }
    let mut code = 0;
    unsafe { GetExitCodeProcess(process.0, &mut code) };
    Ok(code)
}

#[repr(C)]
struct JobProcessIds {
    assigned: u32,
    listed: u32,
    ids: [usize; 64],
}

/// Terminates every process in `job` and waits until each has exited. Termination is
/// asynchronous and a process leaves the job before it lets go of its files, so each is waited
/// on through a handle taken beforehand; failing that, writers may still be running.
fn stop_job(job: HANDLE, timeout: Duration) -> Result<()> {
    use windows_sys::Win32::System::JobObjects::JobObjectBasicProcessIdList;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    let mut list = JobProcessIds {
        assigned: 0,
        listed: 0,
        ids: [0; 64],
    };
    let listed = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            std::mem::size_of::<JobProcessIds>() as u32,
            std::ptr::null_mut(),
        )
    } != 0;
    if !listed || list.listed < list.assigned {
        unsafe { TerminateJobObject(job, 1) };
        return Err(anyhow::Error::new(crate::txn::WritersRunning));
    }
    let handles: Vec<OwnedHandle> = list.ids[..list.listed as usize]
        .iter()
        .filter_map(|&pid| {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid as u32) };
            (!handle.is_null()).then(|| OwnedHandle(handle))
        })
        .collect();
    unsafe { TerminateJobObject(job, 1) };
    let deadline = Instant::now() + timeout;
    for handle in &handles {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = unsafe { WaitForSingleObject(handle.0, u32::try_from(left.as_millis()).unwrap_or(u32::MAX)) };
        if wait != WAIT_OBJECT_0 {
            return Err(anyhow::Error::new(crate::txn::WritersRunning));
        }
    }
    wait_job_empty(job, deadline.saturating_duration_since(Instant::now()))
}

/// Whether no process is left in `job`; a job that cannot be queried counts as busy.
fn job_empty(job: HANDLE) -> bool {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    } != 0;
    queried && info.ActiveProcesses == 0
}

/// Waits until no process is left in the writer job; failing that, writers may still be running.
fn wait_job_empty(job: HANDLE, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if job_empty(job) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow::Error::new(crate::txn::WritersRunning));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// GUI processes running from the install directory, with their sessions.
fn gui_processes(install: &Path) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return found;
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if name.eq_ignore_ascii_case(GUI_EXE)
            && let Ok(info) = ProcessInfo::query(entry.th32ProcessID)
            && info
                .image
                .parent()
                .is_some_and(|dir| dir.as_os_str().eq_ignore_ascii_case(install.as_os_str()))
        {
            let mut session = 0;
            unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut session) };
            found.push((entry.th32ProcessID, session));
        }
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    found
}

/// Starts the GUI in the session of the user who asked for the update, as that user.
pub fn relaunch_gui(initiator: &Initiator) -> Result<bool> {
    let Some(session) = initiator.session else {
        return Ok(false);
    };
    let install = install_dir().context("no install directory")?;
    let mut token = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        return Ok(false);
    }
    let token = OwnedHandle(token);
    if unsafe { sys::token_user_sid(token.0) }.as_deref() != Some(initiator.id.as_str()) {
        return Ok(false);
    }
    let mut environment = std::ptr::null_mut();
    ensure!(
        unsafe { CreateEnvironmentBlock(&mut environment, token.0, 0) } != 0,
        "failed to build the user environment"
    );
    let program = install.join(GUI_EXE);
    let application = wide(program.as_os_str());
    let mut line = wide(&command_line(&program, &[]));
    let cwd = wide(install.as_os_str());
    let mut desktop = wide(OsStr::new("winsta0\\default"));
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.lpDesktop = desktop.as_mut_ptr();
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessAsUserW(
            token.0,
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT,
            environment,
            cwd.as_ptr(),
            &startup,
            &mut info,
        )
    };
    unsafe { DestroyEnvironmentBlock(environment) };
    ensure!(
        created != 0,
        "failed to start the GUI: {}",
        std::io::Error::last_os_error()
    );
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(true)
}

// ---------------------------------------------------------------- applying

fn move_file(from: &Path, to: &Path, flags: u32) -> Result<()> {
    let (from_w, to_w) = (wide(from.as_os_str()), wide(to.as_os_str()));
    ensure!(
        unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), flags) } != 0,
        "failed to move {} to {}: {}",
        from.display(),
        to.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn delete_on_reboot(path: &Path) {
    let path = wide(path.as_os_str());
    unsafe { MoveFileExW(path.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
}

fn sha(path: &Path) -> Option<String> {
    sha256_file(path).ok()
}

fn json_path(value: &serde_json::Value, key: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        value[key]
            .as_str()
            .with_context(|| format!("journal entry lacks {key}"))?,
    ))
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

pub fn apply(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Components> {
    check_ready(&payload.manifest)?;
    match txn.kind {
        Kind::Patch => apply_patch(paths, txn, payload, journal),
        Kind::Full => apply_full(paths, txn, payload, journal),
        other => bail!("a {other:?} payload does not apply to Windows"),
    }
}

/// Publishes changed cores to the service with its own tool, run from the protected transaction
/// directory, after keeping the approved copies to restore.
fn publish_cores(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Vec<CoreChange>> {
    let manifest = &payload.manifest;
    let tools = paths.txn_dir(&txn.id).join("tools");
    std::fs::create_dir_all(&tools)?;
    for name in ["clash-verge-service.exe", "clash-verge-service-install.exe"] {
        payload.extract(name, &tools.join(name))?;
    }
    let backup_dir = paths.txn_dir(&txn.id).join("approved-backup");
    let mut changes = Vec::new();
    for core in &manifest.cores {
        let file = manifest.core_file_name(&core.name);
        let approved = approved_core(&file);
        if !service_root().exists() {
            break;
        }
        let current = sha(&approved);
        if current.as_deref() == Some(core.sha256.as_str()) {
            continue;
        }
        let staged = tools.join(&file);
        payload.extract(&file, &staged)?;
        let backup = current.as_ref().map(|_| backup_dir.join(&file));
        if let Some(backup) = &backup {
            std::fs::create_dir_all(&backup_dir)?;
            clash_verge_updater::state::copy_synced(&approved, backup)?;
        }
        let step = journal.intent(
            "publish_core",
            json!({ "file": file, "old_sha": current, "new_sha": core.sha256, "backup": backup, "tool": tools.join("clash-verge-service-install.exe") }),
        )?;
        let code = run_writer(
            journal,
            step,
            &tools.join("clash-verge-service-install.exe"),
            &[
                OsStr::new("--install-core"),
                staged.as_os_str(),
                OsStr::new("--sha256"),
                OsStr::new(&core.sha256),
            ],
            TOOL_TIMEOUT,
        )?;
        ensure!(code == 0, "publishing {} to the service failed ({code})", core.name);
        journal.done(step)?;
        changes.push(CoreChange {
            name: core.name.clone(),
            from: None,
            to: core.version.clone(),
        });
    }
    Ok(changes)
}

fn apply_patch(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Components> {
    let manifest = &payload.manifest;
    let install = install_dir().context("no install directory")?;
    let staging = install.join(format!(".update-{}", txn.id));
    let step = journal.intent("extract", json!({ "dir": staging }))?;
    std::fs::create_dir(&staging).with_context(|| format!("failed to create {}", staging.display()))?;
    let swaps: Vec<(&str, PathBuf)> = vec![
        (GUI_EXE, install.join(GUI_EXE)),
        ("verge-mihomo.exe", install.join("verge-mihomo.exe")),
        ("verge-mihomo-alpha.exe", install.join("verge-mihomo-alpha.exe")),
        (
            "clash-verge-updater.exe",
            install.join("resources").join("clash-verge-updater.exe"),
        ),
    ];
    for (name, _) in &swaps {
        payload.extract(name, &staging.join(name))?;
    }
    journal.done(step)?;

    let mut components = Components {
        cores: publish_cores(paths, txn, payload, journal)?,
        ..Components::default()
    };

    for (name, target) in &swaps {
        let new_sha = &manifest.file(name).context("missing file")?.sha256;
        let old_sha = sha(target);
        if old_sha.as_deref() == Some(new_sha.as_str()) {
            continue;
        }
        let old = PathBuf::from(format!("{}.old-{}", target.display(), txn.id));
        let staged = staging.join(name);
        let step = journal.intent(
            "swap",
            json!({ "target": target, "old": old, "staged": staged, "old_sha": old_sha, "new_sha": new_sha }),
        )?;
        if target.exists() {
            move_file(target, &old, MOVEFILE_WRITE_THROUGH)?;
        }
        move_file(&staged, target, MOVEFILE_WRITE_THROUGH)?;
        journal.done(step)?;
        components.gui |= *name == GUI_EXE;
    }

    let old_version = installed_version();
    let step = journal.intent(
        "display_version",
        json!({ "old": old_version, "new": manifest.version }),
    )?;
    write_registry("DisplayVersion", &manifest.version)?;
    journal.done(step)?;

    components.updater_changed = replace_updater(paths, payload, journal)?;
    let _ = std::fs::remove_dir_all(&staging);
    clean_old_files(&install, &txn.id);
    Ok(components)
}

/// Deletes files earlier transactions set aside; those still in use go at the next reboot.
fn clean_old_files(install: &Path, current: &str) {
    for dir in [install.to_path_buf(), install.join("resources")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale_old = name.contains(".old-") && !name.ends_with(current);
            let stale_staging = name.starts_with(".update-") && !name.ends_with(current);
            if stale_old && std::fs::remove_file(entry.path()).is_err() {
                delete_on_reboot(&entry.path());
            }
            if stale_staging {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn apply_full(paths: &Paths, txn: &Txn, payload: &Payload, journal: &mut Journal) -> Result<Components> {
    let manifest = &payload.manifest;
    let install = install_dir().context("no install directory")?;
    // The installer cannot close a GUI in another session from session 0; it would abort.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let running = gui_processes(&install);
        if running.is_empty() {
            break;
        }
        let others = running
            .iter()
            .any(|(_, session)| Some(*session) != txn.initiator.session);
        ensure!(
            !others,
            "Clash Verge is running in another user's session; close it there first"
        );
        ensure!(Instant::now() < deadline, "Clash Verge did not exit for the update");
        std::thread::sleep(Duration::from_millis(500));
    }
    let installer = manifest.files_with(Role::Installer).next().context("no installer")?;
    let dir = paths.txn_dir(&txn.id).join("installer");
    std::fs::create_dir_all(&dir)?;
    let setup = dir.join(&installer.path);
    payload.extract(&installer.path, &setup)?;

    let service_before = sha(&service_binary());
    let updater_before = sha(&paths.program());
    let step = journal.intent("run_installer", json!({ "setup": setup }))?;
    let updater_arg = OsString::from(format!("/UPDATER={}", txn.id));
    let code = run_writer(
        journal,
        step,
        &setup,
        &[OsStr::new("/S"), OsStr::new("/UPDATE"), &updater_arg],
        INSTALLER_TIMEOUT,
    )?;
    ensure!(code == 0, "the installer failed ({code})");
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

// ---------------------------------------------------------------- the component's own program

fn service_manager() -> Result<windows_service::service_manager::ServiceManager> {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    Ok(ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?)
}

fn wait_service(
    service: &windows_service::service::Service,
    wanted: windows_service::service::ServiceState,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while service.query_status()?.current_state != wanted {
        ensure!(Instant::now() < deadline, "the service did not reach {wanted:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The service opened with `access`, or `None` when it does not exist; any other failure is an
/// error, never taken as no service.
fn open_updater_service(
    access: windows_service::service::ServiceAccess,
) -> Result<Option<windows_service::service::Service>> {
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    match service_manager()?.open_service(SERVICE_NAME, access) {
        Ok(service) => Ok(Some(service)),
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            Ok(None)
        }
        Err(error) => Err(anyhow::Error::new(error).context("cannot open the update component's service")),
    }
}

fn stop_updater_service() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let Some(service) = open_updater_service(ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)? else {
        return Ok(());
    };
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        wait_service(&service, ServiceState::Stopped)?;
    }
    Ok(())
}

fn start_updater_service() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let service = service_manager()?.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
    if service.query_status()?.current_state != ServiceState::Running {
        service.start::<&OsStr>(&[])?;
        wait_service(&service, ServiceState::Running)?;
    }
    Ok(())
}

/// Swaps the resident program for `new` without the fixed path ever being empty, keeping the
/// previous one; puts the previous one back if the new one does not answer.
fn swap_program(paths: &Paths, new: &Path) -> Result<()> {
    let program = paths.program();
    // Run from the kept copy (by `recover`), that copy is the backup already and cannot be written.
    let running_previous = std::env::current_exe().is_ok_and(|me| {
        me.as_os_str()
            .eq_ignore_ascii_case(paths.previous_program().as_os_str())
    });
    if program.exists() && !running_previous {
        clash_verge_updater::state::copy_synced(&program, &paths.previous_program())?;
    }
    stop_updater_service()?;
    if let Err(error) = move_file(new, &program, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) {
        // Nothing was replaced: the program that was there runs again.
        return Err(match start_updater_service() {
            Ok(()) => error.context("the update component was not replaced"),
            Err(start) => error.context(format!(
                "the update component was not replaced, and does not start: {start:#}"
            )),
        });
    }
    if let Err(error) = start_resident() {
        let restored = (|| -> Result<()> {
            ensure!(paths.previous_program().exists(), "no previous program is kept");
            let restore = program.with_extension("restore");
            clash_verge_updater::state::copy_synced(&paths.previous_program(), &restore)?;
            stop_updater_service()?;
            move_file(&restore, &program, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)?;
            start_resident()
        })();
        return Err(match restored {
            Ok(()) => error.context("the new update component did not start; the previous one was restored"),
            Err(restore) => error.context(format!(
                "the new update component did not start, nor did the previous one: {restore:#}"
            )),
        });
    }
    Ok(())
}

/// Starts the resident component and waits until it answers.
pub fn start_resident() -> Result<()> {
    start_updater_service()?;
    clash_verge_updater::client::wait_answering(Duration::from_secs(30))
}

/// Stops the resident component, so no running process owns the state any more. True if it ran.
pub fn stop_resident() -> Result<bool> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let Some(service) = open_updater_service(ServiceAccess::QUERY_STATUS)? else {
        return Ok(false);
    };
    let running = service.query_status()?.current_state != ServiceState::Stopped;
    stop_updater_service()?;
    Ok(running)
}

/// Replaces the resident program when the payload carries a different one. The worker runs from
/// its own copy, so it outlives the service restart. A newer component is never rolled back.
fn replace_updater(paths: &Paths, payload: &Payload, journal: &mut Journal) -> Result<bool> {
    let Some(file) = payload.manifest.files_with(Role::Updater).next() else {
        return Ok(false);
    };
    if sha(&paths.program()).as_deref() == Some(file.sha256.as_str()) {
        return Ok(false);
    }
    let new = paths.program().with_extension("new");
    payload.extract(&file.path, &new)?;
    let step = journal.intent("replace_updater", json!({ "sha": file.sha256 }))?;
    swap_program(paths, &new)?;
    journal.done(step)?;
    Ok(true)
}

/// Installs this program as the resident component, or updates it, and makes sure it runs.
/// Installers call this; so does `recover`, run from a kept copy.
pub fn install_self() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType};
    let paths = Paths::system();
    prepare_layout(&paths)?;
    let me = std::env::current_exe()?;
    let program = paths.program();
    let same = sha(&me).is_some() && sha(&me) == sha(&program);
    let manager = service_manager()?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Clash Verge Updater".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: program.clone(),
        launch_arguments: vec![OsString::from("service")],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let access =
        ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::CHANGE_CONFIG;
    let existing = manager.open_service(SERVICE_NAME, access);
    if !same {
        let new = program.with_extension("new");
        std::fs::copy(&me, &new)?;
        // Flushing needs a handle with write access on Windows.
        std::fs::OpenOptions::new().write(true).open(&new)?.sync_all()?;
        if existing.is_ok() {
            swap_program(&paths, &new)?;
        } else {
            move_file(&new, &program, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)?;
        }
    }
    match existing {
        Ok(service) => service.change_config(&info)?,
        Err(_) => {
            let service = manager.create_service(&info, access)?;
            let _ = service.set_description("Applies signed Clash Verge updates without asking for elevation");
        }
    }
    start_resident()?;
    let audit = AuditLog::open(&paths.logs_dir())?;
    audit.write(
        Stream::Audit,
        Record::new(Source::Actual, if same { "installed_unchanged" } else { "installed" })
            .detail(json!({ "version": env!("CARGO_PKG_VERSION"), "sha256": sha(&program) })),
    )?;
    Ok(())
}

pub fn uninstall_self() -> Result<()> {
    use windows_service::service::ServiceAccess;
    let paths = Paths::system();
    stop_updater_service()?;
    if let Ok(service) = service_manager()?.open_service(SERVICE_NAME, ServiceAccess::DELETE) {
        service.delete()?;
    }
    let _ = std::fs::remove_dir_all(paths.bin_dir());
    Ok(())
}

// ---------------------------------------------------------------- undo and verify

pub fn undo(paths: &Paths, txn: &Txn, entry: &Entry, done: bool, writers_started: bool) -> Result<Undo> {
    let data = &entry.data;
    match entry.name.as_str() {
        "extract" => {
            let _ = std::fs::remove_dir_all(json_path(data, "dir")?);
            Ok(Undo::Clean)
        }
        "publish_core" => {
            let file = json_str(data, "file").context("no file")?;
            let current = sha(&approved_core(&file));
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
            // Restoring writes as much as publishing did, so it is confined and registered alike.
            let journal = Journal::open(&paths.txn_dir(&txn.id))?;
            let code = run_writer(
                &journal,
                entry.step,
                &json_path(data, "tool")?,
                &[
                    OsStr::new("--install-core"),
                    OsStr::new(&backup),
                    OsStr::new("--sha256"),
                    OsStr::new(&old),
                ],
                TOOL_TIMEOUT,
            )?;
            ensure!(code == 0, "restoring {file} to the service failed ({code})");
            Ok(Undo::Restored)
        }
        "swap" => {
            let target = json_path(data, "target")?;
            let old = json_path(data, "old")?;
            let current = sha(&target);
            let (old_sha, new_sha) = (json_str(data, "old_sha"), json_str(data, "new_sha"));
            if current.is_none() && old.exists() {
                move_file(&old, &target, MOVEFILE_WRITE_THROUGH)?;
                return Ok(Undo::Restored);
            }
            if current == new_sha && old.exists() {
                let aside = PathBuf::from(format!("{}.undone", target.display()));
                move_file(&target, &aside, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)?;
                move_file(&old, &target, MOVEFILE_WRITE_THROUGH)?;
                if std::fs::remove_file(&aside).is_err() {
                    delete_on_reboot(&aside);
                }
                return Ok(Undo::Restored);
            }
            if current == old_sha && !old.exists() {
                return Ok(Undo::Clean);
            }
            // The transaction created the file: there was nothing before it.
            if current == new_sha && old_sha.is_none() && !old.exists() {
                let aside = PathBuf::from(format!("{}.undone", target.display()));
                move_file(&target, &aside, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)?;
                if std::fs::remove_file(&aside).is_err() {
                    delete_on_reboot(&aside);
                }
                return Ok(Undo::Restored);
            }
            Ok(Undo::Superseded)
        }
        "display_version" => {
            let current = installed_version();
            let (old, new) = (json_str(data, "old"), json_str(data, "new"));
            if current == old {
                return Ok(Undo::Clean);
            }
            if current != new {
                return Ok(Undo::Superseded);
            }
            write_registry("DisplayVersion", old.as_deref().unwrap_or_default())?;
            Ok(Undo::Restored)
        }
        "replace_updater" => Ok(Undo::Clean),
        "run_installer" if !writers_started => Ok(Undo::Clean),
        "run_installer" => {
            let _ = done;
            bail!("what the installer changed cannot be undone; repair with the same version")
        }
        other => bail!("unknown step {other}"),
    }
}

/// Confirms the installed state matches the payload: version, layout, files, the service's
/// approved cores and binary, and the component itself.
pub fn verify(manifest: &Manifest) -> Result<()> {
    let install = install_dir().context("no install directory")?;
    ensure!(
        installed_version().as_deref() == Some(manifest.version.as_str()),
        "the registered version is {:?}",
        installed_version()
    );
    if let Some(fingerprint) = &manifest.fingerprint {
        ensure!(
            installed_fingerprint().as_ref() == Some(fingerprint),
            "the install fingerprint differs"
        );
    }
    if manifest.kind == Kind::Patch {
        for name in [GUI_EXE, "verge-mihomo.exe", "verge-mihomo-alpha.exe"] {
            let expected = &manifest.file(name).context("missing file")?.sha256;
            ensure!(sha(&install.join(name)).as_ref() == Some(expected), "{name} differs");
        }
    }
    // The installer always installs the service; a patch keeps whatever was there.
    ensure!(
        manifest.kind != Kind::Full || service_binary().exists(),
        "the service is not installed"
    );
    if service_binary().exists() {
        for core in &manifest.cores {
            let file = manifest.core_file_name(&core.name);
            ensure!(
                sha(&approved_core(&file)).as_deref() == Some(core.sha256.as_str()),
                "the service runs a different {}",
                core.name
            );
        }
        if let Some(service) = &manifest.service {
            ensure!(
                sha(&service_binary()).as_ref() == Some(service),
                "the service binary differs"
            );
        }
        ensure!(service_running("clash_verge_service"), "the service is not running");
    }
    if let Some(updater) = &manifest.updater {
        ensure!(
            sha(&Paths::system().program()).as_ref() == Some(updater),
            "the update component differs"
        );
    }
    Ok(())
}

fn service_running(name: &str) -> bool {
    use windows_service::service::{ServiceAccess, ServiceState};
    service_manager()
        .and_then(|manager| Ok(manager.open_service(name, ServiceAccess::QUERY_STATUS)?))
        .and_then(|service| Ok(service.query_status()?))
        .is_ok_and(|status| status.current_state == ServiceState::Running)
}

// ---------------------------------------------------------------- service host and pipe

windows_service::define_windows_service!(ffi_service_main, service_main);

pub fn run_service() -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("not started by the service control manager")?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service_inner() {
        logger::log(&format!("service: {error:#}"));
    }
}

fn run_service_inner() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let report = |state, accept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    report(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    )?;
    logger::log(&format!("service started, version {}", env!("CARGO_PKG_VERSION")));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async move {
        let daemon = Daemon::open()?;
        daemon.start().await;
        let stop = tokio::task::spawn_blocking(move || stop_rx.recv());
        tokio::select! {
            served = serve(daemon) => served,
            _ = stop => Ok(()),
        }
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    logger::log("service stopped");
    report(ServiceState::Stopped, ServiceControlAccept::empty())?;
    result
}

/// The pipe's security descriptor, built once and kept for the life of the service.
fn pipe_descriptor() -> Result<usize> {
    static DESCRIPTOR: OnceLock<usize> = OnceLock::new();
    if let Some(descriptor) = DESCRIPTOR.get() {
        return Ok(*descriptor);
    }
    let sddl = wide(OsStr::new(PIPE_SDDL));
    let mut descriptor = std::ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "invalid pipe security descriptor"
    );
    Ok(*DESCRIPTOR.get_or_init(|| descriptor as usize))
}

fn create_pipe(first: bool) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: pipe_descriptor()? as *mut c_void,
        bInheritHandle: 0,
    };
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(PIPE_NAME, (&raw mut security).cast::<c_void>())
    }?;
    Ok(server)
}

fn client_of(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> Result<Initiator> {
    let (mut pid, mut session) = (0, 0);
    ensure!(
        unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } != 0,
        "cannot identify the client"
    );
    unsafe { GetNamedPipeClientSessionId(pipe.as_raw_handle(), &mut session) };
    let info = ProcessInfo::query(pid)?;
    Ok(Initiator {
        user: info.user,
        id: info.sid,
        pid,
        session: Some(session),
    })
}

async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let mut server = create_pipe(true).context("failed to open the pipe")?;
    loop {
        server.connect().await?;
        let connected = server;
        server = create_pipe(false)?;
        match client_of(&connected) {
            Ok(initiator) => {
                tokio::spawn(Arc::clone(&daemon).handle(connected, initiator));
            }
            Err(error) => logger::log(&format!("pipe: {error:#}")),
        }
    }
}
