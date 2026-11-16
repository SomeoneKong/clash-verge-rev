use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
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
pub(crate) fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// `MoveFileEx` returns only once the rename is on disk when asked to write through.
#[cfg(windows)]
pub(crate) fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
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

/// Appends lines to `path` and makes them durable, creating the file when needed; a file
/// created here is made durable in its directory too.
///
/// Appends go on from a whole line: a torn one an earlier failure left is cut off first, and a
/// failed append is cut back off. The directory is made durable every time, as an earlier
/// attempt may have created the file and failed.
pub fn append_lines(path: &Path, lines: &[Vec<u8>]) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    if lines.is_empty() {
        return Ok(());
    }
    // not opened for appending: such a handle cannot be cut back on Windows
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut length = file.seek(SeekFrom::End(0))?;
    if length > 0 {
        let mut last = [0u8];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            let mut content = Vec::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_end(&mut content)?;
            length = content
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |at| at as u64 + 1);
            file.set_len(length)?;
        }
        file.seek(SeekFrom::Start(length))?;
    }
    let mut bytes = Vec::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        if file.set_len(length).and_then(|()| file.sync_all()).is_err() {
            bail!(
                "failed to append to {} and to cut the append off: {error}",
                path.display()
            );
        }
        return Err(error).with_context(|| format!("failed to append to {}", path.display()));
    }
    sync_parent(path)
}

/// Copies a damaged file aside, keeping it for inspection while it is repaired in place.
pub fn preserve(path: &Path) -> Result<PathBuf> {
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
    std::fs::copy(path, &target).with_context(|| format!("failed to copy aside {}", path.display()))?;
    // flushing needs write access on Windows
    std::fs::OpenOptions::new().write(true).open(&target)?.sync_all()?;
    sync_parent(&target)?;
    Ok(target)
}

/// The lines of a file of one JSON value per line.
pub struct Lines<T> {
    pub values: Vec<T>,
    /// The last line is cut short, as a write that a crash interrupted leaves it.
    pub torn_tail: bool,
    /// Lines before the last could not be read.
    pub damaged: bool,
}

pub fn read_lines<T: DeserializeOwned>(path: &Path) -> Result<Option<Lines<T>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut lines = Lines {
        values: Vec::new(),
        torn_tail: false,
        damaged: false,
    };
    let ends_complete = bytes.last().is_none_or(|byte| *byte == b'\n');
    let parts: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    let count = parts.len();
    for (index, part) in parts.into_iter().enumerate() {
        let last = index + 1 == count;
        if part.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(part) {
            Ok(value) if !(last && !ends_complete) => lines.values.push(value),
            _ if last => lines.torn_tail = true,
            _ => lines.damaged = true,
        }
    }
    Ok(Some(lines))
}

/// Replaces a file of lines with `values`, one per line.
pub fn rewrite_lines<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    write_atomic(path, &bytes)
}
