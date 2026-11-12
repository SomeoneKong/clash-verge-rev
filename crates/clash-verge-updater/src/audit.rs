//! The audit log: JSON lines, flushed to disk per record. Rejected requests and client claims go
//! to their own rate-limited files so they cannot push update records out of the rotation.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Done or seen by the component itself.
    Actual,
    /// Written by an installer that ran without the component.
    Installer,
    /// Stated by a client; not verified.
    Claimed,
    /// A version change the component noticed afterwards; how and when it happened is unknown.
    Observed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub kind: String,
    pub user: String,
    pub id: String,
    pub pid: u32,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadInfo {
    pub kind: crate::manifest::Kind,
    pub sha256: String,
    pub size: u64,
    pub key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreChange {
    pub name: String,
    pub from: Option<String>,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Components {
    pub gui: bool,
    pub cores: Vec<CoreChange>,
    pub service_changed: bool,
    pub updater_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub time: String,
    #[serde(default)]
    pub seq: u64,
    pub source: Source,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<PayloadInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_installed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Components>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl Record {
    pub fn new(source: Source, event: impl Into<String>) -> Self {
        Self {
            time: now(),
            seq: 0,
            source,
            event: event.into(),
            txn: None,
            trigger: None,
            from_version: None,
            to_version: None,
            payload: None,
            fingerprint_installed: None,
            fingerprint_target: None,
            components: None,
            result: None,
            error: None,
            duration_ms: None,
            detail: None,
        }
    }

    pub fn txn(mut self, id: &str) -> Self {
        self.txn = Some(id.to_owned());
        self
    }

    pub fn versions(mut self, from: Option<&str>, to: Option<&str>) -> Self {
        self.from_version = from.map(str::to_owned);
        self.to_version = to.map(str::to_owned);
        self
    }

    pub fn result(mut self, result: impl Into<String>) -> Self {
        self.result = Some(result.into());
        self
    }

    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Audit,
    Rejected,
    Claims,
}

impl Stream {
    const fn file(self) -> &'static str {
        match self {
            Self::Audit => "audit.jsonl",
            Self::Rejected => "rejected.jsonl",
            Self::Claims => "claims.jsonl",
        }
    }

    const fn max_size(self) -> u64 {
        match self {
            Self::Audit => 5 * 1024 * 1024,
            Self::Rejected | Self::Claims => 1024 * 1024,
        }
    }

    const fn keep(self) -> u32 {
        match self {
            Self::Audit => 20,
            Self::Rejected | Self::Claims => 5,
        }
    }

    /// Records per identity per window; `None` for unlimited.
    const fn limit(self) -> Option<(u32, Duration)> {
        match self {
            Self::Audit => None,
            Self::Rejected => Some((10, Duration::from_secs(60))),
            Self::Claims => Some((20, Duration::from_secs(3600))),
        }
    }
}

/// Per stream and identity: when the window began, records in it, records dropped since.
type Windows = HashMap<(Stream, String), (Instant, u32, u64)>;

const STREAMS: [Stream; 3] = [Stream::Audit, Stream::Rejected, Stream::Claims];

/// The service, installers and administrative commands each write the log from their own process;
/// this file lock orders them, and sequence numbers are taken from the files under it.
struct DirLock {
    _file: std::fs::File,
}

impl DirLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(".lock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(file) = try_lock_file(&path)? {
                return Ok(Self { _file: file });
            }
            anyhow::ensure!(Instant::now() < deadline, "the audit log stayed locked");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(windows)]
fn try_lock_file(path: &Path) -> Result<Option<std::fs::File>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to open {}", path.display())),
    }
}

#[cfg(unix)]
fn try_lock_file(path: &Path) -> Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(error.into())
}

pub struct AuditLog {
    dir: PathBuf,
    windows: Mutex<Windows>,
}

impl AuditLog {
    /// Opens the log directory, dropping a torn last line left by a crash.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let log = Self {
            dir: dir.to_owned(),
            windows: Mutex::new(HashMap::new()),
        };
        Ok(log)
    }

    /// Appends `record`, flushed to disk. Errors are returned: an update must not run unrecorded.
    /// Sequence numbers run across all streams, so a merged page is in order.
    pub fn write(&self, stream: Stream, record: Record) -> Result<u64> {
        let _lock = DirLock::acquire(&self.dir)?;
        // A writer in another process may have died halfway through a line.
        if repair_tail(&self.dir.join(stream.file()))? {
            let note =
                Record::new(Source::Actual, "log_truncated").detail(serde_json::json!({ "file": stream.file() }));
            self.append_locked(stream, note)?;
        }
        self.append_locked(stream, record)
    }

    fn append_locked(&self, stream: Stream, mut record: Record) -> Result<u64> {
        let seq = STREAMS
            .into_iter()
            .map(|stream| last_seq_of_stream(&self.dir, stream))
            .max()
            .unwrap_or(0)
            + 1;
        record.seq = seq;
        let path = self.dir.join(stream.file());
        append_line(&path, &record)?;
        rotate(&path, stream.max_size(), stream.keep())?;
        Ok(seq)
    }

    /// Rate-limited write for records any client can cause; dropped ones are counted and noted
    /// with the next record that passes.
    pub fn write_limited(&self, stream: Stream, identity: &str, mut record: Record) -> Result<()> {
        if let Some((limit, window)) = stream.limit() {
            let mut windows = self
                .windows
                .lock()
                .map_err(|_| anyhow::anyhow!("audit lock poisoned"))?;
            let entry = windows
                .entry((stream, identity.to_owned()))
                .or_insert_with(|| (Instant::now(), 0, 0));
            if entry.0.elapsed() > window {
                *entry = (Instant::now(), 0, entry.2);
            }
            let verdict = if entry.1 >= limit {
                entry.2 += 1;
                None
            } else {
                entry.1 += 1;
                Some(std::mem::take(&mut entry.2))
            };
            drop(windows);
            let Some(dropped) = verdict else {
                return Ok(());
            };
            if dropped > 0 {
                record.detail = Some(serde_json::json!({ "dropped_before": dropped, "detail": record.detail }));
            }
        }
        self.write(stream, record).map(drop)
    }
}

fn append_line(path: &Path, record: &Record) -> Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o644);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    // Readable by everyone and writable only by root, whatever the umask of the writing process.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}

/// Cuts a last line that does not end in a newline; true if something was cut.
fn repair_tail(path: &Path) -> Result<bool> {
    let mut file = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("failed to open {}", path.display())),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    let mut last = [0u8];
    file.seek(std::io::SeekFrom::End(-1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(false);
    }
    let bytes = std::fs::read(path)?;
    let keep = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    file.set_len(keep as u64)?;
    file.sync_all()?;
    Ok(true)
}

/// The sequence number of the last record in a file, read from its tail.
fn last_seq(path: &Path) -> Option<u64> {
    const TAIL: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    tail.split(|&byte| byte == b'\n')
        .rev()
        .find_map(|line| serde_json::from_slice::<Record>(line).ok())
        .map(|record| record.seq)
}

/// A stream rotated a moment ago has an empty file; its last number is then in the rotated one.
fn last_seq_of_stream(dir: &Path, stream: Stream) -> u64 {
    let path = dir.join(stream.file());
    last_seq(&path)
        .or_else(|| {
            let mut name = path.as_os_str().to_owned();
            name.push(".1");
            last_seq(Path::new(&name))
        })
        .unwrap_or(0)
}

fn rotate(path: &Path, max_size: u64, keep: u32) -> Result<()> {
    if std::fs::metadata(path).map_or(0, |meta| meta.len()) <= max_size {
        return Ok(());
    }
    let numbered = |index: u32| {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    };
    let _ = std::fs::remove_file(numbered(keep));
    for index in (1..keep).rev() {
        let from = numbered(index);
        if from.exists() {
            std::fs::rename(&from, numbered(index + 1))?;
        }
    }
    std::fs::rename(path, numbered(1))?;
    Ok(())
}

/// Reads records newest first, merging the update log and client claims across rotations.
pub fn read_page(dir: &Path, before_seq: Option<u64>, limit: usize) -> Vec<Record> {
    let mut records = Vec::new();
    for stream in [Stream::Audit, Stream::Claims] {
        let base = dir.join(stream.file());
        let mut paths = vec![base.clone()];
        paths.extend((1..=stream.keep()).map(|index| {
            let mut name = base.as_os_str().to_owned();
            name.push(format!(".{index}"));
            PathBuf::from(name)
        }));
        for path in paths {
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            records.extend(
                std::io::BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|line| serde_json::from_str::<Record>(&line).ok())
                    .filter(|record| before_seq.is_none_or(|before| record.seq < before)),
            );
        }
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.seq));
    records.truncate(limit);
    records
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cvu-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn claims_cannot_push_out_update_records() {
        let dir = temp_dir("flood");
        let log = AuditLog::open(&dir).unwrap();
        log.write(Stream::Audit, Record::new(Source::Actual, "applied"))
            .unwrap();
        for _ in 0..1000 {
            log.write_limited(Stream::Claims, "u", Record::new(Source::Claimed, "fallback"))
                .unwrap();
        }
        let records = read_page(&dir, None, 1000);
        assert_eq!(records.iter().filter(|r| r.event == "applied").count(), 1);
        assert_eq!(records.iter().filter(|r| r.event == "fallback").count(), 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_line_is_cut_and_noted() {
        let dir = temp_dir("torn");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        std::fs::write(
            &path,
            "{\"time\":\"t\",\"seq\":1,\"source\":\"actual\",\"event\":\"a\"}\n{\"time\":",
        )
        .unwrap();
        let log = AuditLog::open(&dir).unwrap();
        log.write(Stream::Audit, Record::new(Source::Actual, "b")).unwrap();
        let events: Vec<String> = read_page(&dir, None, 10).into_iter().map(|r| r.event).collect();
        assert_eq!(events, ["b", "log_truncated", "a"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
