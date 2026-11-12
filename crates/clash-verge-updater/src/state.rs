//! `state.json`: the version floor and the transaction in progress. It doubles as the
//! machine-wide "update in progress" marker the GUI reads when it cannot reach the component.

use crate::manifest::Kind;
use crate::protocol::Trigger;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Applying,
    /// macOS: applied, the GUI has not started replacing the bundle.
    UserPhasePending,
    /// macOS: the GUI is replacing the bundle.
    UserPhaseBegun,
    /// The writers of an interrupted transaction could not be confirmed stopped.
    RecoveryBlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    RolledBack,
    /// Nothing was changed.
    Failed,
    /// Something was changed that could not be undone; a repair with the kept payload can finish it.
    NeedsRepair,
    /// Another installation changed the system after the transaction started; it was not undone.
    Superseded,
    /// macOS: a user phase that will not be finished was given up.
    Abandoned,
}

/// Who started a transaction, as the kernel reported it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initiator {
    pub user: String,
    /// SID on Windows, uid elsewhere.
    pub id: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

/// A process told apart from a later one reusing its id by its creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRef {
    pub pid: u32,
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Txn {
    pub id: String,
    pub version: String,
    pub from_version: Option<String>,
    pub kind: Kind,
    pub phase: Phase,
    pub initiator: Initiator,
    pub trigger: Trigger,
    pub started_at: String,
    pub payload_sha256: String,
    pub payload_size: u64,
    pub key_id: String,
    pub relaunch: bool,
    #[serde(default)]
    pub repair: bool,
    /// Length of the external installs file when the transaction started.
    #[serde(default)]
    pub external_mark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<ProcessRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finished {
    pub id: String,
    pub version: String,
    pub from_version: Option<String>,
    pub kind: Kind,
    pub outcome: Outcome,
    pub finished_at: String,
    pub error: Option<String>,
    pub initiator: Initiator,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub format: u32,
    /// No payload at or below this version is accepted, except to repair.
    pub floor: Option<String>,
    /// The installed version last seen, to notice installations made outside the component.
    pub observed_version: Option<String>,
    pub txn: Option<Txn>,
    pub needs_repair: Option<Finished>,
    pub last: Option<Finished>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                format: FORMAT,
                ..Self::default()
            }),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Replaces `path` so that a crash leaves either the old or the new content, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = std::path::PathBuf::from(temp);
    {
        let mut file = std::fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    sync_parent(path)
}

/// Copies `from` to `to` and flushes the copy, so a backup is on disk before what it backs up
/// changes.
///
/// On Unix the copy is a new file of the caller's: `std::fs::copy` clones on macOS and keeps the
/// owner of `from`, and a program that root runs must not be writable by whoever owned its source.
pub fn copy_synced(from: &Path, to: &Path) -> Result<()> {
    let context = || format!("failed to copy {} to {}", from.display(), to.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut source = std::fs::File::open(from).with_context(context)?;
        let mode = source.metadata().with_context(context)?.permissions().mode() & 0o777;
        match std::fs::remove_file(to) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error).with_context(context),
            _ => {}
        }
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .with_context(context)?;
        std::io::copy(&mut source, &mut target).with_context(context)?;
        target.set_permissions(std::fs::Permissions::from_mode(mode))?;
        target.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(from, to).with_context(context)?;
        std::fs::OpenOptions::new().write(true).open(to)?.sync_all()?;
    }
    sync_parent(to)
}

/// Makes an entry created or renamed in `path`'s directory durable; Windows has nothing to flush
/// for it.
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
