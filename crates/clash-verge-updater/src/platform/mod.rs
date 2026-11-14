//! Everything that differs per operating system. Each module offers the same functions.

// Every platform offers the same signatures, so some return values are trivial on one of them.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
mod linux;
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_trust;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

/// A real directory (no symlink) that root owns and nobody else can write to.
#[cfg(unix)]
pub fn owned_by_root(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

/// The digest of `path`, `None` when there is no such file. A file that is there but cannot be read
/// is an error: it must not be taken for one that is not.
pub fn existing_sha(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    use anyhow::Context as _;
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
        Ok(_) => clash_verge_updater::payload::sha256_file(path).map(Some),
    }
}

/// Creates `dir`, a directory of a transaction, so that it and the one above it are entered in
/// their parents on disk: what recovery needs there must not vanish in a crash.
pub fn create_dir_durable(dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    clash_verge_updater::state::sync_parent(dir)?;
    if let Some(parent) = dir.parent() {
        clash_verge_updater::state::sync_parent(parent)?;
    }
    Ok(())
}

pub fn set_modified(path: &std::path::Path, time: std::time::SystemTime) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.set_modified(time)
        .with_context(|| format!("failed to set the time of {}", path.display()))?;
    file.sync_all()?;
    Ok(())
}

/// Copies `from` to `to` with its modification time: the service tells the app's core from its
/// approved copy by size and time, and the upstream tool carries the time of what it publishes.
pub fn copy_keeping_time(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    let modified = std::fs::metadata(from)?.modified()?;
    clash_verge_updater::state::copy_synced(from, to)?;
    set_modified(to, modified)
}

/// The core of a core payload under each core name, in the transaction directory: the upstream
/// tool names what it publishes after the file it is given. Each copy carries the build time.
pub fn core_copies(
    paths: &clash_verge_updater::paths::Paths,
    txn: &clash_verge_updater::state::Txn,
    payload: &clash_verge_updater::payload::Payload,
) -> anyhow::Result<Vec<(clash_verge_updater::manifest::CoreEntry, std::path::PathBuf)>> {
    use anyhow::Context as _;
    let manifest = &payload.manifest;
    let built_at = chrono::DateTime::parse_from_rfc3339(&manifest.built_at)
        .with_context(|| format!("invalid build time {:?}", manifest.built_at))?;
    let dir = paths.txn_dir(&txn.id).join("cores");
    create_dir_durable(&dir)?;
    let copies = manifest
        .cores
        .iter()
        .map(|core| {
            let copy = dir.join(manifest.core_file_name(&core.name));
            payload.extract(&manifest.core_source(&core.name), &copy)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755))?;
            }
            set_modified(&copy, built_at.into())?;
            Ok((core.clone(), copy))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Some((_, copy)) = copies.first() {
        clash_verge_updater::state::sync_parent(copy)?;
    }
    Ok(copies)
}

/// A path root may write or run: it and every directory above it are root's, writable by nobody
/// else, and none is a link.
#[cfg(unix)]
pub fn root_only(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    path.ancestors().all(|part| {
        std::fs::symlink_metadata(part)
            .is_ok_and(|meta| !meta.file_type().is_symlink() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
    })
}

/// An approved core is read and kept to restore only where nobody but root can have changed it:
/// putting it back through the service tool would publish whatever it holds. Without the
/// directory there is nothing to read.
#[cfg(unix)]
pub fn check_approved(path: &std::path::Path) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(path);
    if absent(dir) {
        return Ok(());
    }
    anyhow::ensure!(root_only(dir), "{} is not protected", dir.display());
    anyhow::ensure!(absent(path) || root_only(path), "{} is not protected", path.display());
    Ok(())
}

/// Whether there is no file at `path` at all.
#[cfg(unix)]
fn absent(path: &std::path::Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// Makes a core the service tool published durable: the tool renames it into place, creating its
/// directory if need be, without flushing either.
#[cfg(unix)]
pub fn sync_published(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to flush {}", path.display()))?;
    sync_dirs_above(path)
}

/// Flushes the directory of `path` and the one above it.
#[cfg(unix)]
fn sync_dirs_above(path: &std::path::Path) -> anyhow::Result<()> {
    clash_verge_updater::state::sync_parent(path)?;
    match path.parent() {
        Some(dir) => clash_verge_updater::state::sync_parent(dir),
        None => Ok(()),
    }
}

/// Makes what is at `path` durable, a file or its absence: an undo that finds its work done may
/// follow one that stopped before it flushed.
#[cfg(unix)]
pub fn sync_current(path: &std::path::Path) -> anyhow::Result<()> {
    if path.exists() {
        sync_published(path)
    } else if path.parent().is_some_and(std::path::Path::exists) {
        sync_dirs_above(path)
    } else {
        Ok(())
    }
}

/// What undoing a published core calls for, from the approved copy as it is now.
#[derive(Debug, PartialEq, Eq)]
pub enum CoreUndo {
    Clean,
    Superseded,
    /// Publish the backup again with the service tool.
    Restore,
    /// Take back a core that was not there before.
    Remove,
}

/// `taken_back`: a core the service did not have before goes again; an app update leaves it.
pub fn core_undo(
    current: Option<&str>,
    old: Option<&str>,
    new: Option<&str>,
    backup: bool,
    taken_back: bool,
) -> CoreUndo {
    if current == old {
        return CoreUndo::Clean;
    }
    // On Windows the tool replaces a running core in two moves, between which the approved path
    // is empty; elsewhere it renames in one step, so a missing core is someone else's doing.
    if cfg!(windows) && current.is_none() && old.is_some() && backup {
        return CoreUndo::Restore;
    }
    if current != new {
        return CoreUndo::Superseded;
    }
    match (old.is_some() && backup, taken_back) {
        (true, _) => CoreUndo::Restore,
        (false, true) => CoreUndo::Remove,
        (false, false) => CoreUndo::Clean,
    }
}

/// How undoing one journal step went.
pub enum Undo {
    /// The step's change was reverted.
    #[cfg_attr(not(windows), allow(dead_code))]
    Restored,
    /// The step had not changed anything, or nothing needs reverting.
    Clean,
    /// Something else changed the same thing since; it is left as it is.
    Superseded,
}

/// Quotes command-line arguments the way `CommandLineToArgvW` and POSIX shells both read
/// unquoted paths without spaces; ours never contain quotes.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn command_line(program: &std::path::Path, args: &[&std::ffi::OsStr]) -> std::ffi::OsString {
    let mut line = std::ffi::OsString::from("\"");
    line.push(program.as_os_str());
    line.push("\"");
    for arg in args {
        line.push(" ");
        if arg.to_string_lossy().contains(' ') {
            line.push("\"");
            line.push(arg);
            line.push("\"");
        } else {
            line.push(arg);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{CoreUndo, core_undo};

    #[test]
    fn undoing_a_published_core_follows_what_is_there_now() {
        let (old, new) = (Some("o"), Some("n"));
        assert_eq!(core_undo(old, old, new, true, true), CoreUndo::Clean);
        assert_eq!(core_undo(new, old, new, true, true), CoreUndo::Restore);
        let midway = if cfg!(windows) {
            CoreUndo::Restore
        } else {
            CoreUndo::Superseded
        };
        assert_eq!(
            core_undo(None, old, new, true, true),
            midway,
            "between the tool's two moves"
        );
        assert_eq!(core_undo(Some("x"), old, new, true, true), CoreUndo::Superseded);
        assert_eq!(core_undo(new, None, new, false, true), CoreUndo::Remove);
        assert_eq!(core_undo(new, None, new, false, false), CoreUndo::Clean);
        assert_eq!(core_undo(None, None, new, false, true), CoreUndo::Clean);
    }
}
