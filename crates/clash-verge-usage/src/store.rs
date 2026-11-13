use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    files::{self, Loaded},
    model::{
        Counts, FORMAT, HourData, Identity, InstanceState, Meta, MonthShard, ProxyRecord, Route, State, UsageGap,
        UsageGapReason, add, day_name, day_of, day_start, hour_total, merge_max, month_days, month_name,
        parse_day_name,
    },
};

/// Days whose last hour is this many hours old are past what the core keeps, so no later
/// collection can add to them.
const ARCHIVE_AFTER_HOURS: i64 = 32 * 24;
/// Hours requested from the core at a time when catching up.
pub const BATCH_HOURS: i64 = 48;

/// The usage the core reported for a range of hours.
#[derive(Debug, Clone)]
pub struct CoreUsage {
    pub instance: String,
    pub started_hour: i64,
    pub hour: i64,
    pub settled: i64,
    pub retained_since: i64,
    pub entries: Vec<CoreEntry>,
}

#[derive(Debug, Clone)]
pub struct CoreEntry {
    pub identity: Identity,
    /// The kind and route the traffic was counted for; none from a core that does not tell
    /// them apart.
    pub route: Option<Route>,
    pub buckets: Vec<CoreBucket>,
}

#[derive(Debug, Clone, Copy)]
pub struct CoreBucket {
    pub hour: i64,
    pub up: u64,
    pub down: u64,
}

/// What a store created for the first time is set up with.
pub struct InitParams {
    pub client: String,
    pub day_offset: i64,
    pub created_at: String,
}

/// The usage history kept in one directory.
///
/// Files are replaced atomically and in an order that never lets the collection progress get
/// ahead of the data: routes and proxy identities, then the days written, then the state.
///
/// The routes are kept apart from the identities, as an app that does not know them rewrites
/// the identities without them.
pub struct Store {
    pub(crate) dir: PathBuf,
    pub(crate) meta: Meta,
    pub(crate) proxies: BTreeMap<String, ProxyRecord>,
    pub(crate) routes: BTreeMap<String, Route>,
    pub(crate) state: State,
    gaps_changed: bool,
    /// Identities added or updated since the dictionary was last saved, or, until the first
    /// collection saves it, since the app started: a backup left behind by an app that ended
    /// between saving a dictionary and its backup is brought up to date.
    proxies_dirty: bool,
    /// Routes added since they were last saved; set from the start likewise.
    routes_dirty: bool,
}

impl Store {
    pub fn open(dir: &Path, init: impl FnOnce() -> InitParams, now_hour: i64) -> Result<Self> {
        files::create_dir(dir)?;
        files::create_dir(&dir.join("hours"))?;
        files::create_dir(&dir.join("days"))?;
        let has_history = has_history(dir)?;
        let meta = open_meta(dir, has_history, init)?;
        let proxies = load_or_backup(&dir.join("proxies.json"), &dir.join("proxies.json.bak"))?.unwrap_or_default();
        let routes = load_or_backup(&dir.join("routes.json"), &dir.join("routes.json.bak"))?.unwrap_or_default();
        let state = load_or_backup(&dir.join("state.json"), &dir.join("state.json.bak"))?;
        let mut store = Self {
            dir: dir.to_owned(),
            meta,
            proxies,
            routes,
            state: state.clone().unwrap_or_else(|| State {
                format: FORMAT,
                ..State::default()
            }),
            gaps_changed: false,
            proxies_dirty: true,
            routes_dirty: true,
        };
        // Before any file is checked, as an unreadable one saves the state.
        if state.is_none() && has_history {
            store.state.coverage_unknown_before = Some(store.named_history_end()?.max(now_hour + 1));
            store.save_state()?;
        }
        store.check_files()?;
        store.archive(now_hour, true)?;
        Ok(store)
    }

    pub const fn meta(&self) -> &Meta {
        &self.meta
    }

    pub const fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn hour_path(&self, day: i64) -> PathBuf {
        self.dir.join("hours").join(format!("{}.json", day_name(day)))
    }

    pub(crate) fn shard_path(&self, day: i64) -> PathBuf {
        self.dir.join("days").join(format!("{}.json", month_name(day)))
    }

    /// Takes note of the core a collection is about to read from. A different core than the
    /// last one seen ends the last one: what it did after its last collection is a gap unless
    /// the app stopped it after a final collection.
    pub fn observe(&mut self, core: &CoreUsage, now_secs: i64) -> Result<()> {
        let mut changed = false;
        match self.state.instance.as_mut() {
            Some(current) if current.id == core.instance => {
                if core.hour > current.last_hour || current.stopped {
                    current.last_hour = current.last_hour.max(core.hour);
                    current.stopped = false;
                    changed = true;
                }
            }
            _ => {
                if let Some(previous) = self.state.instance.take()
                    && !previous.stopped
                {
                    self.add_gap(
                        previous.done + 1,
                        previous.last_hour.max(core.started_hour),
                        UsageGapReason::CoreLost,
                        core.started_hour < previous.last_hour,
                    );
                }
                if core.started_hour < core.retained_since {
                    self.add_gap(
                        core.started_hour,
                        core.retained_since - 1,
                        UsageGapReason::Expired,
                        false,
                    );
                }
                self.state.instance = Some(InstanceState {
                    id: core.instance.clone(),
                    started_hour: core.started_hour,
                    done: core.started_hour.max(core.retained_since) - 1,
                    last_hour: core.hour,
                    last_collected: now_secs,
                    stopped: false,
                });
                changed = true;
            }
        }
        changed |= self.skip_expired(core.retained_since);
        if changed {
            self.save_state()?;
        }
        Ok(())
    }

    /// Records the hours the core no longer keeps, before `retained_since`, as a gap and moves
    /// the progress past them.
    fn skip_expired(&mut self, retained_since: i64) -> bool {
        let Some(from) = self.state.instance.as_ref().map(|current| current.done + 1) else {
            return false;
        };
        if from >= retained_since {
            return false;
        }
        self.add_gap(from, retained_since - 1, UsageGapReason::Expired, false);
        if let Some(current) = self.state.instance.as_mut() {
            current.done = retained_since - 1;
        }
        true
    }

    /// The hours to request next from the current core, or `None` before one is observed.
    pub fn next_range(&self) -> Option<(i64, i64)> {
        let current = self.state.instance.as_ref()?;
        let since = current.done + 1;
        Some((since, (current.done + BATCH_HOURS).min(current.last_hour.max(since))))
    }

    /// Stores what the current core reported for the hours requested up to `until`, then moves
    /// the progress to the last of them that the core has settled.
    pub fn apply(&mut self, core: &CoreUsage, until: i64, now_secs: i64) -> Result<()> {
        if self
            .state
            .instance
            .as_ref()
            .is_none_or(|current| current.id != core.instance)
        {
            bail!("usage came from core {} which is not the one observed", core.instance);
        }
        let mut days: BTreeMap<i64, HourData> = BTreeMap::new();
        // Hour → ids of the usage recorded before the core told routes apart, which the usage
        // split by route now collected for the same proxies replaces.
        let mut superseded: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
        for entry in &core.entries {
            let id = entry.identity.id_with(entry.route.as_ref());
            if let Some(route) = &entry.route
                && !self.routes.contains_key(&id)
            {
                self.routes.insert(id.clone(), route.clone());
                self.routes_dirty = true;
            }
            for bucket in &entry.buckets {
                self.proxies_dirty |= self.see_proxy(&id, &entry.identity, bucket.hour);
                if entry.route.is_some() {
                    superseded.entry(bucket.hour).or_default().insert(entry.identity.id());
                }
                days.entry(day_of(bucket.hour, self.meta.day_offset))
                    .or_default()
                    .entry(bucket.hour)
                    .or_default()
                    .entry(id.clone())
                    .or_default()
                    .insert(core.instance.clone(), [bucket.up, bucket.down]);
            }
        }
        if self.routes_dirty {
            self.save_routes()?;
            self.routes_dirty = false;
        }
        if self.proxies_dirty {
            self.save_proxies()?;
            self.proxies_dirty = false;
        }
        let done_before = self.state.instance.as_ref().map_or(i64::MIN, |current| current.done);
        for (day, hours) in days {
            self.merge_day(day, hours, &superseded, &core.instance)?;
        }
        // A day found unreadable moved the progress back to be collected again.
        let rolled_back = self
            .state
            .instance
            .as_ref()
            .is_some_and(|current| current.done < done_before);
        // The core no longer kept what it did not report from before its retained start.
        self.skip_expired(core.retained_since);
        if let Some(current) = self.state.instance.as_mut() {
            if !rolled_back {
                current.done = current.done.max(until.min(core.settled));
            }
            current.last_hour = current.last_hour.max(core.hour);
            current.last_collected = now_secs;
        }
        self.save_state()
    }

    /// Records that the app ended the core after a final collection, so its end is no gap. Saved
    /// every time, as an earlier attempt may have set it without saving it.
    pub fn mark_stopped(&mut self, instance: &str) -> Result<()> {
        match self.state.instance.as_mut() {
            Some(current) if current.id == instance => {
                current.stopped = true;
                self.save_state()
            }
            _ => Ok(()),
        }
    }

    fn see_proxy(&mut self, id: &str, identity: &Identity, hour: i64) -> bool {
        match self.proxies.get_mut(id) {
            Some(record) => {
                let (first, last) = (record.first_seen.min(hour), record.last_seen.max(hour));
                let changed = (first, last) != (record.first_seen, record.last_seen);
                record.first_seen = first;
                record.last_seen = last;
                changed
            }
            None => {
                self.proxies.insert(
                    id.to_owned(),
                    ProxyRecord {
                        identity: identity.clone(),
                        first_seen: hour,
                        last_seen: hour,
                    },
                );
                true
            }
        }
    }

    /// Writes collected hours into the day's only source: its hour file, or, for a day already
    /// archived, the hours that came late. The file is written even when nothing changed, as
    /// an earlier attempt may have failed after replacing it but before it was durable.
    ///
    /// The usage of `instance` recorded under a `superseded` id in an hour is dropped first:
    /// an app that did not split usage by route collected part of that hour from the same
    /// core, and what was just collected split by route adds up to no less.
    fn merge_day(
        &mut self,
        day: i64,
        collected: HourData,
        superseded: &BTreeMap<i64, BTreeSet<String>>,
        instance: &str,
    ) -> Result<()> {
        if let Some(mut hours) = self.read_hours(day)? {
            drop_superseded(&mut hours, superseded, instance);
            merge_hours(&mut hours, collected);
            return files::save(&self.hour_path(day), &hours);
        }
        let mut shard = self.read_shard(day)?;
        if let Some(archived) = shard.get_mut(&day_name(day)) {
            archived.time_anomaly = true;
            drop_superseded(&mut archived.late, superseded, instance);
            merge_hours(&mut archived.late, collected);
            return files::save(&self.shard_path(day), &shard);
        }
        files::save(&self.hour_path(day), &collected)
    }

    /// Reads a day's hour file. An unreadable one is recorded as a gap, and, if the current
    /// core still keeps that day, the progress is moved back to collect it again; both are on
    /// disk before the file is set aside.
    pub(crate) fn read_hours(&mut self, day: i64) -> Result<Option<HourData>> {
        let path = self.hour_path(day);
        match files::load::<HourData>(&path)? {
            Loaded::Ok(hours) => Ok(Some(hours)),
            Loaded::Missing => Ok(None),
            Loaded::Corrupt => {
                let start = day_start(day, self.meta.day_offset);
                self.add_gap(start, start + 23, UsageGapReason::Corrupt, false);
                if let Some(current) = self.state.instance.as_mut() {
                    current.done = current.done.min((start - 1).max(current.started_hour - 1));
                }
                self.save_state()?;
                files::quarantine(&path)?;
                Ok(None)
            }
        }
    }

    pub(crate) fn read_shard(&mut self, day: i64) -> Result<MonthShard> {
        let path = self.shard_path(day);
        match files::load::<MonthShard>(&path)? {
            Loaded::Ok(shard) => Ok(shard),
            Loaded::Missing => Ok(MonthShard::new()),
            Loaded::Corrupt => {
                let (first, last) = month_days(day);
                let offset = self.meta.day_offset;
                self.add_gap(
                    day_start(first, offset),
                    day_start(last + 1, offset) - 1,
                    UsageGapReason::Corrupt,
                    false,
                );
                self.save_state()?;
                files::quarantine(&path)?;
                Ok(MonthShard::new())
            }
        }
    }

    /// Sets aside unreadable day files found at startup, collecting again what the current
    /// core still keeps.
    fn check_files(&mut self) -> Result<()> {
        for day in self.hour_days()? {
            self.read_hours(day)?;
        }
        for month in self.shard_months()? {
            self.read_shard(month)?;
        }
        Ok(())
    }

    /// The hour after the last day that the names of the stored files cover.
    fn named_history_end(&self) -> Result<i64> {
        let offset = self.meta.day_offset;
        let hours_end = self.hour_days()?.last().map(|day| day_start(day + 1, offset));
        let days_end = self
            .shard_months()?
            .last()
            .map(|month| day_start(month_days(*month).1 + 1, offset));
        Ok(hours_end.max(days_end).unwrap_or(i64::MIN))
    }

    /// Every statistics day with stored usage, ascending.
    pub(crate) fn history_days(&mut self) -> Result<Vec<i64>> {
        let mut days = self.hour_days()?;
        for month in self.shard_months()? {
            days.extend(self.read_shard(month)?.keys().filter_map(|name| parse_day_name(name)));
        }
        days.sort_unstable();
        days.dedup();
        Ok(days)
    }

    /// Folds the hour files of days the core can no longer add to into day totals.
    ///
    /// A day with both an hour file and a day total was interrupted between writing the total
    /// and removing the hours. It is finished if still old enough; otherwise the clock went
    /// back, the core may still report its hours, and the total made from the hour file is
    /// dropped so that the hours stay its only source. With `unfinished_only`, other days are
    /// left alone.
    pub fn archive(&mut self, now_hour: i64, unfinished_only: bool) -> Result<()> {
        for day in self.hour_days()? {
            let old_enough = day_start(day + 1, self.meta.day_offset) <= now_hour - ARCHIVE_AFTER_HOURS;
            let mut shard = self.read_shard(day)?;
            let name = day_name(day);
            let interrupted = shard.get(&name).is_some_and(|archived| !archived.base.is_empty());
            if old_enough && (interrupted || !unfinished_only) {
                let Some(hours) = self.read_hours(day)? else {
                    continue;
                };
                shard.entry(name).or_default().base = day_totals(&hours);
                files::save(&self.shard_path(day), &shard)?;
                files::remove(&self.hour_path(day))?;
            } else if !old_enough && interrupted {
                if shard.get(&name).is_some_and(|archived| archived.late.is_empty()) {
                    shard.remove(&name);
                } else if let Some(archived) = shard.get_mut(&name) {
                    archived.base.clear();
                }
                files::save(&self.shard_path(day), &shard)?;
            }
        }
        self.save_state_if_gaps_changed()
    }

    pub(crate) fn hour_days(&self) -> Result<Vec<i64>> {
        let mut days = list_json_names(&self.dir.join("hours"))?
            .iter()
            .filter_map(|name| parse_day_name(name))
            .collect::<Vec<_>>();
        days.sort_unstable();
        Ok(days)
    }

    /// A day in each month that has a shard.
    pub(crate) fn shard_months(&self) -> Result<Vec<i64>> {
        let mut days = list_json_names(&self.dir.join("days"))?
            .iter()
            .filter_map(|name| parse_day_name(&format!("{name}-01")))
            .collect::<Vec<_>>();
        days.sort_unstable();
        Ok(days)
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
            self.gaps_changed = true;
        }
    }

    fn save_state_if_gaps_changed(&mut self) -> Result<()> {
        if self.gaps_changed {
            self.save_state()?;
        }
        Ok(())
    }

    fn save_state(&mut self) -> Result<()> {
        save_with_backup(
            &self.dir.join("state.json"),
            &self.dir.join("state.json.bak"),
            &self.state,
        )?;
        self.gaps_changed = false;
        Ok(())
    }

    fn save_proxies(&self) -> Result<()> {
        save_with_backup(
            &self.dir.join("proxies.json"),
            &self.dir.join("proxies.json.bak"),
            &self.proxies,
        )
    }

    fn save_routes(&self) -> Result<()> {
        save_with_backup(
            &self.dir.join("routes.json"),
            &self.dir.join("routes.json.bak"),
            &self.routes,
        )
    }
}

fn drop_superseded(hours: &mut HourData, superseded: &BTreeMap<i64, BTreeSet<String>>, instance: &str) {
    for (hour, ids) in superseded {
        let Some(proxies) = hours.get_mut(hour) else {
            continue;
        };
        for id in ids {
            if let Some(instances) = proxies.get_mut(id) {
                instances.remove(instance);
                if instances.is_empty() {
                    proxies.remove(id);
                }
            }
        }
        if proxies.is_empty() {
            hours.remove(hour);
        }
    }
}

fn merge_hours(into: &mut HourData, from: HourData) {
    for (hour, proxies) in from {
        for (id, instances) in proxies {
            for (instance, counts) in instances {
                let slot = into
                    .entry(hour)
                    .or_default()
                    .entry(id.clone())
                    .or_default()
                    .entry(instance)
                    .or_insert([0, 0]);
                merge_max(slot, counts);
            }
        }
    }
}

pub(crate) fn day_totals(hours: &HourData) -> BTreeMap<String, Counts> {
    let mut totals = BTreeMap::<String, Counts>::new();
    for proxies in hours.values() {
        for (id, instances) in proxies {
            add(totals.entry(id.clone()).or_insert([0, 0]), hour_total(instances));
        }
    }
    totals
}

fn open_meta(dir: &Path, has_history: bool, init: impl FnOnce() -> InitParams) -> Result<Meta> {
    let (path, backup) = (dir.join("meta.json"), dir.join("meta.json.bak"));
    if let Some(mut meta) = load_or_backup::<Meta>(&path, &backup)? {
        if meta.format < FORMAT {
            // the files of the earlier formats stay as they are
            meta.format = FORMAT;
            save_with_backup(&path, &backup, &meta)?;
        } else if !backup.exists() {
            files::save(&backup, &meta)?;
        }
        return Ok(meta);
    }
    if has_history {
        bail!(
            "the usage history in {} has lost its metadata; restore meta.json to keep using it",
            dir.display()
        );
    }
    let init = init();
    let meta = Meta {
        format: FORMAT,
        client: init.client,
        day_offset: init.day_offset,
        created_at: init.created_at,
    };
    save_with_backup(&path, &backup, &meta)?;
    Ok(meta)
}

fn has_history(dir: &Path) -> Result<bool> {
    Ok([
        "proxies.json",
        "proxies.json.bak",
        "routes.json",
        "routes.json.bak",
        "state.json",
        "state.json.bak",
    ]
    .iter()
    .any(|name| dir.join(name).exists())
        || !list_json_names(&dir.join("hours"))?.is_empty()
        || !list_json_names(&dir.join("days"))?.is_empty())
}

/// Names of the `.json` files in a directory, without the extension.
fn list_json_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) {
            names.push(stem.to_owned());
        }
    }
    Ok(names)
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

fn save_with_backup<T: Serialize>(path: &Path, backup: &Path, value: &T) -> Result<()> {
    files::save(path, value)?;
    files::save(backup, value)
}
