use std::collections::BTreeMap;

use chrono::{Datelike as _, NaiveDate, TimeDelta};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// 2 added the routes of what the core counted.
pub const FORMAT: u32 = 2;

/// Upload and download bytes.
pub type Counts = [u64; 2];
/// Hour → proxy id → core instance → counts, merged by taking the larger value per direction.
pub type HourData = BTreeMap<i64, BTreeMap<String, BTreeMap<String, Counts>>>;

/// A proxy as the core identified it when its connections were made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub provider: String,
    pub name: String,
    #[serde(rename = "type")]
    pub proxy_type: String,
    pub addr: String,
    pub incomplete: bool,
}

impl Identity {
    /// The id of the usage recorded for the proxy before the core told routes apart.
    pub fn id(&self) -> String {
        hex_id(self.hasher())
    }

    /// The id of what the core counted for the proxy: with a route, the proxy's traffic of
    /// that kind and route; without one, all its traffic, as recorded before the core told
    /// routes apart. Each part is hashed with its length, so no two routes share an id, and
    /// neither does a route with the usage recorded before.
    pub fn id_with(&self, route: Option<&Route>) -> String {
        let Some(route) = route else {
            return self.id();
        };
        let mut hasher = self.hasher();
        hasher.update([match route.kind {
            RouteKind::Exit => 1,
            RouteKind::Relay => 2,
        }]);
        hash_names(&mut hasher, &route.groups);
        hasher.update((route.dialers.len() as u64).to_le_bytes());
        for layer in &route.dialers {
            hash_names(&mut hasher, layer);
        }
        hex_id(hasher)
    }

    fn hasher(&self) -> Sha256 {
        let mut hasher = Sha256::new();
        for part in [&self.provider, &self.name, &self.proxy_type, &self.addr] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hasher.update([u8::from(self.incomplete)]);
        hasher
    }

    /// One of the outbounds a config makes for the names it reserves. A config cannot name a
    /// proxy of its own like them; a provider's proxy of the same name has a provider.
    pub fn is_builtin(&self) -> bool {
        self.provider.is_empty() && BUILTIN_NAMES.contains(&self.name.as_str())
    }
}

fn hash_names(hasher: &mut Sha256, names: &[String]) {
    hasher.update((names.len() as u64).to_le_bytes());
    for name in names {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
    }
}

fn hex_id(hasher: Sha256) -> String {
    hasher.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const BUILTIN_NAMES: &[&str] = &["DIRECT", "REJECT", "REJECT-DROP", "PASS", "PASS-RULE", "COMPATIBLE"];

/// What the traffic the core counted for a proxy was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    /// Connections of its own, along the route.
    Exit,
    /// Connections it made to carry another proxy's.
    Relay,
}

/// The kind and route of what the core counted for a proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct Route {
    pub kind: RouteKind,
    /// The groups from the proxy out to the one a rule chose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Layer by layer, the dialer proxies the proxy reached its server through, each as the
    /// node and then its groups; an unknown node is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialers: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRecord {
    #[serde(flatten)]
    pub identity: Identity,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub format: u32,
    /// Identifies this client's data wherever it is exported or reported to.
    pub client: String,
    /// Hours added to UTC to find the statistics day an hour belongs to; fixed once chosen.
    pub day_offset: i64,
    pub created_at: String,
}

/// An archived statistics day.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DayArchive {
    /// Proxy id → day total, fixed once archived.
    pub base: BTreeMap<String, Counts>,
    /// Hours that arrived after the day was archived, which only a clock set back can cause.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub late: HourData,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub time_anomaly: bool,
}

/// Archived days of one month, by date.
pub type MonthShard = BTreeMap<String, DayArchive>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum UsageGapReason {
    /// The core ended without a final collection, so its last traffic is unknown.
    CoreLost,
    /// The core no longer kept the hours when they were collected.
    Expired,
    /// A data file could not be read.
    Corrupt,
}

/// UTC hours, both ends inclusive, whose usage is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct UsageGap {
    pub from: i64,
    pub to: i64,
    pub reason: UsageGapReason,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub time_anomaly: bool,
}

/// Collection progress of the core process last seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceState {
    pub id: String,
    pub started_hour: i64,
    /// Every hour up to this one is collected and on disk, or recorded as a gap.
    pub done: i64,
    /// The core's current hour when it was last seen.
    pub last_hour: i64,
    /// Unix seconds of the last successful collection.
    pub last_collected: i64,
    /// Set once the app stopped or replaced the core after a final collection.
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub format: u32,
    pub instance: Option<InstanceState>,
    pub gaps: Vec<UsageGap>,
    /// Hours before this one may have gaps that were lost with the state file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_unknown_before: Option<i64>,
}

/// Statistics day an hour belongs to, as days since 1970-01-01.
pub const fn day_of(hour: i64, day_offset: i64) -> i64 {
    (hour + day_offset).div_euclid(24)
}

/// First UTC hour of a statistics day.
pub const fn day_start(day: i64, day_offset: i64) -> i64 {
    day * 24 - day_offset
}

pub fn day_date(day: i64) -> NaiveDate {
    NaiveDate::default()
        .checked_add_signed(TimeDelta::days(day))
        .unwrap_or_default()
}

pub fn day_name(day: i64) -> String {
    day_date(day).format("%Y-%m-%d").to_string()
}

pub fn parse_day_name(name: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()?;
    Some(date.signed_duration_since(NaiveDate::default()).num_days())
}

pub fn month_name(day: i64) -> String {
    day_date(day).format("%Y-%m").to_string()
}

/// Days of the month a day is in, first and last.
pub fn month_days(day: i64) -> (i64, i64) {
    let date = day_date(day);
    let first = date.with_day(1).unwrap_or(date);
    let next = first.checked_add_months(chrono::Months::new(1)).unwrap_or(first);
    let base = NaiveDate::default();
    (
        first.signed_duration_since(base).num_days(),
        next.signed_duration_since(base).num_days() - 1,
    )
}

/// Merges counts by taking the larger value per direction, so a response read earlier cannot
/// lower a later one, and reading the same hour twice does not count it twice.
pub fn merge_max(into: &mut Counts, from: Counts) {
    *into = [into[0].max(from[0]), into[1].max(from[1])];
}

pub const fn add(into: &mut Counts, from: Counts) {
    into[0] = into[0].saturating_add(from[0]);
    into[1] = into[1].saturating_add(from[1]);
}

pub fn hour_total(instances: &BTreeMap<String, Counts>) -> Counts {
    let mut total = [0, 0];
    for counts in instances.values() {
        add(&mut total, *counts);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::{Identity, Route, RouteKind, day_name, day_of, day_start, month_days, parse_day_name};

    fn identity(proxy_type: &str, addr: &str) -> Identity {
        Identity {
            provider: String::new(),
            name: "n".into(),
            proxy_type: proxy_type.into(),
            addr: addr.into(),
            incomplete: false,
        }
    }

    #[test]
    fn only_the_reserved_names_are_builtin() {
        let named = |provider: &str, name: &str, proxy_type: &str| Identity {
            provider: provider.into(),
            name: name.into(),
            proxy_type: proxy_type.into(),
            addr: String::new(),
            incomplete: false,
        };
        assert!(named("", "DIRECT", "Direct").is_builtin());
        assert!(named("", "REJECT-DROP", "RejectDrop").is_builtin());
        assert!(!named("", "LAN", "Direct").is_builtin());
        assert!(!named("sub", "DIRECT", "Vless").is_builtin());
    }

    #[test]
    fn routes_have_ids_of_their_own() {
        let a = identity("Vless", "a:1");
        let route = |kind, groups: &[&str], dialers: &[&[&str]]| Route {
            kind,
            groups: groups.iter().map(|name| (*name).into()).collect(),
            dialers: dialers
                .iter()
                .map(|layer| layer.iter().map(|name| (*name).into()).collect())
                .collect(),
        };
        let exit = route(RouteKind::Exit, &[], &[]);
        assert_eq!(a.id_with(None), a.id());
        assert_ne!(
            a.id_with(Some(&exit)),
            a.id(),
            "an exit without a route is not the usage of before"
        );
        assert_ne!(
            a.id_with(Some(&exit)),
            a.id_with(Some(&route(RouteKind::Relay, &[], &[])))
        );
        assert_ne!(
            a.id_with(Some(&route(RouteKind::Exit, &["G", "H"], &[]))),
            a.id_with(Some(&route(RouteKind::Exit, &["GH"], &[])))
        );
        assert_ne!(
            a.id_with(Some(&route(RouteKind::Exit, &[], &[&["A", "G"]]))),
            a.id_with(Some(&route(RouteKind::Exit, &[], &[&["A"], &["G"]])))
        );
        assert_ne!(
            a.id_with(Some(&route(RouteKind::Exit, &["G"], &[]))),
            a.id_with(Some(&route(RouteKind::Exit, &[], &[&["G"]])))
        );
    }

    #[test]
    fn ids_tell_identities_apart() {
        let a = identity("Vmess", "a:1");
        let mut b = a.clone();
        b.provider = "sub".into();
        assert_eq!(a.id().len(), 16);
        assert_ne!(a.id(), b.id());
        assert_eq!(a.id(), identity("Vmess", "a:1").id());
    }

    #[test]
    fn statistics_days_are_24_utc_hours_shifted_by_the_offset() {
        // 2026-11-13 00:00 UTC
        let midnight = 1_794_528_000 / 3600;
        assert_eq!(day_name(day_of(midnight, 0)), "2026-11-13");
        assert_eq!(day_name(day_of(midnight - 1, 8)), "2026-11-13");
        assert_eq!(day_name(day_of(midnight - 9, 8)), "2026-11-12");
        assert_eq!(day_start(day_of(midnight, 8), 8), midnight - 8);
        assert_eq!(day_name(day_of(midnight + 4, -5)), "2026-11-12");
        assert_eq!(parse_day_name("2026-11-13"), Some(day_of(midnight, 0)));
        let (first, last) = month_days(day_of(midnight, 0));
        assert_eq!(day_name(first), "2026-11-01");
        assert_eq!(day_name(last), "2026-11-30");
    }
}
