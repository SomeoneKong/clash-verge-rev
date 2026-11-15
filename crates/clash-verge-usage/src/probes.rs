//! Decision records of the core's probe strategy, kept beside the traffic usage in `probes/`.
//!
//! Events are appended to a JSON Lines file per statistics day, each line an event with the
//! `instance` and `schema` it came with. `meta.json` indexes what every day file holds up to
//! the byte it committed, so that lines appended without their index entry, which an app that
//! ended between the two leaves behind, are taken into the index again. The contexts events
//! refer to are kept per instance in `contexts/`.
//!
//! What is on disk for an instance is the union of the day indexes; its gaps never overlap it.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead as _, BufReader, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    files::{self, Loaded},
    model::{day_name, day_of, parse_day_name},
};

/// 2 indexes the contexts needs were derived and tests started under as well.
pub const PROBE_FORMAT: u32 = 2;
/// Days of events kept.
pub const PROBE_RETENTION_DAYS: i64 = 90;
/// Pages one collection reads at most; the rest waits for the next one.
pub const PROBE_PAGES: usize = 5;
pub const PROBE_PAGE_SIZE: u32 = 1000;
/// Stops noted for instances not seen yet, kept for when their first events arrive.
const PENDING_STOPS: usize = 64;

/// Fields of an event naming a context version, and the kind of context each names.
const CONTEXT_FIELDS: [(&str, &str); 6] = [
    ("config", "config"),
    ("exclusions", "exclusions"),
    ("derivedConfig", "config"),
    ("derivedExclusions", "exclusions"),
    ("startConfig", "config"),
    ("startExclusions", "exclusions"),
];

/// Sequence numbers, both ends inclusive.
pub type SeqRange = [u64; 2];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeqGapReason {
    /// The core dropped the events before they were collected.
    Dropped,
    /// A day file holding them could not be read.
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqGap {
    pub from: u64,
    pub to: u64,
    pub reason: SeqGapReason,
}

/// What is known of the events of one core process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInstance {
    #[serde(default)]
    pub started_at: String,
    /// Every event up to this one is on disk, recorded as a gap, or expired.
    pub cursor: u64,
    /// The events on disk.
    #[serde(default)]
    pub written: Vec<SeqRange>,
    #[serde(default)]
    pub gaps: Vec<SeqGap>,
    /// The index of what was collected was lost: gaps and the stop may be unknown.
    #[serde(default)]
    pub coverage_unknown: bool,
    /// The app stopped or replaced the core after a final collection.
    #[serde(default)]
    pub stopped: bool,
    /// The core is gone: the app stopped it, or the usage was collected from a core seen
    /// after it.
    #[serde(default)]
    pub ended: bool,
    /// When the store first saw the instance, in the order it saw instances; 0 for one found
    /// only in the day files.
    #[serde(default)]
    pub seen: u64,
}

/// What one day file holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayIndex {
    /// Bytes of the file the index accounts for.
    pub committed: u64,
    /// Instance → sequence numbers in the file.
    #[serde(default)]
    pub seqs: BTreeMap<String, Vec<SeqRange>>,
    /// Instance → contexts the file's events refer to, as `config:<v>` or `exclusions:<v>`.
    #[serde(default)]
    pub contexts: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeMeta {
    pub format: u32,
    /// The first statistics day kept; earlier days expired. Saved before they are deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_since: Option<String>,
    #[serde(default)]
    pub instances: BTreeMap<String, ProbeInstance>,
    /// Instances the app ended before any of their events was collected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_stops: Vec<String>,
    /// The order number of the instance seen last.
    #[serde(default)]
    pub seen: u64,
    #[serde(default)]
    pub days: BTreeMap<String, DayIndex>,
}

/// The contexts kept for one instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceContexts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Value>,
    /// Contexts the core no longer kept when asked.
    #[serde(default)]
    pub unavailable: BTreeSet<String>,
}

/// What a collection from an instance reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePlan {
    /// Read the events after this one.
    pub after: u64,
    /// Ranges recorded as corrupt that the core still keeps, to read again.
    pub refetch: Vec<SeqRange>,
}

/// Contexts an instance's events refer to that are not kept yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingContexts {
    pub params: bool,
    pub configs: Vec<u64>,
    pub exclusions: Vec<u64>,
}

impl MissingContexts {
    pub const fn is_empty(&self) -> bool {
        !self.params && self.configs.is_empty() && self.exclusions.is_empty()
    }
}

/// What the complete lines of a day file hold.
#[derive(Debug, Default)]
struct DayScan {
    seqs: BTreeMap<String, Vec<SeqRange>>,
    contexts: BTreeMap<String, BTreeSet<String>>,
    /// The byte after the last complete line.
    end: u64,
    /// Complete lines that could not be read.
    bad: usize,
}

impl DayScan {
    fn add(&mut self, instance: String, seq: u64, refs: BTreeSet<String>) {
        add_seq(self.seqs.entry(instance.clone()).or_default(), seq);
        self.contexts.entry(instance).or_default().extend(refs);
    }

    fn into_index(self, committed: u64) -> DayIndex {
        DayIndex {
            committed,
            seqs: self.seqs,
            contexts: self.contexts,
        }
    }
}

pub struct ProbeStore {
    dir: PathBuf,
    day_offset: i64,
    meta: ProbeMeta,
}

impl ProbeStore {
    /// Opens the records in `dir`, taking lines appended after the index was last saved into
    /// it. Without a readable index, it is rebuilt from the day files; what the lost index
    /// told of their instances, gaps and stops, is then unknown.
    pub fn open(dir: &Path, day_offset: i64) -> Result<Self> {
        files::create_dir(dir)?;
        files::create_dir(&dir.join("contexts"))?;
        let meta_path = dir.join("meta.json");
        let meta = match files::load::<ProbeMeta>(&meta_path)? {
            Loaded::Ok(meta) => Some(meta),
            Loaded::Missing => None,
            Loaded::Corrupt => {
                files::quarantine(&meta_path)?;
                None
            }
        };
        let rebuilt = meta.is_none();
        let mut store = Self {
            dir: dir.to_owned(),
            day_offset,
            meta: meta.unwrap_or_else(|| ProbeMeta {
                format: PROBE_FORMAT,
                ..ProbeMeta::default()
            }),
        };
        if store.meta.format < PROBE_FORMAT {
            // The index of an earlier format lacks contexts events refer to: read the files
            // anew.
            for name in store.day_names()? {
                store.verify_day(&name)?;
            }
            store.meta.format = PROBE_FORMAT;
        }
        store.repair()?;
        if rebuilt {
            for entry in store.meta.instances.values_mut() {
                entry.coverage_unknown = true;
                // a running core is read on after what the files hold
                entry.cursor = entry.cursor.max(last(&entry.written).unwrap_or(0));
            }
        }
        store.save_meta()?;
        Ok(store)
    }

    pub const fn meta(&self) -> &ProbeMeta {
        &self.meta
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    fn day_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.jsonl"))
    }

    fn contexts_path(&self, instance: &str) -> Result<PathBuf> {
        if instance.is_empty() || !instance.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("unexpected core instance {instance:?}");
        }
        Ok(self.dir.join("contexts").join(format!("{instance}.json")))
    }

    fn save_meta(&self) -> Result<()> {
        files::save(&self.meta_path(), &self.meta)
    }

    /// Names of the day files, ascending.
    fn day_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            if let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".jsonl"))
                && parse_day_name(stem).is_some()
            {
                names.push(stem.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Whether a day is before the retention: its file is to be deleted, and its events are
    /// no loss.
    fn expired(&self, name: &str) -> bool {
        match (&self.meta.retained_since, parse_day_name(name)) {
            (Some(since), Some(day)) => parse_day_name(since).is_some_and(|since| day < since),
            _ => false,
        }
    }

    /// The entry of an instance a collection or the usage tells of, created as seen last,
    /// with a stop noted before it was seen.
    fn instance_entry(&mut self, instance: &str) -> &mut ProbeInstance {
        if !self.meta.instances.contains_key(instance) {
            self.meta.seen += 1;
            let seen = self.meta.seen;
            self.found_entry(instance).seen = seen;
        }
        self.found_entry(instance)
    }

    /// The entry of an instance, created with a stop noted before it was seen; one found only
    /// in the day files is not known to be seen before or after any other.
    fn found_entry(&mut self, instance: &str) -> &mut ProbeInstance {
        let stopped = take_pending(&mut self.meta.pending_stops, instance);
        let entry = self.meta.instances.entry(instance.to_owned()).or_default();
        if stopped {
            entry.stopped = true;
            entry.ended = true;
        }
        entry
    }

    /// Brings the index up to date with the day files: lines after the committed byte, and
    /// files the index does not know, are taken in; an incomplete last line is cut off; an
    /// indexed file that went missing or shrank is checked against the index. Expired days
    /// a deletion left behind are deleted.
    pub fn repair(&mut self) -> Result<()> {
        let names = self.day_names()?;
        let mut lost: BTreeMap<String, Vec<SeqRange>> = BTreeMap::new();
        for name in &names {
            let path = self.day_path(name);
            if self.expired(name) {
                files::remove(&path)?;
                self.meta.days.remove(name);
                continue;
            }
            let len = std::fs::metadata(&path)?.len();
            let committed = self.meta.days.get(name).map_or(0, |index| index.committed);
            if len == committed {
                continue;
            }
            if len < committed {
                self.verify_day(name)?;
                continue;
            }
            let scan = scan_day(&path, committed)?;
            if scan.bad > 0 {
                self.verify_day(name)?;
                continue;
            }
            // Lines a write left behind may not be on disk yet: the index takes them in only
            // once they are.
            if scan.end < len {
                truncate(&path, scan.end)?;
            } else {
                sync_file(&path)?;
            }
            if !self.meta.days.contains_key(name) {
                files::sync_parent(&path)?;
            }
            let end = scan.end;
            let index = self.meta.days.entry(name.clone()).or_default();
            for (instance, ranges) in scan.seqs {
                let seqs = index.seqs.entry(instance).or_default();
                for range in ranges {
                    add_range(seqs, range);
                }
            }
            for (instance, refs) in scan.contexts {
                index.contexts.entry(instance).or_default().extend(refs);
            }
            index.committed = end;
        }
        // Indexed files that are gone: their events are lost, unless they expired.
        let gone: Vec<String> = self
            .meta
            .days
            .keys()
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        for name in gone {
            if let Some(index) = self.meta.days.remove(&name)
                && !self.expired(&name)
            {
                for (instance, ranges) in index.seqs {
                    let entry = lost.entry(instance).or_default();
                    for range in ranges {
                        add_range(entry, range);
                    }
                }
            }
        }
        self.settle(&lost);
        Ok(())
    }

    /// Reads a whole day file and compares it with its index: indexed events no longer
    /// readable become corrupt gaps. A file with unreadable lines is copied aside, then
    /// replaced by its readable lines; it stays whole until the replacement is in place.
    fn verify_day(&mut self, name: &str) -> Result<()> {
        let path = self.day_path(name);
        let scan = scan_day(&path, 0)?;
        let len = std::fs::metadata(&path)?.len();
        let indexed = self.meta.days.get(name).map(|index| index.committed);
        if scan.bad > 0 || scan.end < len {
            rewrite_readable(&path)?;
        } else if indexed != Some(len) {
            // what the index is to take in has to be on disk first
            sync_file(&path)?;
            if indexed.is_none() {
                files::sync_parent(&path)?;
            }
        }
        let committed = std::fs::metadata(&path)?.len();
        let mut lost: BTreeMap<String, Vec<SeqRange>> = BTreeMap::new();
        match self.meta.days.get(name) {
            Some(index) => {
                for (instance, ranges) in &index.seqs {
                    let kept = scan.seqs.get(instance).map_or(&[][..], Vec::as_slice);
                    let missing = subtract(ranges, kept);
                    if !missing.is_empty() {
                        lost.insert(instance.clone(), missing);
                    }
                }
            }
            None => {
                for instance in scan.seqs.keys() {
                    self.found_entry(instance).coverage_unknown = true;
                }
            }
        }
        self.meta.days.insert(name.to_owned(), scan.into_index(committed));
        self.settle(&lost);
        Ok(())
    }

    /// Recomputes what is on disk for every instance from the day indexes, records `lost`
    /// events no day holds any more as corrupt gaps, and takes what is on disk out of every
    /// gap, keeping all the parts left of each.
    fn settle(&mut self, lost: &BTreeMap<String, Vec<SeqRange>>) {
        let mut written: BTreeMap<String, Vec<SeqRange>> = BTreeMap::new();
        for index in self.meta.days.values() {
            for (instance, ranges) in &index.seqs {
                let entry = written.entry(instance.clone()).or_default();
                for range in ranges {
                    add_range(entry, *range);
                }
            }
        }
        for instance in written.keys().chain(lost.keys()) {
            if !self.meta.instances.contains_key(instance) {
                self.found_entry(instance);
            }
        }
        for (instance, entry) in &mut self.meta.instances {
            entry.written = written.remove(instance).unwrap_or_default();
            if let Some(ranges) = lost.get(instance) {
                for range in subtract(ranges, &entry.written) {
                    add_gap(&mut entry.gaps, range, SeqGapReason::Corrupt);
                }
            }
            entry.gaps = entry
                .gaps
                .iter()
                .flat_map(|gap| {
                    subtract(&[[gap.from, gap.to]], &entry.written)
                        .into_iter()
                        .map(move |range| SeqGap {
                            from: range[0],
                            to: range[1],
                            reason: gap.reason,
                        })
                })
                .collect();
        }
    }

    /// Takes note of an instance a collection reads from, with the time the core started and
    /// the oldest event it keeps, and returns what to read. Events the core dropped before
    /// they were collected become a gap.
    pub fn begin(&mut self, instance: &str, started_at: &str, oldest: u64) -> Result<ProbePlan> {
        self.repair()?;
        let entry = self.instance_entry(instance);
        if entry.started_at.is_empty() {
            entry.started_at = started_at.to_owned();
        }
        if oldest > entry.cursor + 1 {
            for range in subtract(&[[entry.cursor + 1, oldest - 1]], &entry.written) {
                add_gap(&mut entry.gaps, range, SeqGapReason::Dropped);
            }
            entry.cursor = oldest - 1;
        }
        let refetch = entry
            .gaps
            .iter()
            .filter(|gap| gap.reason == SeqGapReason::Corrupt && gap.to >= oldest)
            .map(|gap| [gap.from.max(oldest), gap.to])
            .collect();
        let after = entry.cursor;
        self.save_meta()?;
        Ok(ProbePlan { after, refetch })
    }

    /// Appends events of an instance, read after `after`, leaving out those already on disk,
    /// and moves the collection on once they are on disk. Returns how many were appended.
    pub fn append(&mut self, instance: &str, schema: u32, after: u64, events: &[Value]) -> Result<usize> {
        // Lines on disk that the index is behind on count as on disk.
        self.repair()?;
        let written = self.instance_entry(instance).written.clone();
        let mut by_day: BTreeMap<String, Vec<PendingLine>> = BTreeMap::new();
        let mut last_seq = after;
        let mut taken = Vec::new();
        for event in events {
            let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
                continue;
            };
            last_seq = last_seq.max(seq);
            if contains(&written, seq) || contains(&taken, seq) {
                continue;
            }
            add_seq(&mut taken, seq);
            let mut line = event.clone();
            if let Some(object) = line.as_object_mut() {
                object.insert("instance".to_owned(), Value::from(instance));
                object.insert("schema".to_owned(), Value::from(schema));
            }
            let mut bytes = serde_json::to_vec(&line)?;
            bytes.push(b'\n');
            by_day
                .entry(day_name(self.event_day(event)))
                .or_default()
                .push((seq, bytes, context_refs(event)));
        }
        let mut appended = 0;
        for (name, lines) in by_day {
            let path = self.day_path(&name);
            let committed = self.meta.days.get(&name).map_or(0, |index| index.committed);
            let end = append_lines(&path, committed, lines.iter().map(|(_, bytes, _)| bytes.as_slice()))?;
            let index = self.meta.days.entry(name).or_default();
            index.committed = end;
            for (seq, _, refs) in lines {
                add_seq(index.seqs.entry(instance.to_owned()).or_default(), seq);
                index.contexts.entry(instance.to_owned()).or_default().extend(refs);
                appended += 1;
            }
        }
        self.settle(&BTreeMap::new());
        let entry = self.instance_entry(instance);
        if after <= entry.cursor {
            entry.cursor = entry.cursor.max(last_seq);
        }
        self.save_meta()?;
        Ok(appended)
    }

    fn event_day(&self, event: &Value) -> i64 {
        let secs = event
            .get("time")
            .and_then(Value::as_str)
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .map_or_else(|| chrono::Utc::now().timestamp(), |time| time.timestamp());
        day_of(secs.div_euclid(3600), self.day_offset)
    }

    /// Records that the app ended the core after a final collection, which ends it; saved
    /// every time, so an error tells it is not on disk. An instance not seen yet gets it once
    /// its first events arrive.
    pub fn mark_stopped(&mut self, instance: &str) -> Result<()> {
        let saved = self.meta.clone();
        match self.meta.instances.get_mut(instance) {
            Some(entry) => {
                entry.stopped = true;
                entry.ended = true;
            }
            None => add_pending(&mut self.meta.pending_stops, instance),
        }
        let result = self.save_meta();
        if result.is_err() {
            self.meta = saved;
        }
        result
    }

    /// Takes note of `current`, the core the usage was last collected from, and records every
    /// core seen before it as ended. The usage is collected from one core at a time, so a core
    /// seen before the one it was collected from last is gone; a core a collection of events
    /// read before the usage got to it is seen after it and stays. Saved every time something
    /// changed, so an error tells it is not on disk.
    pub fn end_before(&mut self, current: &str) -> Result<()> {
        let saved = self.meta.clone();
        let created = !self.meta.instances.contains_key(current);
        let seen = self.instance_entry(current).seen;
        let mut changed = created;
        for (instance, entry) in &mut self.meta.instances {
            if instance != current && !entry.ended && entry.seen < seen {
                entry.ended = true;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        let result = self.save_meta();
        if result.is_err() {
            self.meta = saved;
        }
        result
    }

    fn load_contexts(&self, instance: &str) -> Result<InstanceContexts> {
        let path = self.contexts_path(instance)?;
        Ok(match files::load::<InstanceContexts>(&path)? {
            Loaded::Ok(contexts) => contexts,
            Loaded::Missing => InstanceContexts::default(),
            Loaded::Corrupt => {
                files::quarantine(&path)?;
                InstanceContexts::default()
            }
        })
    }

    /// The contexts an instance's kept events refer to that are not kept, nor known to be
    /// unavailable.
    pub fn missing_contexts(&self, instance: &str) -> Result<MissingContexts> {
        let kept = self.load_contexts(instance)?;
        let mut missing = MissingContexts {
            params: kept.params.is_none() && !kept.unavailable.contains("params"),
            ..MissingContexts::default()
        };
        for index in self.meta.days.values() {
            for key in index.contexts.get(instance).into_iter().flatten() {
                if kept.contexts.contains_key(key) || kept.unavailable.contains(key) {
                    continue;
                }
                match parse_context_key(key) {
                    Some(("config", version)) if !missing.configs.contains(&version) => missing.configs.push(version),
                    Some(("exclusions", version)) if !missing.exclusions.contains(&version) => {
                        missing.exclusions.push(version);
                    }
                    _ => {}
                }
            }
        }
        missing.configs.sort_unstable();
        missing.exclusions.sort_unstable();
        Ok(missing)
    }

    /// Keeps the contexts the core returned for an instance, and those it no longer keeps as
    /// unavailable.
    pub fn save_contexts(&mut self, instance: &str, contexts: &[Value], unavailable: &[(String, u64)]) -> Result<()> {
        let mut kept = self.load_contexts(instance)?;
        for context in contexts {
            let kind = context.get("kind").and_then(Value::as_str).unwrap_or_default();
            if kind == "params" {
                kept.params = Some(context.clone());
                continue;
            }
            let Some(version) = context.get("version").and_then(Value::as_u64) else {
                continue;
            };
            kept.contexts.insert(context_key(kind, version), context.clone());
        }
        for (kind, version) in unavailable {
            kept.unavailable.insert(context_key(kind, *version));
        }
        // A context kept is not unavailable, whichever answer came first.
        let has_params = kept.params.is_some();
        kept.unavailable
            .retain(|key| !kept.contexts.contains_key(key) && !(key == "params" && has_params));
        files::save(&self.contexts_path(instance)?, &kept)
    }

    /// Records the contexts still missing of the instances that ended as unavailable: no core
    /// can return them any more.
    pub fn give_up_ended(&mut self) -> Result<()> {
        let ended: Vec<String> = self
            .meta
            .instances
            .iter()
            .filter(|(_, entry)| entry.ended)
            .map(|(instance, _)| instance.clone())
            .collect();
        for instance in ended {
            let missing = self.missing_contexts(&instance)?;
            if missing.is_empty() {
                continue;
            }
            let mut kept = self.load_contexts(&instance)?;
            if missing.params {
                kept.unavailable.insert("params".to_owned());
            }
            for version in &missing.configs {
                kept.unavailable.insert(context_key("config", *version));
            }
            for version in &missing.exclusions {
                kept.unavailable.insert(context_key("exclusions", *version));
            }
            files::save(&self.contexts_path(&instance)?, &kept)?;
        }
        Ok(())
    }

    /// Deletes the days older than the retention, and the contexts no kept event refers to.
    /// The new start of the retention is saved before any file is deleted, so that a deletion
    /// cut short is finished rather than taken for a loss.
    pub fn retain(&mut self, today: i64) -> Result<()> {
        let first = day_name(today - PROBE_RETENTION_DAYS + 1);
        if self.meta.retained_since.as_ref().is_none_or(|since| *since < first) {
            let expires =
                self.day_names()?.iter().any(|name| *name < first) || self.meta.days.keys().any(|name| *name < first);
            if expires {
                // files are deleted only by a start of the retention that is on disk
                let previous = self.meta.retained_since.replace(first);
                if let Err(error) = self.save_meta() {
                    self.meta.retained_since = previous;
                    return Err(error);
                }
            }
        }
        self.repair()?;
        self.save_meta()?;
        self.drop_unreferenced_contexts()
    }

    /// Contexts are deleted only once no day index refers to them, not by their age.
    fn drop_unreferenced_contexts(&mut self) -> Result<()> {
        let mut referenced: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for index in self.meta.days.values() {
            for (instance, keys) in &index.contexts {
                referenced
                    .entry(instance.clone())
                    .or_default()
                    .extend(keys.iter().cloned());
            }
            for instance in index.seqs.keys() {
                referenced.entry(instance.clone()).or_default();
            }
        }
        for entry in std::fs::read_dir(self.dir.join("contexts"))? {
            let name = entry?.file_name();
            let Some(instance) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            let ended = self.meta.instances.get(instance).is_none_or(|entry| entry.ended);
            match referenced.get(instance) {
                // the contexts of a core still running are kept for the events to come
                None if ended => files::remove(&self.contexts_path(instance)?)?,
                keys => {
                    let none = BTreeSet::new();
                    let keys = keys.unwrap_or(&none);
                    let mut kept = self.load_contexts(instance)?;
                    let before = kept.contexts.len();
                    kept.contexts.retain(|key, _| keys.contains(key));
                    if kept.contexts.len() != before {
                        files::save(&self.contexts_path(instance)?, &kept)?;
                    }
                }
            }
        }
        // Cores that ended with nothing kept are forgotten too, but for the gaps they still
        // show; one still running keeps its progress. What was noted of one stays noted in
        // case its events come late.
        let forgotten: Vec<String> = self
            .meta
            .instances
            .iter()
            .filter(|(instance, entry)| {
                !referenced.contains_key(*instance) && entry.written.is_empty() && entry.gaps.is_empty() && entry.ended
            })
            .map(|(instance, _)| instance.clone())
            .collect();
        if forgotten.is_empty() {
            return Ok(());
        }
        for instance in forgotten {
            if self.meta.instances.remove(&instance).is_some_and(|entry| entry.stopped) {
                add_pending(&mut self.meta.pending_stops, &instance);
            }
        }
        self.save_meta()
    }

    /// Writes every kept event, once each, to `events_path`, and to `meta_path` what explains
    /// them: the contexts they refer to and those missing or unavailable, and the coverage of
    /// each instance. Files are read line by line.
    pub fn export(&mut self, events_path: &Path, meta_path: &Path, client: &str, exported_at: &str) -> Result<()> {
        self.repair()?;
        for name in self.day_names()? {
            self.verify_day(&name)?;
        }
        self.save_meta()?;
        let file = std::fs::File::create(events_path)
            .with_context(|| format!("failed to create {}", events_path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut seen: BTreeMap<String, Vec<SeqRange>> = BTreeMap::new();
        for name in self.day_names()? {
            for_each_line(&self.day_path(&name), 0, |line| {
                let Some((instance, seq, _)) = parse_line(line) else {
                    return Ok(());
                };
                let seen = seen.entry(instance).or_default();
                if contains(seen, seq) {
                    return Ok(());
                }
                add_seq(seen, seq);
                writer.write_all(line)?;
                writer.write_all(b"\n")?;
                Ok(())
            })?;
        }
        writer.flush()?;
        writer.into_inner().map_err(|error| error.into_error())?.sync_all()?;

        let mut instances = BTreeMap::new();
        for (instance, entry) in &self.meta.instances {
            let contexts = self.load_contexts(instance)?;
            let referenced: BTreeSet<String> = self
                .meta
                .days
                .values()
                .filter_map(|index| index.contexts.get(instance))
                .flatten()
                .cloned()
                .collect();
            let missing: Vec<&String> = referenced
                .iter()
                .filter(|key| !contexts.contexts.contains_key(*key) && !contexts.unavailable.contains(*key))
                .collect();
            let unavailable: Vec<&String> = referenced
                .iter()
                .filter(|key| contexts.unavailable.contains(*key))
                .collect();
            let included: BTreeMap<&String, &Value> = contexts
                .contexts
                .iter()
                .filter(|(key, _)| referenced.contains(*key))
                .collect();
            instances.insert(
                instance.clone(),
                serde_json::json!({
                    "startedAt": entry.started_at,
                    "first": entry.written.first().map(|range| range[0]),
                    "last": last(&entry.written),
                    "gaps": entry.gaps,
                    // a core that is gone cannot fill its gaps any more
                    "coverageUnknown": entry.coverage_unknown || (entry.ended && !entry.gaps.is_empty()),
                    "stoppedAfterLastCollection": entry.stopped,
                    // The core does not report a last event: what an instance recorded after
                    // its last collection is unknown.
                    "tail": "unknown",
                    "params": contexts.params,
                    "paramsUnavailable": contexts.params.is_none() && contexts.unavailable.contains("params"),
                    "contexts": included,
                    "missingContexts": missing,
                    "unavailableContexts": unavailable,
                }),
            );
        }
        let meta = serde_json::json!({
            "format": PROBE_FORMAT,
            "client": client,
            "exportedAt": exported_at,
            "dayOffset": self.day_offset,
            "retainedSince": self.meta.retained_since,
            "instances": instances,
        });
        let file =
            std::fs::File::create(meta_path).with_context(|| format!("failed to create {}", meta_path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, &meta)?;
        writer.flush()?;
        writer.into_inner().map_err(|error| error.into_error())?.sync_all()?;
        Ok(())
    }
}

/// Notes an instance not seen yet, keeping the latest few.
fn add_pending(pending: &mut Vec<String>, instance: &str) {
    if !pending.iter().any(|noted| noted == instance) {
        pending.push(instance.to_owned());
        let excess = pending.len().saturating_sub(PENDING_STOPS);
        pending.drain(..excess);
    }
}

/// Takes an instance out of what was noted before it was seen, and reports whether it was.
fn take_pending(pending: &mut Vec<String>, instance: &str) -> bool {
    let at = pending.iter().position(|noted| noted == instance);
    if let Some(at) = at {
        pending.remove(at);
    }
    at.is_some()
}

/// Flushes a file the app wrote; flushing needs write access on Windows.
fn sync_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to flush {}", path.display()))
}

fn context_key(kind: &str, version: u64) -> String {
    format!("{kind}:{version}")
}

fn parse_context_key(key: &str) -> Option<(&str, u64)> {
    let (kind, version) = key.split_once(':')?;
    Some((kind, version.parse().ok()?))
}

/// The contexts an event refers to: those in effect when it was recorded, and those a need
/// was derived or a test started under.
fn context_refs(event: &Value) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for (field, kind) in CONTEXT_FIELDS {
        if let Some(version) = event.get(field).and_then(Value::as_u64)
            && version > 0
        {
            refs.insert(context_key(kind, version));
        }
    }
    refs
}

/// The instance, sequence number and contexts of a stored line.
fn parse_line(line: &[u8]) -> Option<(String, u64, BTreeSet<String>)> {
    if line.is_empty() {
        return None;
    }
    let event: Value = serde_json::from_slice(line).ok()?;
    let instance = event.get("instance")?.as_str()?.to_owned();
    let seq = event.get("seq")?.as_u64()?;
    Some((instance, seq, context_refs(&event)))
}

/// A line to append: its sequence number, its bytes and the contexts it refers to.
type PendingLine = (u64, Vec<u8>, BTreeSet<String>);

/// Calls `each` with every complete line of a file from `from`, without its newline, and
/// returns the byte after the last complete line.
fn for_each_line(path: &Path, from: u64, mut each: impl FnMut(&[u8]) -> Result<()>) -> Result<u64> {
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    file.seek(SeekFrom::Start(from))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut end = from;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            return Ok(end);
        }
        end += read as u64;
        line.pop();
        if !line.is_empty() {
            each(&line)?;
        }
    }
}

/// What the complete lines of a file hold from `from`.
fn scan_day(path: &Path, from: u64) -> Result<DayScan> {
    let mut scan = DayScan::default();
    scan.end = for_each_line(path, from, |line| {
        match parse_line(line) {
            Some((instance, seq, refs)) => scan.add(instance, seq, refs),
            None => scan.bad += 1,
        }
        Ok(())
    })?;
    Ok(scan)
}

/// Replaces a file by its readable complete lines, keeping a copy of it as it was. The file
/// stays in place, whole, until the replacement is on disk.
fn rewrite_readable(path: &Path) -> Result<()> {
    let copy = aside_path(path);
    std::fs::copy(path, &copy).with_context(|| format!("failed to copy {}", path.display()))?;
    sync_file(&copy)?;
    files::sync_parent(&copy)?;

    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    {
        let file = std::fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        for_each_line(path, 0, |line| {
            if parse_line(line).is_some() {
                writer.write_all(line)?;
                writer.write_all(b"\n")?;
            }
            Ok(())
        })?;
        writer.flush()?;
        writer.into_inner().map_err(|error| error.into_error())?.sync_all()?;
    }
    files::replace(&temp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    files::sync_parent(path)
}

/// A free name beside `path` for a copy of it that could not be read.
fn aside_path(path: &Path) -> PathBuf {
    let mut index = 0;
    loop {
        let mut name = path.as_os_str().to_owned();
        name.push(if index == 0 {
            ".corrupt".to_owned()
        } else {
            format!(".corrupt.{index}")
        });
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn truncate(path: &Path, len: u64) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

/// Appends lines to a day file whose every byte the index accounts for, and returns the end
/// of the file once they are on disk.
fn append_lines<'a>(path: &Path, committed: u64, lines: impl Iterator<Item = &'a [u8]>) -> Result<u64> {
    let created = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    if file.metadata()?.len() != committed {
        bail!("{} holds lines its index does not account for", path.display());
    }
    for line in lines {
        file.write_all(line)?;
    }
    file.sync_all()?;
    let end = file.metadata()?.len();
    if created {
        files::sync_parent(path)?;
    }
    Ok(end)
}

fn last(ranges: &[SeqRange]) -> Option<u64> {
    ranges.last().map(|range| range[1])
}

fn contains(ranges: &[SeqRange], seq: u64) -> bool {
    ranges.iter().any(|range| range[0] <= seq && seq <= range[1])
}

/// Adds a sequence number to sorted, merged ranges.
fn add_seq(ranges: &mut Vec<SeqRange>, seq: u64) {
    add_range(ranges, [seq, seq]);
}

fn add_range(ranges: &mut Vec<SeqRange>, range: SeqRange) {
    ranges.push(range);
    ranges.sort_unstable();
    let mut merged: Vec<SeqRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(prev) if range[0] <= prev[1].saturating_add(1) => prev[1] = prev[1].max(range[1]),
            _ => merged.push(range),
        }
    }
    *ranges = merged;
}

/// The parts of `ranges` not in `minus`, which is sorted and merged.
fn subtract(ranges: &[SeqRange], minus: &[SeqRange]) -> Vec<SeqRange> {
    let mut left = Vec::new();
    for range in ranges {
        let mut from = range[0];
        for cut in minus {
            if cut[1] < from || cut[0] > range[1] {
                continue;
            }
            if cut[0] > from {
                left.push([from, cut[0] - 1]);
            }
            from = cut[1].saturating_add(1);
            if from > range[1] {
                break;
            }
        }
        if from <= range[1] && from >= range[0] {
            left.push([from, range[1]]);
        }
    }
    left
}

fn add_gap(gaps: &mut Vec<SeqGap>, range: SeqRange, reason: SeqGapReason) {
    if range[0] > range[1] {
        return;
    }
    for gap in gaps.iter_mut() {
        if gap.reason == reason && range[0] <= gap.to.saturating_add(1) && gap.from <= range[1].saturating_add(1) {
            gap.from = gap.from.min(range[0]);
            gap.to = gap.to.max(range[1]);
            return;
        }
    }
    gaps.push(SeqGap {
        from: range[0],
        to: range[1],
        reason,
    });
    gaps.sort_by_key(|gap| gap.from);
}
