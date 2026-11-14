//! The per-transaction journal. Every change is recorded as an intent before it is made and as done
//! after, each flushed to disk, so recovery knows what may have happened. Writers are registered
//! before they start, so recovery can wait for every one of them to stop.

use anyhow::{Context as _, Result};
use clash_verge_updater::state::ProcessRef;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Intent,
    Done,
    /// A process that may write; recovery waits until it is gone.
    Writer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub step: u32,
    pub name: String,
    pub mark: Mark,
    #[serde(default)]
    pub data: serde_json::Value,
}

pub struct Journal {
    path: PathBuf,
    next: u32,
}

impl Journal {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("journal.jsonl");
        // A line torn by a crash is cut before anything is appended after it.
        if let Ok(bytes) = std::fs::read(&path)
            && !bytes.is_empty()
            && !bytes.ends_with(b"\n")
        {
            let keep = bytes
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |index| index + 1);
            let file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.set_len(keep as u64)?;
            file.sync_all()?;
        }
        let next = read(dir)?
            .iter()
            .filter(|entry| entry.step != u32::MAX)
            .map(|entry| entry.step + 1)
            .max()
            .unwrap_or(0);
        Ok(Self { path, next })
    }

    fn append(&self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let created = !self.path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        file.write_all(&line)?;
        file.sync_all()?;
        // A journal lost with its directory entry would make recovery think nothing changed.
        if created {
            clash_verge_updater::state::sync_parent(&self.path)?;
        }
        Ok(())
    }

    /// Records what is about to change and how to undo it; returns the step number.
    pub fn intent(&mut self, name: &str, data: serde_json::Value) -> Result<u32> {
        let step = self.next;
        self.next += 1;
        self.append(&Entry {
            step,
            name: name.to_owned(),
            mark: Mark::Intent,
            data,
        })?;
        Ok(step)
    }

    pub fn done(&self, step: u32) -> Result<()> {
        self.append(&Entry {
            step,
            name: String::new(),
            mark: Mark::Done,
            data: serde_json::Value::Null,
        })
    }

    /// Registers a process started for `step` before it is let run.
    pub fn writer(&self, step: u32, process: ProcessRef) -> Result<()> {
        self.append(&Entry {
            step,
            name: "writer".into(),
            mark: Mark::Writer,
            data: serde_json::to_value(process)?,
        })
    }
}

/// Every entry, in the order written. Only a torn last line, left by a crash while appending, is
/// ignored; anything else unreadable is an error, since recovery must not guess what changed.
pub fn read(dir: &Path) -> Result<Vec<Entry>> {
    let path = dir.join("journal.jsonl");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
    // What follows the last newline: nothing, or a line torn by a crash.
    lines.pop();
    lines
        .into_iter()
        .map(|line| serde_json::from_slice(line).with_context(|| format!("{} is damaged", path.display())))
        .collect()
}

/// The steps that were started, newest first, with whether each was completed.
pub fn started_steps(entries: &[Entry]) -> Vec<(Entry, bool)> {
    let mut steps: Vec<(Entry, bool)> = entries
        .iter()
        .filter(|entry| entry.mark == Mark::Intent)
        .map(|intent| {
            let done = entries
                .iter()
                .any(|entry| entry.mark == Mark::Done && entry.step == intent.step);
            (intent.clone(), done)
        })
        .collect();
    steps.reverse();
    steps
}

/// The workers in the order they started; each registers itself first under its own step. A core
/// upgrade decided to be undone starts a second one.
pub fn workers(entries: &[Entry]) -> Vec<ProcessRef> {
    entries
        .iter()
        .filter(|entry| entry.mark == Mark::Writer && entry.step == u32::MAX)
        .filter_map(|entry| serde_json::from_value(entry.data.clone()).ok())
        .collect()
}

/// The worker started last: the one the transaction now waits on.
pub fn worker(entries: &[Entry]) -> Option<ProcessRef> {
    workers(entries).pop()
}

/// How many workers of the transaction in `dir` have registered so far.
#[cfg_attr(windows, allow(dead_code))]
pub fn worker_count(dir: &Path) -> usize {
    read(dir).map_or(0, |entries| workers(&entries).len())
}

/// Waits for a worker started after `before` workers had registered to register itself, and
/// returns it. Windows knows its worker from starting it.
#[cfg_attr(windows, allow(dead_code))]
pub fn await_worker(dir: &Path, before: usize, timeout: Duration) -> Result<ProcessRef> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(entries) = read(dir) {
            let workers = workers(&entries);
            if workers.len() > before
                && let Some(worker) = workers.last()
            {
                return Ok(*worker);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the worker did not start");
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn writers(entries: &[Entry]) -> Vec<ProcessRef> {
    entries
        .iter()
        .filter(|entry| entry.mark == Mark::Writer)
        .filter_map(|entry| serde_json::from_value(entry.data.clone()).ok())
        .collect()
}
