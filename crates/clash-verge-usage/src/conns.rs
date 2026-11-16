//! Connection stats: what the core counted per key (source, process, target, network, rule
//! and route) and UTC hour, collected while recording is on and kept for 32 days.
//!
//! Settled hours are appended once to the day's file and never rewritten; the hours the core
//! may still add to are merged in memory and saved to `current.json` from time to time. The
//! state is written last, so the collection progress never gets ahead of the data.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

use crate::{
    files::{self, Loaded},
    model::{UsageGap, UsageGapReason, day_name, day_of, day_start, parse_day_name},
};

pub const CONN_FORMAT: u32 = 1;
/// Settled hours requested from the core at a time.
pub const CONN_BATCH_HOURS: i64 = 24;
/// Days of hours kept on disk.
pub const CONN_RETENTION_DAYS: i64 = 32;
/// How often the hours the core may still add to are saved.
const CURRENT_SAVE_SECS: i64 = 300;

/// What the core counts connections under.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(default, rename_all = "camelCase")]
pub struct ConnKey {
    pub in_type: String,
    pub in_name: String,
    pub in_user: String,
    #[serde(rename = "srcIP")]
    pub src_ip: String,
    /// The path of the process, or its name when the path is unknown.
    pub process: String,
    /// The host, else the sniffed host, else the destination IP.
    pub target: String,
    #[serde(rename = "targetIsIP")]
    pub target_is_ip: bool,
    pub network: String,
    pub rule: String,
    pub rule_payload: String,
    /// The provider of the proxy that made the connections.
    pub provider: String,
    /// The proxy that made the connections, then the groups out to the one a rule chose.
    pub chain: Vec<String>,
}

impl ConnKey {
    /// Every field is hashed with its length, so no two keys share an id.
    pub fn id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"conn-key-1");
        for part in [
            &self.in_type,
            &self.in_name,
            &self.in_user,
            &self.src_ip,
            &self.process,
            &self.target,
            &self.network,
            &self.rule,
            &self.rule_payload,
            &self.provider,
        ] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        hasher.update([u8::from(self.target_is_ip)]);
        hasher.update((self.chain.len() as u64).to_le_bytes());
        for name in &self.chain {
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
        }
        hasher.finalize()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Whether the core counted connections in an hour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum Counting {
    /// Throughout the hour.
    On,
    /// For a part of it.
    Partial,
    #[default]
    Off,
}

impl Counting {
    /// What two reads of the same hour of the same core make together.
    pub fn merge(self, other: Self) -> Self {
        if self == other { self } else { Self::Partial }
    }
}

/// Upload, download and connections opened.
pub type ConnCounts = [u64; 3];

pub fn merge_counts(into: &mut ConnCounts, from: ConnCounts) {
    for (into, from) in into.iter_mut().zip(from) {
        *into = (*into).max(from);
    }
}

/// What the core reported for a range of hours.
#[derive(Debug, Clone, Default)]
pub struct CoreConns {
    pub instance: String,
    pub started_hour: i64,
    pub hour: i64,
    pub settled: i64,
    pub retained_since: i64,
    pub enabled: bool,
    pub tick: i64,
    pub counting_on: Vec<(i64, i64)>,
    pub counting_partial: Vec<(i64, i64)>,
    pub entries: Vec<CoreConnEntry>,
}

impl CoreConns {
    pub fn counting(&self, hour: i64) -> Counting {
        let within = |ranges: &[(i64, i64)]| ranges.iter().any(|(from, to)| (*from..=*to).contains(&hour));
        if within(&self.counting_on) {
            Counting::On
        } else if within(&self.counting_partial) {
            Counting::Partial
        } else {
            Counting::Off
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoreConnEntry {
    pub key: ConnKey,
    pub buckets: Vec<CoreConnBucket>,
}

#[derive(Debug, Clone, Copy)]
pub struct CoreConnBucket {
    pub hour: i64,
    pub up: u64,
    pub down: u64,
    pub conns: u64,
}

/// One hour of one core, as appended to the day's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HourRow {
    pub hour: i64,
    pub instance: String,
    #[serde(default)]
    pub counting: Counting,
    #[serde(default)]
    pub entries: BTreeMap<String, ConnCounts>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KeyLine {
    pub id: String,
    pub first_seen: i64,
    #[serde(flatten)]
    pub key: ConnKey,
}

/// An hour the core may still add to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingHour {
    #[serde(default)]
    pub counting: Counting,
    #[serde(default)]
    pub entries: BTreeMap<String, ConnCounts>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CurrentFile {
    instance: String,
    hours: BTreeMap<i64, PendingHour>,
}

/// A span of time, in Unix seconds, the app recorded connection stats in; open while it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RecordingSpan {
    pub from: i64,
    #[serde(default)]
    pub to: Option<i64>,
}

/// Collection progress of the core last seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnInstance {
    pub id: String,
    pub started_hour: i64,
    /// Every hour up to this one is appended to its day's file, or recorded as a gap.
    pub done: i64,
    /// The core's current hour when it was last seen.
    pub last_hour: i64,
    /// Unix seconds of the last successful collection.
    pub last_collected: i64,
    /// Recording was turned off, and what the core counted before is still to be read.
    #[serde(default)]
    pub finishing: bool,
    /// Nothing more is owed from the core: it was read in full after it stopped counting, or
    /// before the app stopped it.
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnState {
    pub format: u32,
    /// When the app recorded: the only record of whether it is recording now.
    #[serde(default)]
    pub recording: Vec<RecordingSpan>,
    pub instance: Option<ConnInstance>,
    #[serde(default)]
    pub gaps: Vec<UsageGap>,
    /// Hours before this one may have gaps and recording spans lost with the state file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_unknown_before: Option<i64>,
    /// The state file was lost, and recording with it, until the user turns it on again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setting_lost: bool,
    /// Since the state file was lost, the next core seen is read back as far as it keeps,
    /// whatever recording said, as what it kept is not known to be taken.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfill: bool,
}

/// The connection stats kept in one directory.
pub struct ConnStore {
    pub(crate) dir: PathBuf,
    pub(crate) day_offset: i64,
    pub(crate) state: ConnState,
    pub(crate) keys: HashMap<String, ConnKey>,
    /// The hours of the current instance the core may still add to.
    pub(crate) pending: BTreeMap<i64, PendingHour>,
    /// What the current instance had pending was lost with `current.json`.
    pending_lost: bool,
    current_saved_at: i64,
    /// The instance and tick of the last read that left the pending hours complete.
    baseline: Option<(String, i64)>,
    pub(crate) cache: Option<crate::conns_query::Mirror>,
    compacted_day: i64,
    /// The instance and final collection that last read every hour not appended; anything
    /// done to the store since takes it back.
    read_in_full: Option<(String, u64)>,
}

impl ConnStore {
    pub fn open(dir: &Path, day_offset: i64, now_hour: i64) -> Result<Self> {
        files::create_dir(dir)?;
        files::create_dir(&dir.join("hours"))?;
        let has_history = has_history(dir)?;
        let state = load_or_backup::<ConnState>(&dir.join("state.json"), &dir.join("state.json.bak"))?;
        let mut store = Self {
            dir: dir.to_owned(),
            day_offset,
            state: state.clone().unwrap_or_else(|| ConnState {
                format: CONN_FORMAT,
                ..ConnState::default()
            }),
            keys: HashMap::new(),
            pending: BTreeMap::new(),
            pending_lost: false,
            current_saved_at: i64::MIN,
            baseline: None,
            cache: None,
            compacted_day: i64::MIN,
            read_in_full: None,
        };
        if state.is_none() && has_history {
            store.state.coverage_unknown_before = Some(store.history_end()?.max(now_hour + 1));
            store.state.setting_lost = true;
            store.state.backfill = true;
            store.save_state()?;
        }
        store.load_keys()?;
        store.load_current()?;
        for day in store.days()? {
            store.read_day(day)?;
        }
        if store.state.recording.is_empty() && !store.state.setting_lost {
            // recording is on until the user turns it off
            store.set_recording(true, now_hour * 3600)?;
        }
        Ok(store)
    }

    pub const fn state(&self) -> &ConnState {
        &self.state
    }

    pub fn recording(&self) -> bool {
        self.state.recording.last().is_some_and(|span| span.to.is_none())
    }

    /// Whether there is anything to collect: recording is on, or a core is still to be read
    /// up to where recording was turned off.
    pub fn collecting(&self) -> bool {
        self.recording() || self.state.instance.as_ref().is_some_and(|current| current.finishing)
    }

    pub fn finishing(&self) -> bool {
        self.state.instance.as_ref().is_some_and(|current| current.finishing)
    }

    /// Turns recording on or off; accepted once the state is on disk. Turning it on again
    /// while a core is still to be read on takes that over; otherwise the hours recording was
    /// off are not read.
    pub fn set_recording(&mut self, on: bool, now_secs: i64) -> Result<()> {
        if on == self.recording() {
            return Ok(());
        }
        let now_hour = now_secs.div_euclid(3600);
        let mut state = self.state.clone();
        let mut goes_on = false;
        if on {
            state.recording.push(RecordingSpan {
                from: now_secs,
                to: None,
            });
            state.setting_lost = false;
            if let Some(current) = state.instance.as_mut() {
                if current.finishing {
                    current.finishing = false;
                } else {
                    // a core read in full when recording stopped goes on in this hour
                    current.done = now_hour - 1;
                    current.stopped = false;
                    goes_on = true;
                }
            }
        } else {
            if let Some(span) = state.recording.last_mut() {
                span.to = Some(now_secs);
            }
            if let Some(current) = state.instance.as_mut()
                && !current.stopped
            {
                current.finishing = true;
            }
        }
        // the setting is taken once in the state file; the backup is brought up to date by
        // the next save if it fails here
        files::save(&self.dir.join("state.json"), &state)?;
        self.state = state;
        self.read_in_full = None;
        if goes_on {
            self.pending.retain(|hour, _| *hour >= now_hour);
        }
        if on {
            self.baseline = None;
        }
        let _ = files::save(&self.dir.join("state.json.bak"), &self.state);
        Ok(())
    }

    /// Takes note of the core a collection is about to read from. A different core than the
    /// last one seen ends the last one: what it did after its last collection is a gap unless
    /// nothing more was owed from it.
    pub fn observe(&mut self, core: &CoreConns, now_secs: i64) -> Result<()> {
        self.read_in_full = None;
        let mut changed = false;
        match self.state.instance.as_mut() {
            Some(current) if current.id == core.instance => {
                if core.hour > current.last_hour {
                    current.last_hour = core.hour;
                    changed = true;
                }
            }
            _ => {
                // the ended core stays until what was taken from it is on disk
                if let Some(previous) = self.state.instance.clone() {
                    self.end_instance(previous, core.started_hour)?;
                }
                self.state.instance = None;
                if self.recording() {
                    let since = if self.state.backfill {
                        i64::MIN
                    } else {
                        self.state
                            .recording
                            .last()
                            .map_or(i64::MIN, |span| span.from.div_euclid(3600))
                    };
                    self.state.backfill = false;
                    let first = core.started_hour.max(since);
                    if first < core.retained_since {
                        self.add_gap(first, core.retained_since - 1, UsageGapReason::Expired, false);
                    }
                    self.state.instance = Some(ConnInstance {
                        id: core.instance.clone(),
                        started_hour: core.started_hour,
                        done: first.max(core.retained_since) - 1,
                        last_hour: core.hour,
                        last_collected: now_secs,
                        finishing: false,
                        stopped: false,
                    });
                }
                self.pending.clear();
                self.pending_lost = false;
                self.baseline = None;
                self.save_current(now_secs)?;
                changed = true;
            }
        }
        changed |= self.skip_expired(core.retained_since)?;
        if changed {
            self.save_state()?;
        }
        Ok(())
    }

    /// Appends what was taken from an ended core, and records what it did after its last
    /// collection as a gap unless nothing more was owed from it.
    fn end_instance(&mut self, previous: ConnInstance, next_started_hour: i64) -> Result<()> {
        let rows = self.pending_rows(&previous.id, previous.done);
        self.append_rows(&rows)?;
        if !previous.stopped {
            self.add_gap(
                previous.done + 1,
                previous.last_hour.max(next_started_hour),
                UsageGapReason::CoreLost,
                next_started_hour < previous.last_hour,
            );
        }
        Ok(())
    }

    /// Records the hours the core no longer keeps, before `retained_since`, as a gap and moves
    /// the progress past them; what was taken of them is appended first.
    fn skip_expired(&mut self, retained_since: i64) -> Result<bool> {
        let Some(current) = self.state.instance.clone() else {
            return Ok(false);
        };
        if current.done + 1 >= retained_since {
            return Ok(false);
        }
        let taken: Vec<HourRow> = self
            .pending_rows(&current.id, current.done)
            .into_iter()
            .filter(|row| row.hour < retained_since)
            .collect();
        self.append_rows(&taken)?;
        self.add_gap(current.done + 1, retained_since - 1, UsageGapReason::Expired, false);
        if let Some(current) = self.state.instance.as_mut() {
            current.done = retained_since - 1;
        }
        self.pending.retain(|hour, _| *hour >= retained_since);
        Ok(true)
    }

    /// The settled hours to read in full next, or `None` once they are all appended.
    pub fn settled_range(&self, settled: i64) -> Option<(i64, i64)> {
        let current = self.state.instance.as_ref()?;
        if current.stopped || current.done >= settled {
            return None;
        }
        Some((current.done + 1, (current.done + CONN_BATCH_HOURS).min(settled)))
    }

    /// Appends the settled hours read in full from `since` to `until`, each hour with or
    /// without entries, and moves the progress past them.
    pub fn apply_settled(&mut self, core: &CoreConns, since: i64, until: i64, now_secs: i64) -> Result<()> {
        self.check_instance(core)?;
        self.skip_expired(core.retained_since)?;
        let done = self.state.instance.as_ref().map_or(i64::MIN, |current| current.done);
        let (from, to) = (since.max(done + 1), until.min(core.settled));
        let mut rows: BTreeMap<i64, HourRow> = (from..=to)
            .map(|hour| {
                (
                    hour,
                    HourRow {
                        hour,
                        instance: core.instance.clone(),
                        counting: core.counting(hour),
                        entries: BTreeMap::new(),
                    },
                )
            })
            .collect();
        let mut new_keys = Vec::new();
        for entry in &core.entries {
            let id = entry.key.id();
            for bucket in &entry.buckets {
                if let Some(row) = rows.get_mut(&bucket.hour) {
                    self.see_key(&id, &entry.key, bucket.hour, &mut new_keys);
                    merge_counts(
                        row.entries.entry(id.clone()).or_insert([0; 3]),
                        [bucket.up, bucket.down, bucket.conns],
                    );
                }
            }
        }
        self.append_keys(&new_keys)?;
        let rows: Vec<HourRow> = rows.into_values().collect();
        self.append_rows(&rows)?;
        // a day found unreadable meanwhile moved the progress back
        let rolled_back = self.state.instance.as_ref().is_some_and(|current| current.done < done);
        if let Some(current) = self.state.instance.as_mut() {
            if !rolled_back && to >= from {
                current.done = current.done.max(to);
            }
            current.last_hour = current.last_hour.max(core.hour);
            current.last_collected = now_secs;
        }
        let done = self.state.instance.as_ref().map_or(i64::MIN, |current| current.done);
        self.pending.retain(|hour, _| *hour > done);
        self.save_state()
    }

    /// The hours the core may still add to, to read next, and the tick to read from when
    /// the last read left them complete.
    pub fn pending_request(&self, settled: i64, hour: i64) -> Option<(i64, i64, Option<i64>)> {
        let current = self.state.instance.as_ref()?;
        if current.stopped {
            return None;
        }
        let since = current.done.max(settled) + 1;
        if since > hour {
            return None;
        }
        let touched = self
            .baseline
            .as_ref()
            .filter(|(instance, _)| *instance == current.id)
            .map(|(_, tick)| tick - 1);
        Some((since, hour, touched))
    }

    /// Merges what the core reported for the hours it may still add to, from `since` to
    /// `until` as requested. A read sets the baseline later reads go on from, unless the
    /// core's clock moved past `until` before it answered: the new hour was not read.
    pub fn apply_pending(&mut self, core: &CoreConns, since: i64, until: i64, now_secs: i64) -> Result<()> {
        self.check_instance(core)?;
        let done = self.state.instance.as_ref().map_or(i64::MIN, |current| current.done);
        let from = since.max(done + 1);
        let mut new_keys = Vec::new();
        for hour in from..=until {
            // what the core says of an hour only takes in more of it
            self.pending.entry(hour).or_default().counting = core.counting(hour);
        }
        for entry in &core.entries {
            let id = entry.key.id();
            for bucket in entry
                .buckets
                .iter()
                .filter(|bucket| (from..=until).contains(&bucket.hour))
            {
                self.see_key(&id, &entry.key, bucket.hour, &mut new_keys);
                merge_counts(
                    self.pending
                        .entry(bucket.hour)
                        .or_default()
                        .entries
                        .entry(id.clone())
                        .or_insert([0; 3]),
                    [bucket.up, bucket.down, bucket.conns],
                );
            }
        }
        self.append_keys(&new_keys)?;
        self.baseline = (core.hour <= until).then(|| (core.instance.clone(), core.tick));
        self.pending_lost = false;
        if let Some(current) = self.state.instance.as_mut() {
            current.last_hour = current.last_hour.max(core.hour);
            current.last_collected = now_secs;
        }
        if now_secs.saturating_sub(self.current_saved_at) >= CURRENT_SAVE_SECS {
            self.save_current(now_secs)?;
            self.save_state()?;
        }
        Ok(())
    }

    /// Reads after a failed one start again in full.
    pub fn forget_baseline(&mut self) {
        self.baseline = None;
    }

    /// Notes that final collection `attempt` read every hour of `instance` not appended.
    pub fn read_in_full(&mut self, instance: &str, attempt: u64) {
        self.read_in_full = Some((instance.to_owned(), attempt));
    }

    /// Ends the reading of a core recording was turned off for, once it no longer counts and
    /// was read in full: what it had pending is final.
    pub fn finish(&mut self, instance: &str, now_secs: i64) -> Result<()> {
        match self.state.instance.as_ref() {
            Some(current) if current.id == instance && current.finishing && !self.recording() => {}
            _ => return Ok(()),
        }
        self.settle_pending()?;
        if let Some(current) = self.state.instance.as_mut() {
            current.finishing = false;
            current.stopped = true;
        }
        self.save_current(now_secs)?;
        self.save_state()
    }

    /// Records that the app stopped the core after final collection `attempt`: if that read it
    /// in full with nothing done to the store since, what it had pending is final and its end
    /// is no gap.
    pub fn mark_stopped(&mut self, instance: &str, attempt: u64, now_secs: i64) -> Result<()> {
        if self.read_in_full.as_ref() != Some(&(instance.to_owned(), attempt)) || self.pending_lost {
            return Ok(());
        }
        match self.state.instance.as_ref() {
            Some(current) if current.id == instance => {}
            _ => return Ok(()),
        }
        self.settle_pending()?;
        if let Some(current) = self.state.instance.as_mut() {
            current.stopped = true;
            current.finishing = false;
        }
        self.save_current(now_secs)?;
        self.save_state()?;
        // kept until then, so that a failed write can be retried
        self.read_in_full = None;
        Ok(())
    }

    /// Appends the pending hours as final and moves the progress to the core's last hour.
    fn settle_pending(&mut self) -> Result<()> {
        let Some(current) = self.state.instance.clone() else {
            return Ok(());
        };
        let rows = self.pending_rows(&current.id, current.done);
        self.append_rows(&rows)?;
        self.pending.clear();
        if let Some(current) = self.state.instance.as_mut() {
            current.done = current.done.max(current.last_hour);
        }
        Ok(())
    }

    fn pending_rows(&self, instance: &str, done: i64) -> Vec<HourRow> {
        self.pending
            .iter()
            .filter(|(hour, _)| **hour > done)
            .map(|(hour, pending)| HourRow {
                hour: *hour,
                instance: instance.to_owned(),
                counting: pending.counting,
                entries: pending.entries.clone(),
            })
            .collect()
    }

    /// Saves the pending hours, as an app that ends before the next save would lose them.
    pub fn flush(&mut self, now_secs: i64) -> Result<()> {
        self.save_current(now_secs)?;
        self.save_state()
    }

    fn check_instance(&self, core: &CoreConns) -> Result<()> {
        if self
            .state
            .instance
            .as_ref()
            .is_none_or(|current| current.id != core.instance)
        {
            bail!(
                "connection stats came from core {} which is not the one observed",
                core.instance
            );
        }
        Ok(())
    }

    fn see_key(&mut self, id: &str, key: &ConnKey, hour: i64, new_keys: &mut Vec<KeyLine>) {
        if !self.keys.contains_key(id) {
            self.keys.insert(id.to_owned(), key.clone());
            new_keys.push(KeyLine {
                id: id.to_owned(),
                first_seen: hour,
                key: key.clone(),
            });
        }
    }

    fn append_keys(&mut self, keys: &[KeyLine]) -> Result<()> {
        let lines = keys
            .iter()
            .map(serde_json::to_vec)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Err(error) = files::append_lines(&self.dir.join("keys.jsonl"), &lines) {
            // they are added again once read again
            for key in keys {
                self.keys.remove(&key.id);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn day_path(&self, day: i64) -> PathBuf {
        self.dir.join("hours").join(format!("{}.jsonl", day_name(day)))
    }

    /// Appends rows to their days' files, each file once, and to the cached hours.
    fn append_rows(&mut self, rows: &[HourRow]) -> Result<()> {
        let mut days: BTreeMap<i64, Vec<Vec<u8>>> = BTreeMap::new();
        for row in rows {
            days.entry(day_of(row.hour, self.day_offset))
                .or_default()
                .push(serde_json::to_vec(row)?);
        }
        for (day, lines) in days {
            if let Err(error) = files::append_lines(&self.day_path(day), &lines) {
                // the days appended before are on disk: the cached hours no longer tell
                self.cache = None;
                return Err(error);
            }
        }
        if let Some(cache) = self.cache.as_mut() {
            for row in rows {
                cache.add(row);
            }
        }
        Ok(())
    }

    /// Reads a day's rows. A torn last line, which a crash during an append leaves, is cut
    /// off. Other unreadable lines make the day a gap: the file is set aside, the readable rows
    /// written back, and, if the current core still keeps that day, the progress moved back to
    /// read it again.
    pub(crate) fn read_day(&mut self, day: i64) -> Result<Vec<HourRow>> {
        let path = self.day_path(day);
        let Some(lines) = files::read_lines::<HourRow>(&path)? else {
            return Ok(Vec::new());
        };
        if lines.damaged {
            let start = day_start(day, self.day_offset);
            self.add_gap(start, start + 23, UsageGapReason::Corrupt, false);
            if let Some(current) = self.state.instance.as_mut()
                && current.done >= start
            {
                current.done = (start - 1).max(current.started_hour - 1);
                current.stopped = false;
                self.baseline = None;
            }
            self.save_state()?;
            // the file stays whole until the readable rows replace it
            files::preserve(&path)?;
            files::rewrite_lines(&path, &lines.values)?;
            self.cache = None;
        } else if lines.torn_tail {
            files::rewrite_lines(&path, &lines.values)?;
        }
        Ok(lines.values)
    }

    pub(crate) fn days(&self) -> Result<Vec<i64>> {
        let mut days = Vec::new();
        for entry in std::fs::read_dir(self.dir.join("hours"))? {
            let name = entry?.file_name();
            if let Some(day) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(parse_day_name)
            {
                days.push(day);
            }
        }
        days.sort_unstable();
        Ok(days)
    }

    fn history_end(&self) -> Result<i64> {
        Ok(self
            .days()?
            .last()
            .map_or(i64::MIN, |day| day_start(day + 1, self.day_offset)))
    }

    fn load_keys(&mut self) -> Result<()> {
        let path = self.dir.join("keys.jsonl");
        let Some(lines) = files::read_lines::<KeyLine>(&path)? else {
            return Ok(());
        };
        if lines.torn_tail || lines.damaged {
            // a key lost here is added again by the next read of the core that reports it
            files::rewrite_lines(&path, &lines.values)?;
        }
        self.keys = lines.values.into_iter().map(|line| (line.id, line.key)).collect();
        Ok(())
    }

    fn load_current(&mut self) -> Result<()> {
        let path = self.dir.join("current.json");
        let current = match files::load::<CurrentFile>(&path)? {
            Loaded::Ok(current) => Some(current),
            Loaded::Corrupt => {
                files::quarantine(&path)?;
                None
            }
            Loaded::Missing => None,
        };
        let Some(instance) = self.state.instance.as_ref() else {
            return Ok(());
        };
        match current {
            Some(current) if current.instance == instance.id => {
                let done = instance.done;
                self.pending = current.hours.into_iter().filter(|(hour, _)| *hour > done).collect();
            }
            _ => {
                // what the core may still add to is read again while it runs
                self.pending_lost = !instance.stopped && instance.done < instance.last_hour;
            }
        }
        Ok(())
    }

    fn save_current(&mut self, now_secs: i64) -> Result<()> {
        let instance = self
            .state
            .instance
            .as_ref()
            .map(|current| current.id.clone())
            .unwrap_or_default();
        files::save(
            &self.dir.join("current.json"),
            &CurrentFile {
                instance,
                hours: self.pending.clone(),
            },
        )?;
        self.current_saved_at = now_secs;
        Ok(())
    }

    /// Deletes the days that ended before the retention, then, once a day, drops the keys
    /// nothing kept refers to.
    pub fn retain(&mut self, now_hour: i64) -> Result<()> {
        let cutoff = now_hour - CONN_RETENTION_DAYS * 24;
        let mut removed = false;
        // the files set aside for a day go with it
        for entry in std::fs::read_dir(self.dir.join("hours"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(day) = name.to_str().and_then(|name| name.get(..10)).and_then(parse_day_name) else {
                continue;
            };
            if day_start(day + 1, self.day_offset) <= cutoff {
                files::remove(&entry.path())?;
                removed = true;
            }
        }
        if let Some(cache) = self.cache.as_mut() {
            cache.drop_before(cutoff);
        }
        let today = day_of(now_hour, self.day_offset);
        if self.compacted_day == today {
            return Ok(());
        }
        let mut referenced: HashSet<String> = HashSet::new();
        for day in self.days()? {
            for row in self.read_day(day)? {
                referenced.extend(row.entries.into_keys());
            }
        }
        for pending in self.pending.values() {
            referenced.extend(pending.entries.keys().cloned());
        }
        if removed || self.keys.keys().any(|id| !referenced.contains(id)) {
            self.keys.retain(|id, _| referenced.contains(id));
            let mut lines: Vec<KeyLine> = self
                .keys
                .iter()
                .map(|(id, key)| KeyLine {
                    id: id.clone(),
                    first_seen: 0,
                    key: key.clone(),
                })
                .collect();
            lines.sort_by(|a, b| a.id.cmp(&b.id));
            files::rewrite_lines(&self.dir.join("keys.jsonl"), &lines)?;
        }
        self.compacted_day = today;
        Ok(())
    }

    /// Deletes every connection stat kept and starts again: recording, if on, from now, and the
    /// running core from its current hour.
    pub fn clear(&mut self, now_secs: i64) -> Result<()> {
        let recording = self.recording();
        let state = ConnState {
            format: CONN_FORMAT,
            recording: if recording {
                vec![RecordingSpan {
                    from: now_secs,
                    to: None,
                }]
            } else {
                Vec::new()
            },
            ..ConnState::default()
        };
        // the setting stays on disk throughout: the state is replaced before the rest goes
        save_state(&self.dir, &state)?;
        self.state = state;
        self.keys.clear();
        self.pending.clear();
        self.pending_lost = false;
        self.baseline = None;
        self.cache = None;
        self.read_in_full = None;
        // every other file, those set aside and those a failed write left included
        for dir in [self.dir.join("hours"), self.dir.clone()] {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name();
                if entry.file_type()?.is_file() && name != "state.json" && name != "state.json.bak" {
                    files::remove(&entry.path())?;
                }
            }
        }
        Ok(())
    }

    fn add_gap(&mut self, from: i64, to: i64, reason: UsageGapReason, time_anomaly: bool) {
        if from > to {
            return;
        }
        let gap = UsageGap {
            from,
            to,
            reason,
            time_anomaly,
        };
        if !self.state.gaps.contains(&gap) {
            self.state.gaps.push(gap);
        }
    }

    fn save_state(&self) -> Result<()> {
        save_state(&self.dir, &self.state)
    }
}

fn save_state(dir: &Path, state: &ConnState) -> Result<()> {
    files::save(&dir.join("state.json"), state)?;
    files::save(&dir.join("state.json.bak"), state)
}

/// Whether recording is on, as the state on disk says, without opening the store: on for a
/// store that never recorded, as opening it turns recording on.
pub fn recording_on_disk(dir: &Path) -> bool {
    [dir.join("state.json"), dir.join("state.json.bak")]
        .iter()
        .find_map(|path| match files::load::<ConnState>(path) {
            Ok(Loaded::Ok(state)) => Some(
                state
                    .recording
                    .last()
                    .map_or(!state.setting_lost, |span| span.to.is_none()),
            ),
            _ => None,
        })
        .unwrap_or_else(|| matches!(has_history(dir), Ok(false)))
}

fn has_history(dir: &Path) -> Result<bool> {
    if ["state.json", "state.json.bak", "keys.jsonl", "current.json"]
        .iter()
        .any(|name| dir.join(name).exists())
    {
        return Ok(true);
    }
    match std::fs::read_dir(dir.join("hours")) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Loads a file, falling back to its backup when it is missing or unreadable.
fn load_or_backup<T: DeserializeOwned + Serialize>(path: &Path, backup: &Path) -> Result<Option<T>> {
    match files::load(path)? {
        Loaded::Ok(value) => return Ok(Some(value)),
        Loaded::Corrupt => {
            files::quarantine(path)?;
        }
        Loaded::Missing => {}
    }
    match files::load::<T>(backup)? {
        Loaded::Ok(value) => {
            files::save(path, &value)?;
            Ok(Some(value))
        }
        Loaded::Missing | Loaded::Corrupt => Ok(None),
    }
}
