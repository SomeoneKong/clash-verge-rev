//! Queries and export of the connection stats.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Write as _,
    path::Path,
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    conns::{ConnCounts, ConnKey, ConnStore, Counting, HourRow, RecordingSpan, merge_counts},
    model::{UsageGap, day_of},
};

/// The longest range a query covers.
pub const CONN_QUERY_HOURS: i64 = 7 * 24;
/// Groups listed one by one; the rest are added up as the other.
const TOP_GROUPS: usize = 500;
/// Cached hours are let go after this long without a query.
const CACHE_IDLE_SECS: i64 = 600;

/// What one core counted in one hour, per key.
type CoreHour = (Counting, HashMap<String, ConnCounts>);
/// The same, with no counting when nothing of the core was taken.
type QueriedCore = (Option<Counting>, HashMap<String, ConnCounts>);
/// Hour → core instance → what it counted.
type Hours = BTreeMap<i64, BTreeMap<String, CoreHour>>;

/// The rows of a range of hours, by hour and core instance, as on disk.
pub(crate) struct Mirror {
    from: i64,
    to: i64,
    hours: Hours,
    used_at: i64,
}

impl Mirror {
    pub(crate) fn add(&mut self, row: &HourRow) {
        if !(self.from..=self.to).contains(&row.hour) {
            return;
        }
        add_row(&mut self.hours, row);
    }

    pub(crate) fn drop_before(&mut self, hour: i64) {
        self.hours.retain(|kept, _| *kept >= hour);
        self.from = self.from.max(hour);
    }
}

fn add_row(hours: &mut Hours, row: &HourRow) {
    let slot = hours
        .entry(row.hour)
        .or_default()
        .entry(row.instance.clone())
        .or_insert_with(|| (row.counting, HashMap::new()));
    slot.0 = slot.0.merge(row.counting);
    for (id, counts) in &row.entries {
        merge_counts(slot.1.entry(id.clone()).or_insert([0; 3]), *counts);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ConnDimension {
    Target,
    Process,
    /// The full chain, from the group a rule chose out to the proxy.
    Chain,
    /// The proxy that made the connections.
    Exit,
    Rule,
    /// The source IP.
    Source,
    Inbound,
    Network,
}

impl ConnDimension {
    pub fn value(self, key: &ConnKey) -> String {
        match self {
            Self::Target => key.target.clone(),
            Self::Process => key.process.clone(),
            Self::Chain => key.chain.iter().rev().cloned().collect::<Vec<_>>().join(" → "),
            Self::Exit => key.chain.first().cloned().unwrap_or_default(),
            Self::Rule if key.rule_payload.is_empty() => key.rule.clone(),
            Self::Rule => format!("{},{}", key.rule, key.rule_payload),
            Self::Source => key.src_ip.clone(),
            Self::Inbound if key.in_name.is_empty() => key.in_type.clone(),
            Self::Inbound => format!("{}/{}", key.in_type, key.in_name),
            Self::Network => key.network.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnFilter {
    pub dimension: ConnDimension,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnQuery {
    /// First and last UTC hour, both inclusive, at most a week apart.
    pub from_hour: i64,
    pub to_hour: i64,
    pub group_by: Vec<ConnDimension>,
    #[serde(default)]
    pub filters: Vec<ConnFilter>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnGroup {
    /// One value per dimension grouped by; a key no longer known has empty values.
    pub values: Vec<String>,
    pub up: u64,
    pub down: u64,
    /// Connections opened.
    pub conns: u64,
}

impl ConnGroup {
    const fn add(&mut self, counts: ConnCounts) {
        self.up = self.up.saturating_add(counts[0]);
        self.down = self.down.saturating_add(counts[1]);
        self.conns = self.conns.saturating_add(counts[2]);
    }
}

/// How far an hour was recorded, by the app's recording spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Recorded {
    Full,
    Part,
    None,
}

/// Whether what a core counted in an hour was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Collected {
    Taken,
    /// The core may still add to it, and it is not read in full yet.
    Pending,
    Gap,
}

/// What an hour of the result stands for, the least complete of its cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Unknown,
    NotRecorded,
    Gap,
    Pending,
    NotCounted,
    PartlyCounted,
    Complete,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnHourCore {
    pub instance: String,
    /// What the core said of its counting in the hour; none when nothing of it was taken.
    pub counting: Option<Counting>,
    pub collected: Collected,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnHour {
    pub hour: i64,
    pub coverage: Coverage,
    pub recorded: Recorded,
    pub cores: Vec<ConnHourCore>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnReport {
    pub from_hour: i64,
    pub to_hour: i64,
    /// The largest groups by traffic.
    pub groups: Vec<ConnGroup>,
    /// Everything else added up.
    pub other: Option<ConnGroup>,
    pub total: ConnGroup,
    pub hours: Vec<ConnHour>,
}

impl ConnStore {
    /// Groups what was counted from `from_hour` to `to_hour`. The hours are read into memory
    /// once and kept while queries go on; the hours the core may still add to come from what
    /// the last collection merged.
    pub fn query(&mut self, query: &ConnQuery, now_secs: i64) -> Result<ConnReport> {
        let (from, to) = (query.from_hour, query.to_hour);
        if to < from || to - from >= CONN_QUERY_HOURS {
            bail!("a query covers 1 to {CONN_QUERY_HOURS} hours");
        }
        self.ensure_cache(from, to)?;
        let mut groups: HashMap<Vec<String>, ConnGroup> = HashMap::new();
        let mut total = ConnGroup::default();
        let mut hours = Vec::new();
        for hour in from..=to {
            let mut cores: BTreeMap<String, QueriedCore> = BTreeMap::new();
            if let Some(rows) = self.cache.as_ref().and_then(|cache| cache.hours.get(&hour)) {
                for (instance, (counting, entries)) in rows {
                    cores.insert(instance.clone(), (Some(*counting), entries.clone()));
                }
            }
            let pending_core = self.pending_core(hour);
            if let Some(instance) = &pending_core
                && let Some(pending) = self.pending.get(&hour)
            {
                let core = cores.entry(instance.clone()).or_insert_with(|| (None, HashMap::new()));
                core.0 = Some(
                    core.0
                        .map_or(pending.counting, |counting| counting.merge(pending.counting)),
                );
                for (id, counts) in &pending.entries {
                    merge_counts(core.1.entry(id.clone()).or_insert([0; 3]), *counts);
                }
            }
            for (_, entries) in cores.values() {
                for (id, counts) in entries {
                    let Some(values) = self.group_values(id, query) else {
                        continue;
                    };
                    total.add(*counts);
                    groups
                        .entry(values.clone())
                        .or_insert_with(|| ConnGroup {
                            values,
                            ..ConnGroup::default()
                        })
                        .add(*counts);
                }
            }
            hours.push(self.hour_coverage(hour, &cores, pending_core.as_deref()));
        }
        if let Some(cache) = self.cache.as_mut() {
            cache.used_at = now_secs;
        }
        let mut groups: Vec<ConnGroup> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            (b.up + b.down)
                .cmp(&(a.up + a.down))
                .then(b.conns.cmp(&a.conns))
                .then(a.values.cmp(&b.values))
        });
        let other = (groups.len() > TOP_GROUPS).then(|| {
            let mut other = ConnGroup::default();
            for group in groups.drain(TOP_GROUPS..) {
                other.add([group.up, group.down, group.conns]);
            }
            other
        });
        Ok(ConnReport {
            from_hour: from,
            to_hour: to,
            groups,
            other,
            total,
            hours,
        })
    }

    /// Lets the cached hours go once queries stopped a while ago.
    pub fn release_idle_cache(&mut self, now_secs: i64) {
        if self
            .cache
            .as_ref()
            .is_some_and(|cache| now_secs - cache.used_at >= CACHE_IDLE_SECS)
        {
            self.cache = None;
        }
    }

    fn ensure_cache(&mut self, from: i64, to: i64) -> Result<()> {
        if self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.from <= from && to <= cache.to)
        {
            return Ok(());
        }
        let (first_day, last_day) = (day_of(from, self.day_offset), day_of(to, self.day_offset));
        let mut hours = BTreeMap::new();
        for day in self.days()? {
            if day < first_day || day > last_day {
                continue;
            }
            for row in self.read_day(day)? {
                if (from..=to).contains(&row.hour) {
                    add_row(&mut hours, &row);
                }
            }
        }
        self.cache = Some(Mirror {
            from,
            to,
            hours,
            used_at: i64::MIN,
        });
        Ok(())
    }

    /// The instance whose hour the core may still add to, when it is one.
    fn pending_core(&self, hour: i64) -> Option<String> {
        let current = self.state.instance.as_ref()?;
        (!current.stopped && hour > current.done && hour <= current.last_hour).then(|| current.id.clone())
    }

    /// The values of the dimensions grouped by, or `None` when a filter leaves the key out.
    fn group_values(&self, id: &str, query: &ConnQuery) -> Option<Vec<String>> {
        let Some(key) = self.keys.get(id) else {
            return query
                .filters
                .is_empty()
                .then(|| vec![String::new(); query.group_by.len()]);
        };
        if query
            .filters
            .iter()
            .any(|filter| filter.dimension.value(key) != filter.value)
        {
            return None;
        }
        Some(query.group_by.iter().map(|dimension| dimension.value(key)).collect())
    }

    fn hour_coverage(&self, hour: i64, cores: &BTreeMap<String, QueriedCore>, pending_core: Option<&str>) -> ConnHour {
        let recorded = recorded(&self.state.recording, hour);
        let gap = in_gaps(&self.state.gaps, hour);
        let mut list: Vec<ConnHourCore> = cores
            .iter()
            .map(|(instance, (counting, _))| ConnHourCore {
                instance: instance.clone(),
                counting: *counting,
                collected: if Some(instance.as_str()) == pending_core {
                    Collected::Pending
                } else {
                    Collected::Taken
                },
            })
            .collect();
        if let Some(instance) = pending_core
            && !cores.contains_key(instance)
        {
            list.push(ConnHourCore {
                instance: instance.to_owned(),
                counting: None,
                collected: Collected::Pending,
            });
        }
        if gap {
            for core in &mut list {
                core.collected = Collected::Gap;
            }
        }
        let coverage = if self.state.coverage_unknown_before.is_some_and(|before| hour < before) {
            Coverage::Unknown
        } else if recorded == Recorded::None {
            Coverage::NotRecorded
        } else if gap {
            Coverage::Gap
        } else if pending_core.is_some() {
            Coverage::Pending
        } else if list.is_empty() {
            // nothing was taken: complete only where no core ran, before the current one or
            // after the app stopped it
            match self.state.instance.as_ref() {
                // the current core was read past it, yet nothing of it is kept
                Some(current) if (current.started_hour..=current.done).contains(&hour) => Coverage::Unknown,
                Some(current) if hour <= current.last_hour || current.stopped => Coverage::Complete,
                _ => Coverage::Unknown,
            }
        } else {
            list.iter()
                .map(|core| match core.counting {
                    Some(Counting::On) | None => Coverage::Complete,
                    Some(Counting::Partial) => Coverage::PartlyCounted,
                    Some(Counting::Off) => Coverage::NotCounted,
                })
                .min()
                .unwrap_or(Coverage::Complete)
        };
        ConnHour {
            hour,
            coverage,
            recorded,
            cores: list,
        }
    }

    /// Writes every connection stat kept, whatever its age, to `path`: the keys, the hours by
    /// core instance, the hours the running core may still add to as they are now, and what
    /// tells how complete they are.
    pub fn export(&mut self, path: &Path, client: &str, exported_at: &str, now_hour: i64) -> Result<()> {
        let file = std::fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut head = serde_json::to_vec(&ExportHead {
            format: crate::conns::CONN_FORMAT,
            client,
            day_offset: self.day_offset,
            exported_at,
        })?;
        head.pop();
        writer.write_all(&head)?;
        writer.write_all(b",\"hours\":[")?;
        let mut referenced = BTreeSet::new();
        let mut countings: BTreeMap<i64, BTreeMap<String, QueriedCore>> = BTreeMap::new();
        let mut first = true;
        for day in self.days()? {
            let mut merged = BTreeMap::new();
            for row in self.read_day(day)? {
                add_row(&mut merged, &row);
            }
            for (hour, cores) in merged {
                for (instance, (counting, entries)) in cores {
                    countings
                        .entry(hour)
                        .or_default()
                        .insert(instance.clone(), (Some(counting), HashMap::new()));
                    referenced.extend(entries.keys().cloned());
                    if !first {
                        writer.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(
                        &mut writer,
                        &HourRow {
                            hour,
                            instance,
                            counting,
                            entries: entries.into_iter().collect(),
                        },
                    )?;
                }
            }
        }
        if let Some(current) = self.state.instance.as_ref() {
            for (hour, pending) in &self.pending {
                let core = countings
                    .entry(*hour)
                    .or_default()
                    .entry(current.id.clone())
                    .or_insert_with(|| (None, HashMap::new()));
                core.0 = Some(
                    core.0
                        .map_or(pending.counting, |counting| counting.merge(pending.counting)),
                );
            }
        }
        writer.write_all(b"],\"current\":")?;
        let current = self.state.instance.as_ref().map(|current| ExportCurrent {
            instance: current.id.clone(),
            hours: self.pending.clone().into_iter().collect(),
        });
        for pending in self.pending.values() {
            referenced.extend(pending.entries.keys().cloned());
        }
        serde_json::to_writer(&mut writer, &current)?;
        writer.write_all(b",\"keys\":{")?;
        let mut unknown = Vec::new();
        let mut first = true;
        for id in &referenced {
            let Some(key) = self.keys.get(id) else {
                unknown.push(id.clone());
                continue;
            };
            if !first {
                writer.write_all(b",")?;
            }
            first = false;
            serde_json::to_writer(&mut writer, id)?;
            writer.write_all(b":")?;
            serde_json::to_writer(&mut writer, key)?;
        }
        writer.write_all(b"},\"coverage\":")?;
        let mut hours = Vec::new();
        let first_hour = countings
            .keys()
            .next()
            .copied()
            .into_iter()
            .chain(self.state.gaps.iter().map(|gap| gap.from))
            .chain(
                self.state
                    .instance
                    .as_ref()
                    .filter(|current| !current.stopped)
                    .map(|current| current.done + 1),
            )
            .min();
        if let Some(from) = first_hour {
            let empty = BTreeMap::new();
            for hour in from.max(now_hour - crate::conns::CONN_RETENTION_DAYS * 24)..=now_hour {
                let pending = self.pending_core(hour);
                let coverage = self.hour_coverage(hour, countings.get(&hour).unwrap_or(&empty), pending.as_deref());
                if coverage.coverage != Coverage::Complete || !coverage.cores.is_empty() {
                    hours.push(coverage);
                }
            }
        }
        serde_json::to_writer(
            &mut writer,
            &ExportCoverage {
                gaps: &self.state.gaps,
                pending: self
                    .state
                    .instance
                    .as_ref()
                    .filter(|current| !current.stopped && current.done < current.last_hour)
                    .map(|current| [current.done + 1, current.last_hour]),
                coverage_unknown_before: self.state.coverage_unknown_before,
                recording: &self.state.recording,
                unknown_keys: &unknown,
                hours: &hours,
            },
        )?;
        writer.write_all(b"}")?;
        writer.flush()?;
        writer.into_inner()?.sync_all()?;
        Ok(())
    }
}

/// How much of an hour the recording spans cover.
fn recorded(spans: &[RecordingSpan], hour: i64) -> Recorded {
    let (start, end) = (hour * 3600, hour * 3600 + 3600);
    let covered: i64 = spans
        .iter()
        .map(|span| {
            let (from, to) = (span.from.max(start), span.to.unwrap_or(i64::MAX).min(end));
            (to - from).max(0)
        })
        .sum();
    match covered {
        0 => Recorded::None,
        3600.. => Recorded::Full,
        _ => Recorded::Part,
    }
}

fn in_gaps(gaps: &[UsageGap], hour: i64) -> bool {
    gaps.iter().any(|gap| (gap.from..=gap.to).contains(&hour))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportHead<'a> {
    format: u32,
    client: &'a str,
    day_offset: i64,
    exported_at: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportCurrent {
    instance: String,
    /// As collected when exported; the core may still add to them.
    hours: BTreeMap<i64, crate::conns::PendingHour>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportCoverage<'a> {
    gaps: &'a [UsageGap],
    pending: Option<[i64; 2]>,
    coverage_unknown_before: Option<i64>,
    recording: &'a [RecordingSpan],
    unknown_keys: &'a [String],
    hours: &'a [ConnHour],
}
