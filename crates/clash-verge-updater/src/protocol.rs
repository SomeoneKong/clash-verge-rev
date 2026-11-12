//! IPC between the GUI and the component: length-prefixed JSON frames. A `Stage` request is
//! followed by the payload bytes themselves.

use crate::manifest::Kind;
use crate::state::{Finished, Txn};
use anyhow::{Result, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub const PROTOCOL: u32 = 1;
pub const MAX_FRAME: u32 = 64 * 1024;

/// Where a request came from, as the client states it. Who sent it is taken from the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub kind: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserPhaseEvent {
    Begin,
    Replaced,
    NotReplaced,
    NotRestored,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
    },
    Stage {
        trigger: Trigger,
        size: u64,
        signatures: Vec<String>,
    },
    Apply {
        txn: String,
        relaunch: bool,
    },
    UserPhase {
        txn: String,
        event: UserPhaseEvent,
    },
    Repair {
        txn: Option<String>,
        /// A full repair runs the installer, which the GUI leaves for; it is then started again.
        #[serde(default)]
        relaunch: bool,
    },
    Status {
        txn: String,
    },
    Cancel {
        txn: String,
    },
    /// macOS, root only: gives up a user phase whose initiator will not come back.
    Abandon {
        txn: String,
    },
    Report {
        claim: serde_json::Value,
    },
    Audit {
        before_seq: Option<u64>,
        limit: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub updater_version: String,
    pub platform: String,
    pub installed_version: Option<String>,
    pub floor: Option<String>,
    pub fingerprint: Option<String>,
    pub current: Option<Txn>,
    pub needs_repair: Option<Finished>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Staged {
    pub txn: String,
    pub version: String,
    pub kind: Kind,
    pub size: u64,
    pub sha256: String,
}

/// A transaction as the component reports it: still running, or how it ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxnView {
    Staged(Staged),
    Running(Txn),
    Finished(Finished),
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Hello(Box<Hello>),
    Staged(Staged),
    Txn(TxnView),
    Audit { records: Vec<crate::audit::Record> },
    Done,
    Busy { current: TxnView },
    Rejected { reason: String },
    Error { message: String },
}

pub async fn write_frame<W: AsyncWrite + Unpin + Send, T: Serialize + Sync>(writer: &mut W, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    ensure!(body.len() <= MAX_FRAME as usize, "message too large");
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin + Send, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len);
    ensure!(len <= MAX_FRAME, "message too large");
    let mut body = vec![0; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
