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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub kind: String,
    pub user: String,
    pub id: String,
    pub pid: u32,
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadInfo {
    pub kind: crate::manifest::Kind,
    pub sha256: String,
    pub size: u64,
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreChange {
    pub name: String,
    pub from: Option<String>,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Components {
    pub gui: bool,
    pub cores: Vec<CoreChange>,
    pub service_changed: bool,
    pub updater_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    fn abbreviated(self) -> Self {
        fn prefix(mut value: String) -> String {
            if value.len() > 512 {
                value.truncate(value.floor_char_boundary(512));
                value.push('…');
            }
            value
        }
        let mut error = self.error.map(prefix).unwrap_or_default();
        if !error.is_empty() {
            error.push(' ');
        }
        error.push_str("[truncated for display]");
        Self {
            seq: self.seq,
            time: prefix(self.time),
            txn: self.txn,
            from_version: self.from_version.map(prefix),
            to_version: self.to_version.map(prefix),
            result: self.result.map(prefix),
            error: Some(error),
            ..Self::new(self.source, self.event)
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
        let seq = latest_seq(&self.dir)?
            .checked_add(1)
            .context("audit sequence exhausted")?;
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

/// Reads complete JSON records backwards in blocks without loading a whole rotation.
struct ReverseFile {
    skip_tail: bool,
    file: std::fs::File,
    offset: u64,
    buffer: Vec<u8>,
}

impl ReverseFile {
    fn open(path: &Path) -> Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
        };
        let offset = file.metadata()?.len();
        Ok(Some(Self {
            skip_tail: true,
            file,
            offset,
            buffer: Vec::new(),
        }))
    }

    fn next_record<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            if let Some(index) = self.buffer.iter().rposition(|b| *b == b'\n') {
                let record = if std::mem::take(&mut self.skip_tail) {
                    None
                } else {
                    Some(serde_json::from_slice(&self.buffer[index + 1..])?)
                };
                self.buffer.truncate(index);
                if record.is_some() {
                    return Ok(record);
                }
            } else if self.offset == 0 {
                if self.skip_tail || self.buffer.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(serde_json::from_slice(&std::mem::take(&mut self.buffer))?));
            } else {
                let size = self.offset.min(8192);
                self.offset -= size;
                self.file.seek(std::io::SeekFrom::Start(self.offset))?;
                let mut block = vec![0; size as usize];
                self.file.read_exact(&mut block)?;
                block.append(&mut self.buffer);
                self.buffer = block;
            }
        }
    }
}

#[derive(Deserialize)]
struct Sequence {
    seq: u64,
}

fn latest_seq(dir: &Path) -> Result<u64> {
    let mut latest = 0;
    for stream in STREAMS {
        for name in [stream.file().to_owned(), format!("{}.1", stream.file())] {
            if let Some(mut file) = ReverseFile::open(&dir.join(name))?
                && let Some(record) = file.next_record::<Sequence>()?
            {
                latest = latest.max(record.seq);
                break;
            }
        }
    }
    Ok(latest)
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

fn older(dir: &Path, stream: Stream, upper: u64) -> impl Iterator<Item = Result<Record>> + '_ {
    let mut bound = Some(upper);
    let mut volumes = 0..=stream.keep();
    let mut current: Option<ReverseFile> = None;
    let mut next = move || -> Result<Option<Record>> {
        while let Some(max) = bound {
            if let Some(file) = current.as_mut() {
                while let Some(record) = file.next_record::<Record>()? {
                    if record.seq <= max {
                        bound = record.seq.checked_sub(1);
                        return Ok(Some(record));
                    }
                }
            }
            let Some(index) = volumes.next() else { break };
            let name = stream.file();
            let path = if index == 0 {
                dir.join(name)
            } else {
                dir.join(format!("{name}.{index}"))
            };
            current = None;
            if let Some(mut file) = ReverseFile::open(&path)? {
                let mut first = Vec::new();
                std::io::BufReader::new(&mut file.file).read_until(b'\n', &mut first)?;
                if first.ends_with(b"\n") && serde_json::from_slice::<Sequence>(&first)?.seq <= max {
                    current = Some(file);
                }
            }
        }
        Ok(None)
    };
    std::iter::from_fn(move || next().transpose())
}

pub fn merged(dir: &Path, upper: u64) -> impl Iterator<Item = Result<Record>> + '_ {
    let mut audit = older(dir, Stream::Audit, upper).peekable();
    let mut claims = older(dir, Stream::Claims, upper).peekable();
    std::iter::from_fn(move || match (audit.peek(), claims.peek()) {
        (Some(Err(_)), _) => audit.next(),
        (_, Some(Err(_))) => claims.next(),
        (Some(Ok(a)), Some(Ok(c))) if a.seq < c.seq => claims.next(),
        (Some(_), _) => audit.next(),
        _ => claims.next(),
    })
}

fn upper_bound(dir: &Path, before_seq: Option<u64>) -> Result<Option<u64>> {
    match before_seq {
        Some(before) => Ok(before.checked_sub(1)),
        None => Ok(Some(latest_seq(dir)?)),
    }
}

pub fn read_page(dir: &Path, before_seq: Option<u64>, limit: usize) -> Result<Vec<Record>> {
    upper_bound(dir, before_seq)?
        .into_iter()
        .flat_map(|upper| merged(dir, upper))
        .take(limit)
        .collect()
}

pub fn response_page(dir: &Path, before_seq: Option<u64>, limit: u32) -> Result<crate::protocol::Response> {
    use crate::protocol::{MAX_FRAME, Response};
    let mut records = Vec::new();
    let mut budget = MAX_FRAME as usize - serde_json::to_vec(&Response::Audit { records: vec![] })?.len();
    let upper = if before_seq.is_none() {
        let _lock = DirLock::acquire(dir)?;
        upper_bound(dir, None)?
    } else {
        upper_bound(dir, before_seq)?
    };
    if let Some(upper) = upper {
        for record in merged(dir, upper).take(limit.clamp(1, 200) as usize) {
            let mut record = record?;
            let mut size = serde_json::to_vec(&record)?.len() + usize::from(!records.is_empty());
            if records.is_empty() && size > budget {
                record = record.abbreviated();
                size = serde_json::to_vec(&record)?.len();
            }
            if size > budget {
                anyhow::ensure!(!records.is_empty(), "an audit record exceeds the IPC frame limit");
                break;
            }
            budget -= size;
            records.push(record);
        }
    }
    Ok(Response::Audit { records })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "tests assert by panicking")]
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
        let records = read_page(&dir, None, 1000).unwrap();
        assert_eq!(records.iter().filter(|r| r.event == "applied").count(), 1);
        assert_eq!(records.iter().filter(|r| r.event == "fallback").count(), 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_merges_streams_through_rotation_and_lost_cursors() {
        let dir = temp_dir("pages");
        let log = AuditLog::open(&dir).unwrap();
        let write = |stream| log.write(stream, Record::new(Source::Actual, "event")).unwrap();
        for stream in [Stream::Audit, Stream::Claims, Stream::Audit, Stream::Claims] {
            write(stream);
        }
        let upper = latest_seq(&dir).unwrap();
        write(Stream::Audit);
        let mut page = merged(&dir, upper);
        assert_eq!(page.next().unwrap().unwrap().seq, 4);
        assert_eq!(page.next().unwrap().unwrap().seq, 3);
        for stream in [Stream::Audit, Stream::Claims] {
            rotate(&dir.join(stream.file()), 0, stream.keep()).unwrap();
            write(stream);
        }
        assert_eq!(page.map(|r| r.unwrap().seq).collect::<Vec<_>>(), [2, 1]);
        std::fs::remove_file(dir.join("claims.jsonl.1")).unwrap();
        assert_eq!(
            read_page(&dir, Some(4), 2)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            [3, 1]
        );
        assert!(read_page(&dir, Some(1), 50).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn frame_budget_shortens_pages_without_losing_the_next_record() {
        use crate::protocol::{MAX_FRAME, Response};
        let dir = temp_dir("budget");
        let log = AuditLog::open(&dir).unwrap();
        for _ in 0..5 {
            log.write(
                Stream::Audit,
                Record::new(Source::Actual, "failed").error("x".repeat(18000)),
            )
            .unwrap();
        }
        let mut seq = Vec::new();
        loop {
            let page = response_page(&dir, seq.last().copied(), 50).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME as usize);
            let Response::Audit { records } = page else {
                panic!("expected audit page")
            };
            if records.is_empty() {
                break;
            }
            assert!(records.len() < 5);
            seq.extend(records.iter().map(|r| r.seq));
        }
        assert_eq!(seq, [5, 4, 3, 2, 1]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_record_does_not_hide_earlier_history_or_change_disk() {
        use crate::protocol::{MAX_FRAME, Response};
        let dir = temp_dir("oversized");
        let log = AuditLog::open(&dir).unwrap();
        for size in [1, 70000, 1] {
            log.write(
                Stream::Audit,
                Record::new(Source::Actual, "applied")
                    .txn("tx")
                    .result("x".repeat(size)),
            )
            .unwrap();
        }
        let mut seqs = Vec::new();
        while let page @ Response::Audit { .. } = response_page(&dir, seqs.last().copied(), 50).unwrap() {
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME as usize);
            let Response::Audit { records } = page else {
                unreachable!()
            };
            if records.is_empty() {
                break;
            }
            for r in records {
                if r.seq == 2 {
                    assert_eq!(r.event, "applied");
                    assert_eq!(r.txn.as_deref(), Some("tx"));
                    assert!(r.result.unwrap().ends_with('…'));
                    assert!(r.error.unwrap().contains("truncated"));
                }
                seqs.push(r.seq);
            }
        }
        assert_eq!(seqs, [3, 2, 1]);
        assert_eq!(
            read_page(&dir, Some(3), 1).unwrap()[0].result.as_ref().unwrap().len(),
            70000
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_volume_fails_the_page_until_the_same_cursor_can_be_retried() {
        let dir = temp_dir("read-error");
        let log = AuditLog::open(&dir).unwrap();
        log.write(Stream::Claims, Record::new(Source::Claimed, "fallback"))
            .unwrap();
        let path = dir.join("audit.jsonl.1");
        std::fs::create_dir(&path).unwrap();
        assert!(response_page(&dir, Some(4), 50).is_err());
        std::fs::remove_dir(&path).unwrap();
        for _ in 0..2 {
            log.write(Stream::Audit, Record::new(Source::Actual, "applied"))
                .unwrap();
        }
        rotate(&dir.join("audit.jsonl"), 0, 1).unwrap();
        assert_eq!(
            read_page(&dir, Some(4), 50)
                .unwrap()
                .into_iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn allocation_only_needs_the_sequence_of_a_complete_claim() {
        let dir = temp_dir("claim-schema");
        let log = AuditLog::open(&dir).unwrap();
        let claims = dir.join(Stream::Claims.file());
        std::fs::write(&claims, b"{\"seq\":8,\"trigger\":{}}\n").unwrap();
        assert_eq!(
            log.write(Stream::Audit, Record::new(Source::Actual, "staged")).unwrap(),
            9
        );
        assert!(read_page(&dir, None, 10).is_err());
        std::fs::write(claims, b"{\"seq\":\"invalid\"}\n").unwrap();
        assert!(
            log.write(Stream::Audit, Record::new(Source::Actual, "applied"))
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn short_log_does_not_open_an_older_volume() {
        let dir = temp_dir("short-log");
        let log = AuditLog::open(&dir).unwrap();
        log.write(Stream::Audit, Record::new(Source::Actual, "applied"))
            .unwrap();
        std::fs::create_dir(dir.join("audit.jsonl.1")).unwrap();
        let mut rows = merged(&dir, u64::MAX);
        assert_eq!(rows.next().unwrap().unwrap().seq, 1);
        assert!(rows.next().unwrap().is_err());
        drop(rows);
        std::fs::remove_dir_all(dir).unwrap();
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
        let events: Vec<String> = read_page(&dir, None, 10)
            .unwrap()
            .into_iter()
            .map(|r| r.event)
            .collect();
        assert_eq!(events, ["b", "log_truncated", "a"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
