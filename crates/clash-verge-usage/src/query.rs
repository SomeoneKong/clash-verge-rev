use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::Entry},
    io::Write as _,
    path::Path,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    model::{
        Counts, DayArchive, HourData, MonthShard, ProxyRecord, Route, RouteKind, UsageGap, add, day_name, day_of,
        day_start, hour_total, month_days, month_name,
    },
    store::{Store, day_totals},
};

const TOP_ITEMS: usize = 10;
const MAX_DAILY_POINTS: i64 = 366;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum UsageRange {
    Last24Hours,
    Last7Days,
    Last30Days,
    Last12Months,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum UsageView {
    /// One item per proxy name within a provider: all the traffic the proxy carried.
    Name,
    /// One item per route of a proxy: the traffic of its own connections along the route.
    Route,
}

#[derive(Debug, Clone, Copy, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct UsageQuery {
    pub range: UsageRange,
    pub view: UsageView,
    pub include_builtin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum UsageGranularity {
    Hour,
    Day,
    Month,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub granularity: UsageGranularity,
    /// First and last UTC hour the result covers.
    pub from_hour: i64,
    pub to_hour: i64,
    /// The largest items by total, then everything else as the last entry of each point's values.
    pub items: Vec<UsageItem>,
    pub other: Counts,
    pub points: Vec<UsagePoint>,
    pub overflow: bool,
    /// With the route view, every route with its total, not only the largest.
    pub all_items: Vec<UsageItem>,
    pub gaps: Vec<UsageGap>,
    /// Hours the running core may still add to that are not collected yet.
    pub pending: Option<[i64; 2]>,
    /// Earlier gaps may have been lost with the state file.
    pub coverage_unknown_before: Option<i64>,
    /// Statistics days that got usage after they were archived.
    pub time_anomaly_days: Vec<String>,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct UsageItem {
    pub key: String,
    pub label: String,
    pub provider: Option<String>,
    pub proxy_type: Option<String>,
    pub unknown: bool,
    pub overflow: bool,
    /// With the route view, the route of the item.
    pub route: Option<Route>,
    pub total: Counts,
}

#[derive(Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct UsagePoint {
    pub start_hour: i64,
    /// The statistics day or month, for daily and monthly points.
    pub label: Option<String>,
    pub values: Vec<Counts>,
}

struct Buckets {
    granularity: UsageGranularity,
    /// UTC start hours, ascending, then the end of the last bucket.
    edges: Vec<i64>,
    labels: Vec<Option<String>>,
}

impl Buckets {
    fn index(&self, hour: i64) -> Option<usize> {
        let position = self.edges.partition_point(|edge| *edge <= hour);
        (position > 0 && position < self.edges.len()).then(|| position - 1)
    }

    const fn len(&self) -> usize {
        self.labels.len()
    }
}

/// What a group of proxies is shown as.
#[derive(Clone)]
struct Group {
    label: String,
    provider: Option<String>,
    proxy_type: Option<String>,
    unknown: bool,
    overflow: bool,
    route: Option<Route>,
}

impl Group {
    fn item(self, key: String, total: Counts) -> UsageItem {
        UsageItem {
            key,
            label: self.label,
            provider: self.provider,
            proxy_type: self.proxy_type,
            unknown: self.unknown,
            overflow: self.overflow,
            route: self.route,
            total,
        }
    }
}

impl Store {
    pub fn query(&mut self, request: UsageQuery, now_hour: i64) -> Result<UsageReport> {
        let buckets = self.buckets(request.range, now_hour)?;
        let (from_hour, to_hour) = (buckets.edges[0], buckets.edges[buckets.len()] - 1);
        let mut series: HashMap<String, Vec<Counts>> = HashMap::new();
        let mut time_anomaly_days = Vec::new();
        let offset = self.meta.day_offset;
        let mut shards = Shards::new();
        for day in day_of(from_hour, offset)..=day_of(to_hour, offset) {
            match self.day_source(day, &mut shards)? {
                DaySource::Hours(hours) => add_hours(&mut series, &buckets, &hours),
                DaySource::Archived(archived) => {
                    if archived.time_anomaly {
                        time_anomaly_days.push(day_name(day));
                    }
                    if buckets.granularity == UsageGranularity::Hour {
                        add_hours(&mut series, &buckets, &archived.late);
                    } else if let Some(index) = buckets.index(day_start(day, offset)) {
                        let mut totals = archived.base;
                        for (id, counts) in day_totals(&archived.late) {
                            add(totals.entry(id).or_insert([0, 0]), counts);
                        }
                        for (id, counts) in totals {
                            add(&mut series_of(&mut series, &id, buckets.len())[index], counts);
                        }
                    }
                }
                DaySource::None => {}
            }
        }
        Ok(self.summarize(request, buckets, series, from_hour, to_hour, time_anomaly_days))
    }

    fn summarize(
        &self,
        request: UsageQuery,
        buckets: Buckets,
        series: HashMap<String, Vec<Counts>>,
        from_hour: i64,
        to_hour: i64,
        time_anomaly_days: Vec<String>,
    ) -> UsageReport {
        let mut grouped: BTreeMap<String, (Group, Vec<Counts>)> = BTreeMap::new();
        for (id, values) in series {
            let record = self.proxies.get(&id);
            if !request.include_builtin && record.is_some_and(|record| record.identity.is_builtin()) {
                continue;
            }
            let route = self.routes.get(&id);
            // the route view leaves out relays, and usage recorded without a route: that of
            // before the core told routes apart, and that whose route was lost
            if request.view == UsageView::Route && route.is_none_or(|route| route.kind == RouteKind::Relay) {
                continue;
            }
            let (key, group) = group_of(request.view, &id, record, route);
            let slot = grouped
                .entry(key)
                .or_insert_with(|| (group, vec![[0, 0]; buckets.len()]));
            for (into, counts) in slot.1.iter_mut().zip(values) {
                add(into, counts);
            }
        }
        let mut ranked = grouped
            .into_iter()
            .map(|(key, (group, values))| {
                let mut total = [0, 0];
                for counts in &values {
                    add(&mut total, *counts);
                }
                (key, group, values, total)
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| (b.3[0] + b.3[1]).cmp(&(a.3[0] + a.3[1])).then_with(|| a.0.cmp(&b.0)));

        let overflow = ranked.iter().any(|(_, group, ..)| group.overflow);
        let all_items = if request.view == UsageView::Route {
            ranked
                .iter()
                .map(|(key, group, _, total)| group.clone().item(key.clone(), *total))
                .collect()
        } else {
            Vec::new()
        };
        let rest = ranked.split_off(ranked.len().min(TOP_ITEMS));
        let mut other_values = vec![[0, 0]; buckets.len()];
        let mut other = [0, 0];
        for (.., values, total) in rest {
            add(&mut other, total);
            for (into, counts) in other_values.iter_mut().zip(values) {
                add(into, counts);
            }
        }
        let points = (0..buckets.len())
            .map(|index| UsagePoint {
                start_hour: buckets.edges[index],
                label: buckets.labels[index].clone(),
                values: ranked
                    .iter()
                    .map(|(_, _, values, _)| values[index])
                    .chain(Some(other_values[index]))
                    .collect(),
            })
            .collect();
        let items = ranked
            .into_iter()
            .map(|(key, group, _, total)| group.item(key, total))
            .collect();
        UsageReport {
            granularity: buckets.granularity,
            from_hour,
            to_hour,
            items,
            other,
            points,
            overflow,
            all_items,
            gaps: self
                .state
                .gaps
                .iter()
                .filter(|gap| gap.to >= from_hour && gap.from <= to_hour)
                .cloned()
                .collect(),
            pending: self.pending().filter(|[from, to]| *to >= from_hour && *from <= to_hour),
            coverage_unknown_before: self.state.coverage_unknown_before.filter(|hour| *hour > from_hour),
            time_anomaly_days,
        }
    }

    /// Hours the core still running may add to and that are not collected yet.
    pub fn pending(&self) -> Option<[i64; 2]> {
        let current = self.state.instance.as_ref().filter(|current| !current.stopped)?;
        (current.done < current.last_hour).then_some([current.done + 1, current.last_hour])
    }

    fn buckets(&mut self, range: UsageRange, now_hour: i64) -> Result<Buckets> {
        let offset = self.meta.day_offset;
        let today = day_of(now_hour, offset);
        Ok(match range {
            UsageRange::Last24Hours => self.hour_buckets(now_hour - 23, now_hour)?,
            UsageRange::Last7Days => self.hour_buckets(now_hour - 7 * 24 + 1, now_hour)?,
            UsageRange::Last30Days => day_buckets(today - 29, today, offset),
            UsageRange::Last12Months => day_buckets(today - 364, today, offset),
            UsageRange::All => {
                let (first, last) = self.history_span(today)?;
                if last - first + 1 > MAX_DAILY_POINTS {
                    month_buckets(first, last, offset)
                } else {
                    day_buckets(first, last, offset)
                }
            }
        })
    }

    /// Hourly buckets, or daily ones when a day in the range has an archived total, which
    /// cannot be split into hours.
    fn hour_buckets(&mut self, from: i64, to: i64) -> Result<Buckets> {
        let offset = self.meta.day_offset;
        let (first, last) = (day_of(from, offset), day_of(to, offset));
        let mut shards = Shards::new();
        for day in first..=last {
            if !self.hour_path(day).exists()
                && self
                    .shard(&mut shards, day)?
                    .get(&day_name(day))
                    .is_some_and(|archived| !archived.base.is_empty())
            {
                return Ok(day_buckets(first, last, offset));
            }
        }
        Ok(hour_buckets(from, to))
    }

    /// First and last statistics day of everything recorded, gaps included, and today: a
    /// clock set back can leave usage on later days.
    fn history_span(&mut self, today: i64) -> Result<(i64, i64)> {
        let offset = self.meta.day_offset;
        let mut days = self.history_days()?;
        days.extend(
            self.state
                .gaps
                .iter()
                .flat_map(|gap| [day_of(gap.from, offset), day_of(gap.to, offset)]),
        );
        if let Some([from, to]) = self.pending() {
            days.extend([day_of(from, offset), day_of(to, offset)]);
        }
        if let Some(before) = self.state.coverage_unknown_before {
            days.push(day_of(before - 1, offset));
        }
        days.push(today);
        let first = days.iter().copied().min().unwrap_or(today);
        let last = days.iter().copied().max().unwrap_or(today);
        Ok((first, last))
    }

    /// A day's only source; each day is read once per operation, so it is taken out of the shard.
    fn day_source(&mut self, day: i64, shards: &mut Shards) -> Result<DaySource> {
        if let Some(hours) = self.read_hours(day)? {
            return Ok(DaySource::Hours(hours));
        }
        Ok(self
            .shard(shards, day)?
            .remove(&day_name(day))
            .map_or(DaySource::None, DaySource::Archived))
    }

    /// The shard of a day's month, read once per operation.
    fn shard<'a>(&mut self, shards: &'a mut Shards, day: i64) -> Result<&'a mut MonthShard> {
        Ok(match shards.entry(month_name(day)) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(self.read_shard(day)?),
        })
    }

    /// Writes every day's only source, the proxy identities, the gaps and what the times mean
    /// to `path`, for reading elsewhere. Days are written one at a time.
    pub fn export(&mut self, path: &Path, exported_at: &str) -> Result<()> {
        let file = std::fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut head = serde_json::to_vec(&ExportHead {
            format: self.meta.format,
            client: &self.meta.client,
            day_offset: self.meta.day_offset,
            created_at: &self.meta.created_at,
            exported_at,
        })?;
        // Leave the object open for the days, the identities and the coverage.
        head.pop();
        writer.write_all(&head)?;
        writer.write_all(b",\"days\":[")?;

        let offset = self.meta.day_offset;
        let mut referenced = BTreeSet::new();
        let mut shards = Shards::new();
        let mut first = true;
        for day in self.history_days()? {
            // Days come in order, so only the current month's shard is kept.
            shards.retain(|month, _| *month == month_name(day));
            let (hours, archived) = match self.day_source(day, &mut shards)? {
                DaySource::Hours(hours) => (Some(hours), None),
                DaySource::Archived(archived) => (None, Some(archived)),
                DaySource::None => continue,
            };
            for proxies in hours
                .iter()
                .chain(archived.as_ref().map(|archived| &archived.late))
                .flat_map(|hours| hours.values())
            {
                referenced.extend(proxies.keys().filter(|id| !self.proxies.contains_key(*id)).cloned());
            }
            if let Some(archived) = &archived {
                referenced.extend(
                    archived
                        .base
                        .keys()
                        .filter(|id| !self.proxies.contains_key(*id))
                        .cloned(),
                );
            }
            if !first {
                writer.write_all(b",")?;
            }
            first = false;
            serde_json::to_writer(
                &mut writer,
                &ExportDay {
                    date: day_name(day),
                    from_hour: day_start(day, offset),
                    to_hour: day_start(day + 1, offset) - 1,
                    hours,
                    archived,
                },
            )?;
        }
        writer.write_all(b"],\"routes\":")?;
        serde_json::to_writer(&mut writer, &self.routes)?;
        writer.write_all(b",\"proxies\":")?;
        let mut proxies: BTreeMap<&str, ExportProxy<'_>> = self
            .proxies
            .iter()
            .map(|(id, record)| (id.as_str(), ExportProxy::Known(record)))
            .collect();
        for id in &referenced {
            proxies.insert(id, ExportProxy::Unknown { unknown: true });
        }
        serde_json::to_writer(&mut writer, &proxies)?;
        // Last, as reading the days may have found unreadable files.
        let tail = serde_json::to_vec(&ExportCoverage {
            gaps: &self.state.gaps,
            pending: self.pending(),
            coverage_unknown_before: self.state.coverage_unknown_before,
        })?;
        writer.write_all(b",")?;
        writer.write_all(&tail[1..])?;
        writer.flush()?;
        Ok(())
    }
}

enum DaySource {
    Hours(HourData),
    Archived(DayArchive),
    None,
}

type Shards = HashMap<String, MonthShard>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportHead<'a> {
    format: u32,
    client: &'a str,
    day_offset: i64,
    created_at: &'a str,
    exported_at: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportCoverage<'a> {
    gaps: &'a [UsageGap],
    pending: Option<[i64; 2]>,
    coverage_unknown_before: Option<i64>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ExportProxy<'a> {
    Known(&'a ProxyRecord),
    Unknown { unknown: bool },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportDay {
    date: String,
    /// First and last UTC hour of the statistics day.
    from_hour: i64,
    to_hour: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    hours: Option<HourData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived: Option<DayArchive>,
}

fn series_of<'a>(series: &'a mut HashMap<String, Vec<Counts>>, id: &str, len: usize) -> &'a mut Vec<Counts> {
    series.entry(id.to_owned()).or_insert_with(|| vec![[0, 0]; len])
}

fn add_hours(series: &mut HashMap<String, Vec<Counts>>, buckets: &Buckets, hours: &HourData) {
    for (hour, proxies) in hours {
        let Some(index) = buckets.index(*hour) else {
            continue;
        };
        for (id, instances) in proxies {
            add(&mut series_of(series, id, buckets.len())[index], hour_total(instances));
        }
    }
}

fn group_of(view: UsageView, id: &str, record: Option<&ProxyRecord>, route: Option<&Route>) -> (String, Group) {
    let Some(record) = record else {
        return (
            format!("id:{id}"),
            Group {
                label: id.to_owned(),
                provider: None,
                proxy_type: None,
                unknown: true,
                overflow: false,
                route: route.filter(|_| view == UsageView::Route).cloned(),
            },
        );
    };
    let identity = &record.identity;
    // the core's entry for what it could not count apart; a proxy of that name has a type
    let overflow = identity.name == OVERFLOW_NAME
        && identity.proxy_type.is_empty()
        && identity.provider.is_empty()
        && identity.addr.is_empty()
        && !identity.incomplete;
    let group = Group {
        label: identity.name.clone(),
        provider: (!identity.provider.is_empty()).then(|| identity.provider.clone()),
        proxy_type: (!identity.proxy_type.is_empty()).then(|| identity.proxy_type.clone()),
        unknown: false,
        overflow,
        route: None,
    };
    if overflow {
        return (
            "overflow".to_owned(),
            Group {
                route: route.filter(|_| view == UsageView::Route).cloned(),
                ..group
            },
        );
    }
    match (view, route) {
        // the same route of a proxy rebuilt with another address is one item
        (UsageView::Route, Some(route)) => (
            format!(
                "route:{}\0{}\0{}",
                identity.provider,
                identity.name,
                serde_json::to_string(&(&route.groups, &route.dialers)).unwrap_or_default()
            ),
            Group {
                route: Some(route.clone()),
                ..group
            },
        ),
        _ => (format!("name:{}\0{}", identity.provider, identity.name), group),
    }
}

const OVERFLOW_NAME: &str = "*overflow*";

fn hour_buckets(from: i64, to: i64) -> Buckets {
    Buckets {
        granularity: UsageGranularity::Hour,
        edges: (from..=to + 1).collect(),
        labels: vec![None; usize::try_from(to - from + 1).unwrap_or_default()],
    }
}

fn day_buckets(from: i64, to: i64, offset: i64) -> Buckets {
    Buckets {
        granularity: UsageGranularity::Day,
        edges: (from..=to + 1).map(|day| day_start(day, offset)).collect(),
        labels: (from..=to).map(|day| Some(day_name(day))).collect(),
    }
}

fn month_buckets(from: i64, to: i64, offset: i64) -> Buckets {
    let mut edges = Vec::new();
    let mut labels = Vec::new();
    let mut day = month_days(from).0;
    while day <= to {
        edges.push(day_start(day, offset));
        labels.push(Some(month_name(day)));
        day = month_days(day).1 + 1;
    }
    edges.push(day_start(day, offset));
    Buckets {
        granularity: UsageGranularity::Month,
        edges,
        labels,
    }
}
