use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Serialize, de::DeserializeOwned};

pub enum Loaded<T> {
    Missing,
    Corrupt,
    Ok(T),
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Loaded<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).map_or(Loaded::Corrupt, Loaded::Ok)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Loaded::Missing),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

/// Moves an unreadable file aside, keeping it for inspection instead of overwriting it.
pub fn quarantine(path: &Path) -> Result<PathBuf> {
    let mut index = 0;
    let target = loop {
        let mut name = path.as_os_str().to_owned();
        name.push(if index == 0 {
            ".corrupt".to_owned()
        } else {
            format!(".corrupt.{index}")
        });
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            break candidate;
        }
        index += 1;
    };
    std::fs::rename(path, &target).with_context(|| format!("failed to move aside {}", path.display()))?;
    sync_parent(path)?;
    Ok(target)
}

/// Replaces `path` so that a crash or power loss leaves either the old or the new content.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    {
        let mut file = std::fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    replace(&temp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    sync_parent(path)
}

pub fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn create_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    sync_parent(path)
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// `MoveFileEx` returns only once the rename is on disk when asked to write through.
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let wide = |path: &Path| path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both paths are NUL-terminated UTF-16 buffers that outlive the call.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Makes an entry created, renamed or removed in `path`'s directory durable.
#[cfg(unix)]
pub fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("failed to flush {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
pub const fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
