#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    Identity, UsageGapReason, UsageGranularity, UsageQuery, UsageRange, UsageView,
    model::{UsageGap, day_name, day_of, day_start},
    store::{CoreBucket, CoreEntry, CoreUsage, InitParams, Store},
};

/// 2026-11-13 00:00 UTC.
const MIDNIGHT: i64 = 1_794_528_000 / 3600;
const OFFSET: i64 = 8;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "clash-verge-usage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn init() -> InitParams {
    InitParams {
        client: "client-1".into(),
        day_offset: OFFSET,
        created_at: "2026-11-13T00:00:00Z".into(),
    }
}

fn open(dir: &Path, now_hour: i64) -> Store {
    Store::open(dir, init, now_hour).unwrap()
}

fn proxy(name: &str, addr: &str) -> Identity {
    Identity {
        provider: "sub".into(),
        name: name.into(),
        proxy_type: "Vmess".into(),
        addr: addr.into(),
        incomplete: false,
    }
}

/// Proxies with their (hour, up, down) buckets.
type Reported<'a> = [(Identity, &'a [(i64, u64, u64)])];

fn core(instance: &str, started_hour: i64, hour: i64, entries: &Reported<'_>) -> CoreUsage {
    CoreUsage {
        instance: instance.into(),
        started_hour,
        hour,
        settled: hour - 1,
        retained_since: started_hour.max(hour - 744),
        entries: entries
            .iter()
            .map(|(identity, buckets)| CoreEntry {
                identity: identity.clone(),
                buckets: buckets
                    .iter()
                    .map(|&(hour, up, down)| CoreBucket { hour, up, down })
                    .collect(),
            })
            .collect(),
    }
}

/// Collects everything the core reports, in batches, the way the app does.
fn collect(store: &mut Store, core: &CoreUsage) {
    store.observe(core, 0).unwrap();
    while let Some((since, until)) = store.next_range() {
        let mut batch = core.clone();
        for entry in &mut batch.entries {
            entry
                .buckets
                .retain(|bucket| bucket.hour >= since && bucket.hour <= until);
        }
        store.apply(&batch, until, 0).unwrap();
        if until >= core.hour {
            break;
        }
    }
}

fn day_total(store: &mut Store, now_hour: i64, name: &str) -> [u64; 2] {
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::Last30Days,
                view: UsageView::Name,
                include_builtin: true,
            },
            now_hour,
        )
        .unwrap();
    result
        .items
        .iter()
        .find(|item| item.label == name)
        .map_or([0, 0], |item| item.total)
}

#[test]
fn repeated_and_out_of_order_responses_count_once() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let hk = proxy("HK", "hk:443");
    let later = core("a", MIDNIGHT, MIDNIGHT, &[(hk.clone(), &[(MIDNIGHT, 100, 1000)])]);
    let earlier = core("a", MIDNIGHT, MIDNIGHT, &[(hk, &[(MIDNIGHT, 60, 2000)])]);
    collect(&mut store, &later);
    collect(&mut store, &earlier);
    collect(&mut store, &later);
    assert_eq!(day_total(&mut store, MIDNIGHT, "HK"), [100, 2000]);
}

#[test]
fn progress_follows_the_requested_hours_and_settled_hours() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let start = MIDNIGHT - 100;
    let usage = core(
        "a",
        start,
        MIDNIGHT,
        &[(proxy("HK", "hk:443"), &[(start, 1, 1), (MIDNIGHT, 2, 2)])],
    );
    store.observe(&usage, 0).unwrap();
    assert_eq!(store.next_range(), Some((start, start + 47)));
    store.apply(&usage, start + 47, 0).unwrap();
    assert_eq!(store.next_range(), Some((start + 48, start + 95)));

    collect(&mut store, &usage);
    let current = store.state().instance.clone().unwrap();
    assert_eq!(current.done, MIDNIGHT - 1);
    assert_eq!(store.pending(), Some([MIDNIGHT, MIDNIGHT]));
    assert_eq!(day_total(&mut store, MIDNIGHT, "HK"), [3, 3]);
}

#[test]
fn a_core_lost_without_a_final_collection_leaves_a_gap_up_to_its_successor() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT - 5, MIDNIGHT, &[]));
    collect(&mut store, &core("b", MIDNIGHT + 3, MIDNIGHT + 3, &[]));
    assert_eq!(
        store.state().gaps,
        vec![UsageGap {
            from: MIDNIGHT,
            to: MIDNIGHT + 3,
            reason: UsageGapReason::CoreLost,
            time_anomaly: false,
        }]
    );
    assert_eq!(store.state().instance.as_ref().unwrap().done, MIDNIGHT + 2);
}

#[test]
fn a_core_stopped_after_a_final_collection_leaves_no_gap() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT - 5, MIDNIGHT, &[]));
    store.mark_stopped("a").unwrap();
    collect(&mut store, &core("b", MIDNIGHT, MIDNIGHT, &[]));
    assert!(store.state().gaps.is_empty());
}

#[test]
fn a_core_seen_again_after_its_stop_was_recorded_is_not_stopped() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT - 5, MIDNIGHT, &[]));
    store.mark_stopped("a").unwrap();
    collect(&mut store, &core("a", MIDNIGHT - 5, MIDNIGHT + 2, &[]));
    collect(&mut store, &core("b", MIDNIGHT + 4, MIDNIGHT + 4, &[]));
    assert_eq!(store.state().gaps[0].reason, UsageGapReason::CoreLost);
    assert_eq!(
        (store.state().gaps[0].from, store.state().gaps[0].to),
        (MIDNIGHT + 2, MIDNIGHT + 4)
    );
}

#[test]
fn a_successor_started_before_the_lost_core_was_last_seen_keeps_the_gap_and_flags_the_clock() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT - 5, MIDNIGHT, &[]));
    collect(&mut store, &core("b", MIDNIGHT - 48, MIDNIGHT - 48, &[]));
    assert_eq!(
        store.state().gaps,
        vec![UsageGap {
            from: MIDNIGHT,
            to: MIDNIGHT,
            reason: UsageGapReason::CoreLost,
            time_anomaly: true,
        }]
    );
}

#[test]
fn hours_the_core_no_longer_keeps_are_gaps_not_zero() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let start = MIDNIGHT - 2000;
    collect(&mut store, &core("a", start, start + 10, &[]));
    collect(&mut store, &core("a", start, MIDNIGHT, &[]));
    assert_eq!(
        store.state().gaps,
        vec![UsageGap {
            from: start + 10,
            to: MIDNIGHT - 745,
            reason: UsageGapReason::Expired,
            time_anomaly: false,
        }]
    );

    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", start, MIDNIGHT, &[]));
    assert_eq!(
        (
            store.state().gaps[0].from,
            store.state().gaps[0].to,
            store.state().gaps[0].reason
        ),
        (start, MIDNIGHT - 745, UsageGapReason::Expired)
    );
}

#[test]
fn old_days_are_archived_once_and_keep_their_totals() {
    let dir = TempDir::new();
    let now = MIDNIGHT;
    let old = now - 40 * 24;
    let mut store = open(&dir.0, old);
    collect(
        &mut store,
        &core(
            "a",
            old,
            old + 1,
            &[(proxy("HK", "hk:443"), &[(old, 10, 20), (old + 1, 1, 2)])],
        ),
    );
    let old_day = day_of(old, OFFSET);
    assert!(store.hour_path(old_day).exists());

    store.archive(now, false).unwrap();
    store.archive(now, false).unwrap();
    assert!(!store.hour_path(old_day).exists());

    let mut store = open(&dir.0, now);
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::All,
                view: UsageView::Name,
                include_builtin: true,
            },
            now,
        )
        .unwrap();
    assert_eq!(result.granularity, UsageGranularity::Day);
    assert_eq!(result.items[0].total, [11, 22]);
}

#[test]
fn an_interrupted_archive_is_finished_or_undone_by_the_clock() {
    let dir = TempDir::new();
    let now = MIDNIGHT;
    let old = now - 40 * 24;
    let mut store = open(&dir.0, old);
    collect(
        &mut store,
        &core("a", old, old, &[(proxy("HK", "hk:443"), &[(old, 10, 20)])]),
    );
    let old_day = day_of(old, OFFSET);
    // The day total is written but the hour file is still there.
    let hours = std::fs::read(store.hour_path(old_day)).unwrap();
    store.archive(now, false).unwrap();
    std::fs::write(store.hour_path(old_day), &hours).unwrap();

    // The clock went back to that day: the hour file stays the only source.
    let mut store = open(&dir.0, old);
    assert!(store.hour_path(old_day).exists());
    collect(
        &mut store,
        &core("a", old, old, &[(proxy("HK", "hk:443"), &[(old, 10, 25)])]),
    );
    assert_eq!(day_total(&mut store, old, "HK"), [10, 25]);

    // Back to the present, the day is archived with what was collected meanwhile.
    let mut store = open(&dir.0, now);
    store.archive(now, false).unwrap();
    assert!(!store.hour_path(old_day).exists());
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::All,
                view: UsageView::Name,
                include_builtin: true,
            },
            now,
        )
        .unwrap();
    assert_eq!(result.items[0].total, [10, 25]);
}

#[test]
fn usage_for_an_archived_day_is_added_hour_by_hour_and_only_once() {
    let dir = TempDir::new();
    let now = MIDNIGHT;
    let old = day_start(day_of(now - 40 * 24, OFFSET), OFFSET);
    let mut store = open(&dir.0, old);
    collect(
        &mut store,
        &core("a", old, old, &[(proxy("HK", "hk:443"), &[(old, 1000, 0)])]),
    );
    store.archive(now, false).unwrap();

    // The clock is set back to that day and a new core counts there.
    let mut store = open(&dir.0, old + 10);
    let hk = proxy("HK", "hk:443");
    collect(
        &mut store,
        &core("b", old + 10, old + 10, &[(hk.clone(), &[(old + 10, 60, 0)])]),
    );
    collect(
        &mut store,
        &core("b", old + 10, old + 10, &[(hk.clone(), &[(old + 10, 100, 0)])]),
    );
    collect(
        &mut store,
        &core(
            "b",
            old + 10,
            old + 11,
            &[(hk.clone(), &[(old + 10, 100, 0), (old + 11, 80, 0)])],
        ),
    );
    collect(
        &mut store,
        &core(
            "b",
            old + 10,
            old + 11,
            &[(hk, &[(old + 10, 100, 0), (old + 11, 80, 0)])],
        ),
    );
    assert!(!store.hour_path(day_of(old, OFFSET)).exists());

    let result = store
        .query(
            UsageQuery {
                range: UsageRange::All,
                view: UsageView::Name,
                include_builtin: true,
            },
            now,
        )
        .unwrap();
    assert_eq!(result.items[0].total, [1180, 0]);
    assert_eq!(result.time_anomaly_days, vec![day_name(day_of(old, OFFSET))]);
}

#[test]
fn lost_metadata_is_restored_from_its_backup_and_never_reinitialized_over_history() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", MIDNIGHT, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(MIDNIGHT, 1, 1)])]),
    );
    drop(store);

    std::fs::write(dir.0.join("meta.json"), b"{broken").unwrap();
    let store = Store::open(
        &dir.0,
        || InitParams {
            client: "other".into(),
            day_offset: 0,
            created_at: String::new(),
        },
        MIDNIGHT,
    )
    .unwrap();
    assert_eq!(store.meta().client, "client-1");
    assert!(dir.0.join("meta.json.corrupt").exists());
    drop(store);

    std::fs::remove_file(dir.0.join("meta.json")).unwrap();
    std::fs::remove_file(dir.0.join("meta.json.bak")).unwrap();
    assert!(Store::open(&dir.0, init, MIDNIGHT).is_err());
}

#[test]
fn unknown_identities_keep_their_usage() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", MIDNIGHT, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(MIDNIGHT, 5, 6)])]),
    );
    drop(store);
    std::fs::write(dir.0.join("proxies.json"), b"[").unwrap();
    std::fs::remove_file(dir.0.join("proxies.json.bak")).unwrap();

    let mut store = open(&dir.0, MIDNIGHT);
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::Last24Hours,
                view: UsageView::Name,
                include_builtin: false,
            },
            MIDNIGHT,
        )
        .unwrap();
    assert!(result.items[0].unknown);
    assert_eq!(result.items[0].total, [5, 6]);
}

#[test]
fn an_unreadable_hour_file_is_a_gap_and_collected_again() {
    let dir = TempDir::new();
    let start = MIDNIGHT - 30;
    let usage = core("a", start, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(start, 5, 6)])]);
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &usage);
    let day = day_of(start, OFFSET);
    std::fs::write(store.hour_path(day), b"{").unwrap();
    drop(store);

    let mut store = open(&dir.0, MIDNIGHT);
    assert_eq!(store.state().gaps[0].reason, UsageGapReason::Corrupt);
    // Back to the start of the day, but not before the core started.
    assert!(day_start(day, OFFSET) < start);
    assert_eq!(store.state().instance.as_ref().unwrap().done, start - 1);
    collect(&mut store, &usage);
    assert_eq!(day_total(&mut store, MIDNIGHT, "HK"), [5, 6]);
    assert_eq!(store.state().gaps.len(), 1);
}

#[test]
fn a_lost_state_marks_the_history_before_it_unknown() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", MIDNIGHT, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(MIDNIGHT, 1, 1)])]),
    );
    drop(store);
    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    let store = open(&dir.0, MIDNIGHT + 2);
    assert_eq!(store.state().instance.as_ref().unwrap().id, "a");
    drop(store);

    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    std::fs::remove_file(dir.0.join("state.json.bak")).unwrap();
    let store = open(&dir.0, MIDNIGHT + 2);
    // Up to the end of the last day with usage, which is after the current hour.
    assert_eq!(
        store.state().coverage_unknown_before,
        Some(day_start(day_of(MIDNIGHT, OFFSET) + 1, OFFSET))
    );
    assert!(store.state().instance.is_none());
}

#[test]
fn views_group_proxies_and_keep_the_top_ten() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let mut entries = Vec::new();
    for index in 0..12_u64 {
        entries.push((proxy(&format!("n{index}"), &format!("s{}:443", index % 6)), index + 1));
    }
    let direct = Identity {
        provider: String::new(),
        name: "DIRECT".into(),
        proxy_type: "Direct".into(),
        addr: String::new(),
        incomplete: false,
    };
    let buckets = entries
        .iter()
        .map(|(_, bytes)| [(MIDNIGHT, *bytes, 0_u64)])
        .collect::<Vec<_>>();
    let mut reported = entries
        .iter()
        .zip(&buckets)
        .map(|((identity, _), buckets)| (identity.clone(), &buckets[..]))
        .collect::<Vec<_>>();
    let direct_bucket = [(MIDNIGHT, 1000, 0)];
    reported.push((direct, &direct_bucket[..]));
    collect(&mut store, &core("a", MIDNIGHT, MIDNIGHT, &reported));

    let query = |store: &mut Store, view, include_builtin| {
        store
            .query(
                UsageQuery {
                    range: UsageRange::Last24Hours,
                    view,
                    include_builtin,
                },
                MIDNIGHT,
            )
            .unwrap()
    };
    let by_name = query(&mut store, UsageView::Name, false);
    assert_eq!(by_name.granularity, UsageGranularity::Hour);
    assert_eq!(by_name.items.len(), 10);
    assert_eq!(by_name.items[0].label, "n11");
    assert_eq!(by_name.other, [1 + 2, 0]);
    let last = by_name.points.last().unwrap();
    assert_eq!(last.values.len(), 11);
    assert_eq!(last.values[0], [12, 0]);

    let by_server = query(&mut store, UsageView::Server, true);
    assert_eq!(by_server.items[0].label, "DIRECT");
    assert_eq!(by_server.items[1].label, "s5:443");
    assert_eq!(by_server.items[1].total, [6 + 12, 0]);
    assert_eq!(by_server.items.len(), 7);
}

#[test]
fn hours_expiring_between_batches_are_gaps() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let start = MIDNIGHT - 744;
    let probe = core("a", start, MIDNIGHT, &[]);
    store.observe(&probe, 0).unwrap();
    let (since, until) = store.next_range().unwrap();
    assert_eq!(since, start);
    // The core moved on to the next hour and dropped the first one before answering.
    let mut batch = core("a", start, MIDNIGHT + 1, &[]);
    batch.retained_since = start + 1;
    store.apply(&batch, until, 0).unwrap();
    assert_eq!(
        store.state().gaps,
        vec![UsageGap {
            from: start,
            to: start,
            reason: UsageGapReason::Expired,
            time_anomaly: false,
        }]
    );
    assert_eq!(store.state().instance.as_ref().unwrap().done, until);
}

#[test]
fn identities_are_saved_again_after_a_failed_save() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", MIDNIGHT, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(MIDNIGHT, 1, 1)])]),
    );
    let usage = core("a", MIDNIGHT, MIDNIGHT, &[(proxy("JP", "jp:443"), &[(MIDNIGHT, 2, 2)])]);
    store.observe(&usage, 0).unwrap();
    let (_, until) = store.next_range().unwrap();
    // Replacing the dictionary fails while a directory is in its way.
    std::fs::remove_file(dir.0.join("proxies.json")).unwrap();
    std::fs::create_dir(dir.0.join("proxies.json")).unwrap();
    assert!(store.apply(&usage, until, 0).is_err());
    std::fs::remove_dir(dir.0.join("proxies.json")).unwrap();
    store.apply(&usage, until, 0).unwrap();
    drop(store);

    std::fs::remove_file(dir.0.join("proxies.json.bak")).unwrap();
    let mut store = open(&dir.0, MIDNIGHT);
    assert_eq!(day_total(&mut store, MIDNIGHT, "JP"), [2, 2]);
}

#[test]
fn hours_of_an_archived_day_fall_back_to_days() {
    let dir = TempDir::new();
    let now = MIDNIGHT;
    let old = day_start(day_of(now - 40 * 24, OFFSET), OFFSET);
    let mut store = open(&dir.0, old);
    collect(
        &mut store,
        &core("a", old, old, &[(proxy("HK", "hk:443"), &[(old, 1000, 0)])]),
    );
    store.archive(now, false).unwrap();

    // The clock is set back into the archived day.
    let mut store = open(&dir.0, old + 5);
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::Last24Hours,
                view: UsageView::Name,
                include_builtin: true,
            },
            old + 5,
        )
        .unwrap();
    assert_eq!(result.granularity, UsageGranularity::Day);
    assert_eq!(result.items[0].total, [1000, 0]);
}

#[test]
fn all_covers_gaps_before_the_data_and_days_after_today() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    let start = MIDNIGHT - 2000;
    collect(
        &mut store,
        &core("a", start, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(MIDNIGHT, 7, 0)])]),
    );
    // The clock went back two days.
    let earlier = MIDNIGHT - 48;
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::All,
                view: UsageView::Name,
                include_builtin: true,
            },
            earlier,
        )
        .unwrap();
    assert_eq!(result.items[0].total, [7, 0]);
    assert!(result.from_hour <= start);
    assert!(result.gaps.iter().any(|gap| gap.reason == UsageGapReason::Expired));
}

#[test]
fn an_unreadable_file_found_by_a_query_is_recorded_before_it_is_set_aside() {
    let dir = TempDir::new();
    let start = MIDNIGHT - 30;
    let usage = core("a", start, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(start, 5, 6)])]);
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &usage);
    let day = day_of(start, OFFSET);
    std::fs::write(store.hour_path(day), b"{").unwrap();
    day_total(&mut store, MIDNIGHT, "HK");
    drop(store);

    let mut store = open(&dir.0, MIDNIGHT);
    assert_eq!(store.state().gaps[0].reason, UsageGapReason::Corrupt);
    assert_eq!(store.state().instance.as_ref().unwrap().done, start - 1);
    collect(&mut store, &usage);
    assert_eq!(day_total(&mut store, MIDNIGHT, "HK"), [5, 6]);
}

#[test]
fn openvpn_proxies_share_their_server_even_when_saved_before() {
    let dir = TempDir::new();
    let vpn = |name: &str| Identity {
        provider: String::new(),
        name: name.into(),
        proxy_type: "OpenVPN".into(),
        addr: "vpn.example.com:1194".into(),
        incomplete: false,
    };
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core(
            "a",
            MIDNIGHT,
            MIDNIGHT,
            &[(vpn("a"), &[(MIDNIGHT, 10, 0)]), (vpn("b"), &[(MIDNIGHT, 20, 0)])],
        ),
    );
    drop(store);
    // As saved by a version that did not know OpenVPN has one server.
    let path = dir.0.join("proxies.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let stale = text.replace(r#""server":{"host":"vpn.example.com","port":1194}"#, r#""server":null"#);
    assert_ne!(text, stale);
    std::fs::write(&path, stale).unwrap();

    let mut store = open(&dir.0, MIDNIGHT);
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::Last24Hours,
                view: UsageView::Server,
                include_builtin: true,
            },
            MIDNIGHT,
        )
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].total, [30, 0]);
}

#[test]
fn unknown_coverage_survives_an_unreadable_file_found_while_recovering() {
    let dir = TempDir::new();
    let start = MIDNIGHT - 30;
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", start, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(start, 5, 6)])]),
    );
    let day = day_of(start, OFFSET);
    std::fs::write(store.hour_path(day), b"{").unwrap();
    drop(store);
    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    std::fs::remove_file(dir.0.join("state.json.bak")).unwrap();

    let first = open(&dir.0, MIDNIGHT).state().coverage_unknown_before;
    assert!(first.is_some_and(|before| before > MIDNIGHT));
    assert_eq!(open(&dir.0, MIDNIGHT).state().coverage_unknown_before, first);
}

#[test]
fn all_keeps_a_later_pending_range() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT, MIDNIGHT, &[]));
    let result = store
        .query(
            UsageQuery {
                range: UsageRange::All,
                view: UsageView::Name,
                include_builtin: true,
            },
            MIDNIGHT - 48,
        )
        .unwrap();
    assert!(result.to_hour >= MIDNIGHT);
    assert_eq!(result.pending, Some([MIDNIGHT, MIDNIGHT]));
}

#[test]
fn export_writes_days_identities_and_coverage() {
    let dir = TempDir::new();
    let now = MIDNIGHT;
    let old = day_start(day_of(now - 40 * 24, OFFSET), OFFSET);
    let mut store = open(&dir.0, old);
    collect(
        &mut store,
        &core("a", old, old, &[(proxy("HK", "hk:443"), &[(old, 1000, 0)])]),
    );
    store.archive(now, false).unwrap();
    let mut store = open(&dir.0, now);
    collect(
        &mut store,
        &core("b", now, now, &[(proxy("JP", "jp:443"), &[(now, 7, 8)])]),
    );
    let jp = proxy("JP", "jp:443").id();
    // JP's identity is lost.
    store.proxies.remove(&jp);

    let file = dir.0.join("export.json");
    store.export(&file, "now").unwrap();
    let export: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(export["client"], "client-1");
    assert_eq!(export["dayOffset"], OFFSET);
    let days = export["days"].as_array().unwrap();
    assert_eq!(days.len(), 2);
    assert_eq!(days[0]["archived"]["base"][proxy("HK", "hk:443").id()][0], 1000);
    assert_eq!(days[1]["hours"][now.to_string()][&jp]["b"][1], 8);
    assert_eq!(export["proxies"][&jp]["unknown"], true);
    assert_eq!(export["proxies"][proxy("HK", "hk:443").id()]["name"], "HK");
    assert!(export["gaps"].is_array());
}

#[test]
fn an_export_includes_the_gaps_its_own_reading_found() {
    let dir = TempDir::new();
    let start = MIDNIGHT - 30;
    let mut store = open(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("a", start, MIDNIGHT, &[(proxy("HK", "hk:443"), &[(start, 5, 6)])]),
    );
    std::fs::write(store.hour_path(day_of(start, OFFSET)), b"{").unwrap();

    let file = dir.0.join("export.json");
    store.export(&file, "now").unwrap();
    let export: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(export["gaps"][0]["reason"], "corrupt");
    assert_eq!(export["pending"][0], start);
}

#[test]
fn a_stop_mark_is_saved_again_after_a_failed_save() {
    let dir = TempDir::new();
    let mut store = open(&dir.0, MIDNIGHT);
    collect(&mut store, &core("a", MIDNIGHT, MIDNIGHT, &[]));
    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    std::fs::create_dir(dir.0.join("state.json")).unwrap();
    assert!(store.mark_stopped("a").is_err());
    std::fs::remove_dir(dir.0.join("state.json")).unwrap();
    store.mark_stopped("a").unwrap();
    drop(store);

    std::fs::remove_file(dir.0.join("state.json.bak")).unwrap();
    let mut store = open(&dir.0, MIDNIGHT);
    assert!(store.state().instance.as_ref().unwrap().stopped);
    collect(&mut store, &core("b", MIDNIGHT, MIDNIGHT, &[]));
    assert!(store.state().gaps.is_empty());
}
