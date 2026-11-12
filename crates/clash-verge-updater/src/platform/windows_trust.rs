//! Whether SYSTEM may write or run something at a path: the path and every directory above it must
//! be changeable only by SYSTEM, Administrators or TrustedInstaller. Ported from the service's
//! `trusted_core_location.rs` (clash-verge-service-ipc, GPL-3.0).

use anyhow::{Result, bail};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Component, Path, Prefix};
use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, IsValidSid, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    FILE_WRITE_DATA, FILE_WRITE_EA, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};

/// `NT SERVICE\TrustedInstaller`, the owner Windows leaves on `%ProgramFiles%`.
const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const VOLUME_ROOT_MINIMAL_CAPABILITY_SID: &str =
    "S-1-15-3-65536-1888954469-739942743-1668119174-2468466756-4239452838-1296943325-355587736-700089176";
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
const INHERIT_ONLY_ACE_FLAG: u8 = 0x08;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Rights that put other content behind the target itself; on a directory they also let a
/// principal add entries to it.
const TARGET_HIJACK_RIGHTS: u32 = DELETE
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// Rights that replace a directory above the target. Adding a subdirectory is left out: stock
/// `C:\` grants it to Authenticated Users, but it cannot replace an existing child.
const DIRECTORY_HIJACK_RIGHTS: u32 = DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        use std::os::windows::fs::MetadataExt as _;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    })
}

/// Checks that `path` itself is no reparse point and is owned by SYSTEM, Administrators or
/// TrustedInstaller; its DACL is left to the caller.
pub fn check_owner(path: &Path) -> Result<()> {
    if is_reparse_point(path) {
        bail!("{} is a reparse point", path.display());
    }
    let handle = open_for_review(path)?;
    let security = SecurityInfo::read(handle.as_raw_handle(), &path.display().to_string())?;
    if !TrustedSids::new()?.contains(security.owner) {
        bail!(
            "{} is owned by an account other than SYSTEM, Administrators or TrustedInstaller",
            path.display()
        );
    }
    Ok(())
}

/// Checks `path` (a file or directory) and every directory above it; reparse points are refused.
pub fn check(path: &Path) -> Result<()> {
    // The path as given is what later I/O goes through, so a junction anywhere on it is refused
    // before canonicalizing, which would resolve it away.
    for component in path.ancestors() {
        if is_reparse_point(component) {
            bail!("{} is a reparse point", component.display());
        }
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    let trusted = TrustedSids::new()?;
    for (index, component) in canonical.ancestors().enumerate() {
        let rights = if index == 0 {
            TARGET_HIJACK_RIGHTS
        } else {
            DIRECTORY_HIJACK_RIGHTS
        };
        if is_reparse_point(component) {
            bail!("{} is a reparse point", component.display());
        }
        let handle = open_for_review(component)?;
        review(handle.as_raw_handle(), component, rights, &trusted)?;
    }
    Ok(())
}

fn is_local_volume_root(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        && matches!(components.next(), Some(Component::RootDir))
        && components.next().is_none()
}

fn review(handle: *mut c_void, component: &Path, rights: u32, trusted: &TrustedSids) -> Result<()> {
    let label = component.display().to_string();
    let security = SecurityInfo::read(handle, &label)?;
    let volume_root = is_local_volume_root(component);
    if !trusted.contains(security.owner) {
        bail!("{label} is owned by an account other than SYSTEM, Administrators or TrustedInstaller");
    }
    if security.dacl.is_null() {
        bail!("{label} has no DACL");
    }
    // A volume root cannot be deleted or renamed, so DELETE on it cannot move the target.
    let inert = if volume_root { DELETE } else { 0 };
    for index in 0..u32::from(unsafe { (*security.dacl).AceCount }) {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(security.dacl, index, &mut ace) } == 0 || ace.is_null() {
            bail!("{label} has an unreadable DACL");
        }
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        if header.AceFlags & INHERIT_ONLY_ACE_FLAG != 0 {
            continue;
        }
        match header.AceType {
            ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => continue,
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {}
            _ => bail!("{label} carries an ACE type this check cannot evaluate"),
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Mask & rights & !inert == 0 {
            continue;
        }
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast::<c_void>();
        if trusted.contains(sid) {
            continue;
        }
        // This stock AppContainer capability on a volume root cannot authorize a write alone.
        if volume_root
            && rights == DIRECTORY_HIJACK_RIGHTS
            && header.AceType == ACCESS_ALLOWED_ACE_TYPE
            && header.AceFlags == 0x03
            && allowed.Mask == FILE_ALL_ACCESS
            && unsafe { EqualSid(sid, trusted.volume_capability.as_ptr()) } != 0
        {
            continue;
        }
        bail!(
            "{label} can be changed by {} (mask 0x{:08X})",
            sid_string(sid),
            allowed.Mask
        );
    }
    Ok(())
}

fn sid_string(sid: PSID) -> String {
    let mut wide = std::ptr::null_mut();
    if unsafe { IsValidSid(sid) } == 0 || unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return "<invalid SID>".to_owned();
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

fn open_for_review(path: &Path) -> Result<std::fs::File> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        bail!("{} could not be opened for review", path.display());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

struct TrustedSids {
    trusted_installer: LocalSid,
    volume_capability: LocalSid,
}

impl TrustedSids {
    fn new() -> Result<Self> {
        Ok(Self {
            trusted_installer: LocalSid::from_string(TRUSTED_INSTALLER_SID)?,
            volume_capability: LocalSid::from_string(VOLUME_ROOT_MINIMAL_CAPABILITY_SID)?,
        })
    }

    fn contains(&self, sid: PSID) -> bool {
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return false;
        }
        unsafe {
            IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                || EqualSid(sid, self.trusted_installer.as_ptr()) != 0
        }
    }
}

struct LocalSid(*mut c_void);

impl LocalSid {
    fn from_string(value: &str) -> Result<Self> {
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let mut sid = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 || unsafe { IsValidSid(sid) } == 0 {
            bail!("SID {value} could not be built");
        }
        Ok(Self(sid))
    }

    const fn as_ptr(&self) -> PSID {
        self.0
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

struct SecurityInfo {
    owner: PSID,
    dacl: *mut ACL,
    _descriptor: LocalDescriptor,
}

impl SecurityInfo {
    fn read(handle: *mut c_void, label: &str) -> Result<Self> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 || descriptor.is_null() {
            bail!("{label} security could not be inspected (Win32 {status})");
        }
        Ok(Self {
            owner,
            dacl,
            _descriptor: LocalDescriptor(descriptor),
        })
    }
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}
