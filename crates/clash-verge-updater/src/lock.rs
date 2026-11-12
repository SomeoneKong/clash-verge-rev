//! The machine-wide update lock every writer holds: the worker, recovery, and on Windows the NSIS
//! installer. The operating system releases it when the holder exits, so a crash cannot leave it
//! held.

use anyhow::{Result, bail};
use clash_verge_updater::paths::Paths;
use std::cell::Cell;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

thread_local! {
    /// This thread holds the lock: a second acquisition here would wait on itself. Other threads
    /// of the process take the lock for real.
    static HELD: Cell<bool> = const { Cell::new(false) };
}

pub struct Lock {
    _file: std::fs::File,
    /// Kept and released on the thread that took it, the one the flag above is about.
    _thread: PhantomData<*const ()>,
}

impl Lock {
    const fn new(file: std::fs::File) -> Self {
        Self {
            _file: file,
            _thread: PhantomData,
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        HELD.with(|held| held.set(false));
    }
}

pub fn held() -> bool {
    HELD.with(Cell::get)
}

pub fn acquire(paths: &Paths, timeout: Duration) -> Result<Lock> {
    let deadline = Instant::now() + timeout;
    loop {
        match try_acquire(paths)? {
            Some(lock) => {
                HELD.with(|held| held.set(true));
                return Ok(lock);
            }
            None if Instant::now() >= deadline => bail!("another update is in progress"),
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// An exclusive open of `update.lock`; the file is readable by SYSTEM and Administrators only, so
/// no ordinary user can hold it to block updates.
#[cfg(windows)]
fn try_acquire(paths: &Paths) -> Result<Option<Lock>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(paths.lock_file())
    {
        Ok(file) => Ok(Some(Lock::new(file))),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// dpkg's frontend lock, which every package manager honours; the worker installs with dpkg
/// while holding it (`DPKG_FRONTEND_LOCKED=1`).
#[cfg(target_os = "linux")]
fn try_acquire(_paths: &Paths) -> Result<Option<Lock>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o640)
        .open("/var/lib/dpkg/lock-frontend")?;
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(Some(Lock::new(file)));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => Ok(None),
        _ => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn try_acquire(paths: &Paths) -> Result<Option<Lock>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.lock_file())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(Lock::new(file)));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(error.into()),
    }
}
