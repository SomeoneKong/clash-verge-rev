//! Process identity helpers: who a process runs as, and its creation time so that a later process
//! reusing the same id is not mistaken for it.

use crate::state::ProcessRef;
use anyhow::Result;

/// The creation time of a running process, or `None` once it is gone.
pub fn process_created(pid: u32) -> Option<u64> {
    imp::process_created(pid)
}

/// Whether the very process `process` refers to still runs.
pub fn is_running(process: ProcessRef) -> bool {
    process_created(process.pid) == Some(process.created)
}

pub fn current_process() -> ProcessRef {
    let pid = std::process::id();
    ProcessRef {
        pid,
        created: process_created(pid).unwrap_or_default(),
    }
}

#[cfg(windows)]
pub use imp::{ProcessInfo, terminate, token_user_sid};

#[cfg(unix)]
pub fn user_name(uid: u32) -> String {
    imp::user_name(uid).unwrap_or_else(|| uid.to_string())
}

pub fn ensure_supported() -> Result<()> {
    anyhow::ensure!(
        crate::manifest::Platform::current().is_some(),
        "no update payloads exist for this platform"
    );
    Ok(())
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context as _, Result};
    use std::ffi::c_void;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LocalFree, WAIT_TIMEOUT};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, IsWellKnownSid, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinLocalSystemSid,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
    };

    pub struct ProcessInfo {
        pub image: PathBuf,
        pub is_system: bool,
        pub sid: String,
        pub user: String,
        pub created: u64,
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Terminates `process` only if it is still the one created at that time, checked on the very
    /// handle that terminates it, so a reused PID is never hit.
    pub fn terminate(process: crate::state::ProcessRef) -> bool {
        use windows_sys::Win32::System::Threading::{PROCESS_TERMINATE, TerminateProcess};
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE;
        let handle = unsafe { OpenProcess(access, 0, process.pid) };
        if handle.is_null() {
            return false;
        }
        let handle = Handle(handle);
        alive(&handle) && creation(&handle) == Some(process.created) && unsafe { TerminateProcess(handle.0, 1) } != 0
    }

    /// A process is gone only once its handle is signaled; its exit code changes before it lets
    /// go of its files.
    fn alive(handle: &Handle) -> bool {
        unsafe { WaitForSingleObject(handle.0, 0) == WAIT_TIMEOUT }
    }

    fn open(pid: u32) -> Option<Handle> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        (!handle.is_null()).then_some(Handle(handle))
    }

    fn creation(handle: &Handle) -> Option<u64> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        (unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } != 0)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    pub fn process_created(pid: u32) -> Option<u64> {
        let handle = open(pid)?;
        if !alive(&handle) {
            return None;
        }
        creation(&handle)
    }

    impl ProcessInfo {
        pub fn query(pid: u32) -> Result<Self> {
            let handle = open(pid).with_context(|| format!("cannot open process {pid}"))?;
            let mut buffer = vec![0u16; 32768];
            let mut len = buffer.len() as u32;
            anyhow::ensure!(
                unsafe { QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len) } != 0,
                "cannot read the image of process {pid}"
            );
            let image = PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
            let created = creation(&handle).unwrap_or_default();

            let mut token = std::ptr::null_mut();
            anyhow::ensure!(
                unsafe { OpenProcessToken(handle.0, TOKEN_QUERY, &mut token) } != 0,
                "cannot open the token of process {pid}"
            );
            let token = Handle(token);
            let mut needed = 0;
            unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
            let mut info = vec![0u8; needed as usize];
            anyhow::ensure!(
                unsafe { GetTokenInformation(token.0, TokenUser, info.as_mut_ptr().cast(), needed, &mut needed) } != 0,
                "cannot read the user of process {pid}"
            );
            let sid = unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            Ok(Self {
                image,
                is_system: unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0,
                sid: sid_string(sid),
                user: account_name(sid),
                created,
            })
        }
    }

    /// The SID of the user a token belongs to.
    ///
    /// # Safety
    /// `token` must be a valid token handle opened with `TOKEN_QUERY`.
    pub unsafe fn token_user_sid(token: HANDLE) -> Option<String> {
        let mut needed = 0;
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut info = vec![0u8; needed as usize];
        if unsafe { GetTokenInformation(token, TokenUser, info.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
            return None;
        }
        Some(sid_string(unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid }))
    }

    pub fn sid_string(sid: *mut c_void) -> String {
        let mut wide = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
            return String::new();
        }
        let mut len = 0;
        unsafe {
            while *wide.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
            LocalFree(wide.cast());
            value
        }
    }

    fn account_name(sid: *mut c_void) -> String {
        let mut name = vec![0u16; 256];
        let mut domain = vec![0u16; 256];
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind: SID_NAME_USE = 0;
        if unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                sid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        } == 0
        {
            return sid_string(sid);
        }
        format!(
            "{}\\{}",
            String::from_utf16_lossy(&domain[..domain_len as usize]),
            String::from_utf16_lossy(&name[..name_len as usize])
        )
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn process_created(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may contain spaces; fields after it are space separated.
        let rest = &stat[stat.rfind(')')? + 2..];
        let state = rest.split(' ').next()?;
        if state == "Z" || state == "X" {
            return None;
        }
        rest.split(' ').nth(19)?.parse().ok()
    }

    pub fn user_name(uid: u32) -> Option<String> {
        super::passwd_name(uid)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub fn process_created(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if read != size || info.pbi_status == libc::SZOMB {
            return None;
        }
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn user_name(uid: u32) -> Option<String> {
        super::passwd_name(uid)
    }
}

#[cfg(unix)]
fn passwd_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe { libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result) };
    if status != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}
