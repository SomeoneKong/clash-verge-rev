#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    Collected, ConnDimension, ConnFilter, ConnKey, ConnQuery, ConnReport, ConnStore, CoreConnBucket, CoreConnEntry,
    CoreConns, Counting, Coverage, Recorded, UsageGapReason,
    model::{day_name, day_of},
};

/// 2026-11-13 00:00 UTC.
const MIDNIGHT: i64 = 1_794_528_000 / 3600;
const OFFSET: i64 = 8;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "clash-verge-conns-{}-{}",
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

const fn secs(hour: i64) -> i64 {
    hour * 3600 + 60
}

fn open(dir: &Path, hour: i64) -> ConnStore {
    ConnStore::open(dir, OFFSET, hour).unwrap()
}

/// A store recording since `hour`.
fn recording(dir: &Path, hour: i64) -> ConnStore {
    let mut store = open(dir, hour);
    store.set_recording(true, hour * 3600).unwrap();
    store
}

fn key(target: &str) -> ConnKey {
    ConnKey {
        in_type: "Tun".into(),
        process: "chrome.exe".into(),
        target: target.into(),
        network: "tcp".into(),
        rule: "Match".into(),
        chain: vec!["JP 01".into(), "Proxy".into()],
        ..ConnKey::default()
    }
}

/// A target and its buckets as `(hour, down, conns)`.
type Reported<'a> = (&'a str, &'a [(i64, u64, u64)]);

/// What core `instance` reports at `hour`, counting throughout from its start.
fn core(instance: &str, started: i64, hour: i64, entries: &[Reported<'_>]) -> CoreConns {
    CoreConns {
        instance: instance.into(),
        started_hour: started,
        hour,
        settled: hour - 1,
        retained_since: started.max(hour - 72),
        enabled: true,
        tick: hour * 3600,
        counting_on: vec![(started, hour)],
        counting_partial: Vec::new(),
        entries: entries
            .iter()
            .map(|(target, buckets)| CoreConnEntry {
                key: key(target),
                buckets: buckets
                    .iter()
                    .map(|(hour, down, conns)| CoreConnBucket {
                        hour: *hour,
                        up: 1,
                        down: *down,
                        conns: *conns,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Reads what the core reports the way a collection does: the settled hours in full, batch
/// by batch, then the hours it may still add to.
fn collect(store: &mut ConnStore, report: &CoreConns, now: i64) {
    store.observe(report, now).unwrap();
    while let Some((since, until)) = store.settled_range(report.settled) {
        store
            .apply_settled(&clipped(report, since, until), since, until, now)
            .unwrap();
    }
    if let Some((since, until, _)) = store.pending_request(report.settled, report.hour) {
        store
            .apply_pending(&clipped(report, since, until), since, until, now)
            .unwrap();
    }
}

/// The report as the core answers a request for `since` to `until`.
fn clipped(report: &CoreConns, since: i64, until: i64) -> CoreConns {
    let mut clipped = report.clone();
    for entry in &mut clipped.entries {
        entry.buckets.retain(|bucket| (since..=until).contains(&bucket.hour));
    }
    clipped.entries.retain(|entry| !entry.buckets.is_empty());
    clipped
}

fn by_target(store: &mut ConnStore, from: i64, to: i64) -> ConnReport {
    store
        .query(
            &ConnQuery {
                from_hour: from,
                to_hour: to,
                group_by: vec![ConnDimension::Target],
                filters: Vec::new(),
            },
            secs(to),
        )
        .unwrap()
}

fn downloads(report: &ConnReport) -> Vec<(String, u64)> {
    let mut list: Vec<(String, u64)> = report
        .groups
        .iter()
        .map(|group| (group.values[0].clone(), group.down))
        .collect();
    list.sort();
    list
}

#[test]
fn backfill_reads_every_batch_in_full() {
    let dir = TempDir::new();
    let start = MIDNIGHT;
    let now = start + 60;
    let mut store = recording(&dir.0, start);
    // each batch has a key only it reports, long closed
    let report = core(
        "core-a",
        start,
        now,
        &[
            ("first.example", &[(start + 1, 10, 1)]),
            ("second.example", &[(start + 30, 20, 2)]),
            ("third.example", &[(start + 55, 30, 3)]),
            ("current.example", &[(now, 5, 1)]),
        ],
    );
    collect(&mut store, &report, secs(now));
    assert_eq!(store.state().instance.as_ref().unwrap().done, now - 1);

    let mut store = open(&dir.0, now);
    let report = by_target(&mut store, now - 100, now);
    assert_eq!(
        downloads(&by_target(&mut store, start, start + 59)),
        vec![
            ("first.example".into(), 10),
            ("second.example".into(), 20),
            ("third.example".into(), 30)
        ]
    );
    assert!(
        report.groups.iter().all(|group| group.values[0] != "current.example"),
        "a reopened store reads the pending hour again from the core"
    );
}

#[test]
fn pending_hours_go_on_from_the_tick_and_settle_once() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    let first = core("core-a", hour, hour, &[("a.example", &[(hour, 5, 1)])]);
    collect(&mut store, &first, secs(hour));
    let (since, until, touched) = store.pending_request(first.settled, hour).unwrap();
    assert_eq!((since, until, touched), (hour, hour, Some(first.tick - 1)));

    // an incremental read returns only what changed
    let mut next = core("core-a", hour, hour, &[("b.example", &[(hour, 7, 2)])]);
    next.tick += 60;
    store.apply_pending(&next, since, until, secs(hour) + 60).unwrap();
    assert_eq!(
        downloads(&by_target(&mut store, hour, hour)),
        vec![("a.example".into(), 5), ("b.example".into(), 7)]
    );
    let coverage = &by_target(&mut store, hour, hour).hours[0];
    assert_eq!(coverage.coverage, Coverage::Pending);

    // the hour settles: read in full, appended once, no longer pending
    let settled = core(
        "core-a",
        hour,
        hour + 1,
        &[("a.example", &[(hour, 6, 1)]), ("b.example", &[(hour, 7, 2)])],
    );
    collect(&mut store, &settled, secs(hour + 1));
    let report = by_target(&mut store, hour, hour);
    assert_eq!(
        downloads(&report),
        vec![("a.example".into(), 6), ("b.example".into(), 7)]
    );
    assert_eq!(report.hours[0].coverage, Coverage::Complete);
    assert!(!store.pending.contains_key(&hour));
}

#[test]
fn a_core_read_in_full_before_its_stop_ends_without_a_gap() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    let report = core("core-a", hour, hour + 1, &[("a.example", &[(hour + 1, 9, 1)])]);
    collect(&mut store, &report, secs(hour + 1));
    store.read_in_full("core-a", 1);
    store.mark_stopped("core-a", 1, secs(hour + 1)).unwrap();

    collect(&mut store, &core("core-b", hour + 1, hour + 2, &[]), secs(hour + 2));
    assert!(store.state().gaps.is_empty());
    assert_eq!(
        downloads(&by_target(&mut store, hour + 1, hour + 1)),
        vec![("a.example".into(), 9)]
    );
}

#[test]
fn a_core_lost_without_a_final_read_leaves_a_gap_and_what_was_taken() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 1, &[("a.example", &[(hour + 1, 9, 1)])]),
        secs(hour + 1),
    );
    // a final read that ran out of time is not vouched for: the stop is not noted
    collect(&mut store, &core("core-b", hour + 3, hour + 3, &[]), secs(hour + 3));

    let gap = &store.state().gaps[0];
    assert_eq!(
        (gap.from, gap.to, gap.reason),
        (hour + 1, hour + 3, UsageGapReason::CoreLost)
    );
    let report = by_target(&mut store, hour + 1, hour + 1);
    assert_eq!(downloads(&report), vec![("a.example".into(), 9)]);
    assert_eq!(report.hours[0].coverage, Coverage::Gap);
    assert_eq!(report.hours[0].cores[0].collected, Collected::Gap);
}

#[test]
fn turning_recording_off_finishes_and_turning_it_on_goes_on() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 1, &[("a.example", &[(hour + 1, 50, 1)])]),
        secs(hour + 1),
    );
    store.set_recording(false, secs(hour + 1)).unwrap();
    assert!(store.finishing() && store.collecting());

    // the core no longer counts: read in full, then nothing more is owed
    let mut stopped = core("core-a", hour, hour + 1, &[("a.example", &[(hour + 1, 100, 1)])]);
    stopped.enabled = false;
    collect(&mut store, &stopped, secs(hour + 1) + 30);
    store.finish("core-a", secs(hour + 1) + 30).unwrap();
    assert!(!store.collecting());
    assert_eq!(store.state().instance.as_ref().unwrap().done, hour + 1);

    // on again in the same hour: the hour goes on, counted once
    store.set_recording(true, secs(hour + 1) + 60).unwrap();
    collect(
        &mut store,
        &core("core-a", hour, hour + 1, &[("a.example", &[(hour + 1, 130, 1)])]),
        secs(hour + 1) + 90,
    );
    assert_eq!(
        downloads(&by_target(&mut store, hour + 1, hour + 1)),
        vec![("a.example".into(), 130)]
    );
    assert_eq!(
        by_target(&mut store, hour + 1, hour + 1).hours[0].recorded,
        Recorded::Part
    );
}

#[test]
fn turning_recording_on_while_finishing_keeps_the_progress() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(&mut store, &core("core-a", hour, hour + 1, &[]), secs(hour + 1));
    store.set_recording(false, secs(hour + 1)).unwrap();
    store.set_recording(true, secs(hour + 3)).unwrap();
    assert_eq!(store.state().instance.as_ref().unwrap().done, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 3, &[("late.example", &[(hour + 1, 4, 1)])]),
        secs(hour + 3),
    );
    assert_eq!(
        downloads(&by_target(&mut store, hour + 1, hour + 1)),
        vec![("late.example".into(), 4)]
    );
}

#[test]
fn torn_and_damaged_day_files() {
    let dir = TempDir::new();
    let hour = MIDNIGHT + 10;
    let mut store = recording(&dir.0, MIDNIGHT);
    collect(
        &mut store,
        &core("core-a", MIDNIGHT, hour, &[("a.example", &[(MIDNIGHT + 1, 3, 1)])]),
        secs(hour),
    );
    let path = dir
        .0
        .join("hours")
        .join(format!("{}.jsonl", day_name(day_of(MIDNIGHT + 1, OFFSET))));

    // a crash in the middle of an append leaves a torn last line
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"hour\":");
    std::fs::write(&path, &bytes).unwrap();
    let mut store = open(&dir.0, hour);
    assert!(store.state().gaps.is_empty());
    assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
    assert_eq!(
        downloads(&by_target(&mut store, MIDNIGHT, hour)),
        vec![("a.example".into(), 3)]
    );

    // a damaged line in the middle makes the day a gap, read again from the core
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("garbage\n{text}")).unwrap();
    let store = open(&dir.0, hour);
    assert_eq!(store.state().gaps[0].reason, UsageGapReason::Corrupt);
    assert!(store.state().instance.as_ref().unwrap().done < MIDNIGHT);
    assert!(path.with_extension("jsonl.corrupt").exists());
}

#[test]
fn pending_hours_survive_a_restart() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour, &[("a.example", &[(hour, 5, 1)])]),
        secs(hour),
    );
    store.flush(secs(hour)).unwrap();
    drop(store);
    let mut store = open(&dir.0, hour);
    assert_eq!(
        downloads(&by_target(&mut store, hour, hour)),
        vec![("a.example".into(), 5)]
    );
}

#[test]
fn cores_of_an_hour_add_up_and_the_cache_follows_the_disk() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 1, &[("a.example", &[(hour, 100, 1)])]),
        secs(hour + 1),
    );
    store.read_in_full("core-a", 1);
    store.mark_stopped("core-a", 1, secs(hour + 1)).unwrap();
    collect(
        &mut store,
        &core("core-b", hour, hour + 2, &[("a.example", &[(hour, 200, 1)])]),
        secs(hour + 2),
    );
    assert_eq!(
        downloads(&by_target(&mut store, hour, hour)),
        vec![("a.example".into(), 300)]
    );

    // the cache is kept; the same core reported again counts once
    let again = core("core-b", hour, hour + 3, &[("a.example", &[(hour, 250, 1)])]);
    store.apply_settled(&again, hour, hour, secs(hour + 3)).unwrap();
    assert_eq!(
        downloads(&by_target(&mut store, hour, hour)),
        vec![("a.example".into(), 300)]
    );
    assert_eq!(by_target(&mut store, hour, hour).total.conns, 2);

    store.clear(secs(hour + 3)).unwrap();
    assert!(by_target(&mut store, hour, hour).groups.is_empty());
}

#[test]
fn queries_filter_group_and_tell_coverage() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour + 1);
    let mut report = core(
        "core-a",
        hour,
        hour + 3,
        &[("a.example", &[(hour + 1, 1, 1)]), ("b.example", &[(hour + 2, 2, 1)])],
    );
    report.counting_on = vec![(hour, hour + 1)];
    report.counting_partial = vec![(hour + 2, hour + 3)];
    collect(&mut store, &report, secs(hour + 3));

    let filtered = store
        .query(
            &ConnQuery {
                from_hour: hour,
                to_hour: hour + 3,
                group_by: vec![ConnDimension::Process, ConnDimension::Chain],
                filters: vec![ConnFilter {
                    dimension: ConnDimension::Target,
                    value: "b.example".into(),
                }],
            },
            secs(hour + 3),
        )
        .unwrap();
    assert_eq!(filtered.groups.len(), 1);
    assert_eq!(filtered.groups[0].values, vec!["chrome.exe", "Proxy → JP 01"]);
    assert_eq!(filtered.total.down, 2);
    let coverage: Vec<Coverage> = filtered.hours.iter().map(|hour| hour.coverage).collect();
    assert_eq!(
        coverage,
        vec![
            Coverage::NotRecorded,
            Coverage::Complete,
            Coverage::PartlyCounted,
            Coverage::Pending
        ]
    );
    assert_eq!(filtered.hours[2].cores[0].counting, Some(Counting::Partial));
    assert!(
        store
            .query(
                &ConnQuery {
                    from_hour: hour,
                    to_hour: hour + 7 * 24,
                    group_by: vec![],
                    filters: vec![]
                },
                0
            )
            .is_err()
    );
}

#[test]
fn old_days_go_and_so_do_the_keys_only_they_had() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 2, &[("old.example", &[(hour, 1, 1)])]),
        secs(hour + 2),
    );
    let later = hour + 33 * 24;
    collect(
        &mut store,
        &core("core-b", later, later + 1, &[("new.example", &[(later, 1, 1)])]),
        secs(later + 1),
    );
    store.retain(later + 1).unwrap();
    assert_eq!(store.days().unwrap(), vec![day_of(later, OFFSET)]);
    assert!(store.keys.values().all(|key| key.target == "new.example"));
    let keys = std::fs::read_to_string(dir.0.join("keys.jsonl")).unwrap();
    assert!(!keys.contains("old.example"));
}

#[test]
fn recording_is_on_until_turned_off() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    assert!(crate::recording_on_disk(&dir.0), "a store never opened");
    let mut store = open(&dir.0, hour);
    assert!(store.recording());
    assert_eq!(store.state().recording[0].from, hour * 3600);
    assert!(crate::recording_on_disk(&dir.0));
    store.set_recording(false, secs(hour)).unwrap();
    drop(store);
    assert!(!crate::recording_on_disk(&dir.0));
    assert!(!open(&dir.0, hour + 1).recording(), "turned off, it stays off");
}

#[test]
fn a_state_that_never_recorded_starts_recording() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    std::fs::create_dir_all(&dir.0).unwrap();
    let never = r#"{"format":1,"recording":[],"instance":null,"gaps":[]}"#;
    std::fs::write(dir.0.join("state.json"), never).unwrap();
    std::fs::write(dir.0.join("state.json.bak"), never).unwrap();
    assert!(crate::recording_on_disk(&dir.0));
    assert!(open(&dir.0, hour).recording());
}

#[test]
fn a_lost_state_turns_recording_off_and_coverage_unknown() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 2, &[("a.example", &[(hour, 1, 1)])]),
        secs(hour + 2),
    );
    drop(store);
    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    std::fs::remove_file(dir.0.join("state.json.bak")).unwrap();
    let mut store = open(&dir.0, hour + 2);
    assert!(!store.recording());
    assert!(store.state().setting_lost);
    let report = by_target(&mut store, hour, hour);
    assert_eq!(report.hours[0].coverage, Coverage::Unknown);
    assert_eq!(downloads(&report), vec![("a.example".into(), 1)]);
    assert!(!crate::recording_on_disk(&dir.0));
}

#[test]
fn export_holds_keys_hours_current_and_coverage() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core(
            "core-a",
            hour,
            hour + 1,
            &[("a.example", &[(hour, 1, 1)]), ("b.example", &[(hour + 1, 2, 1)])],
        ),
        secs(hour + 1),
    );
    let path = dir.0.join("export.connections.json");
    store
        .export(&path, "client-1", "2026-11-13T02:00:00Z", hour + 1)
        .unwrap();
    let exported: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(exported["client"], "client-1");
    assert_eq!(exported["hours"].as_array().unwrap().len(), 1);
    assert_eq!(exported["current"]["instance"], "core-a");
    assert_eq!(exported["keys"].as_object().unwrap().len(), 2);
    assert_eq!(exported["coverage"]["pending"], serde_json::json!([hour + 1, hour + 1]));
    assert_eq!(exported["coverage"]["recording"][0]["to"], serde_json::Value::Null);
}

#[test]
fn a_store_whose_state_was_lost_reads_the_next_core_back_in_full() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(&mut store, &core("core-a", hour, hour + 1, &[]), secs(hour + 1));
    drop(store);
    std::fs::remove_file(dir.0.join("state.json")).unwrap();
    std::fs::remove_file(dir.0.join("state.json.bak")).unwrap();
    let mut store = open(&dir.0, hour + 5);
    store.set_recording(true, secs(hour + 5)).unwrap();
    collect(
        &mut store,
        &core("core-a", hour, hour + 5, &[("kept.example", &[(hour + 2, 8, 1)])]),
        secs(hour + 5),
    );
    assert_eq!(
        downloads(&by_target(&mut store, hour + 2, hour + 2)),
        vec![("kept.example".into(), 8)]
    );
    assert!(!store.state().backfill);
}

#[test]
fn hours_the_core_dropped_keep_what_was_taken() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour, &[("a.example", &[(hour, 100, 1)])]),
        secs(hour),
    );
    store.flush(secs(hour)).unwrap();
    // the app comes back after the core no longer keeps that hour
    let mut later = core("core-a", hour, hour + 80, &[]);
    later.retained_since = hour + 8;
    collect(&mut store, &later, secs(hour + 80));
    let report = by_target(&mut store, hour, hour);
    assert_eq!(downloads(&report), vec![("a.example".into(), 100)]);
    assert_eq!(report.hours[0].coverage, Coverage::Gap);
}

#[test]
fn hours_nothing_was_taken_of_are_unknown_until_seen() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    assert_eq!(by_target(&mut store, hour, hour).hours[0].coverage, Coverage::Unknown);
    collect(&mut store, &core("core-a", hour + 2, hour + 3, &[]), secs(hour + 3));
    let coverage: Vec<Coverage> = by_target(&mut store, hour, hour + 4)
        .hours
        .iter()
        .map(|hour| hour.coverage)
        .collect();
    // before the core ran, then read, pending, and not seen yet
    assert_eq!(
        coverage,
        vec![
            Coverage::Complete,
            Coverage::Complete,
            Coverage::Complete,
            Coverage::Pending,
            Coverage::Unknown
        ]
    );
}

#[test]
fn a_read_the_clock_overtook_sets_no_baseline() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    store.observe(&core("core-a", hour, hour, &[]), secs(hour)).unwrap();
    let (since, until, _) = store.pending_request(hour - 1, hour).unwrap();
    let late = core("core-a", hour, hour + 1, &[("new.example", &[(hour + 1, 3, 1)])]);
    store.apply_pending(&late, since, until, secs(hour + 1)).unwrap();
    assert!(
        !store.pending.contains_key(&(hour + 1)),
        "the hour not asked for is not taken"
    );
    assert_eq!(store.pending_request(hour, hour + 1).unwrap().2, None);
}

#[test]
fn clearing_takes_every_file_away() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour + 2, &[("a.example", &[(hour, 1, 1)])]),
        secs(hour + 2),
    );
    let aside = dir.0.join("hours").join("2026-11-13.jsonl.corrupt");
    std::fs::write(&aside, b"x").unwrap();
    std::fs::write(dir.0.join("keys.jsonl.tmp"), b"x").unwrap();
    store.clear(secs(hour + 2)).unwrap();
    assert!(!aside.exists());
    assert!(!dir.0.join("keys.jsonl.tmp").exists());
    assert!(store.recording());
}

#[test]
fn export_covers_the_pending_hours() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(
        &mut store,
        &core("core-a", hour, hour, &[("a.example", &[(hour, 1, 1)])]),
        secs(hour),
    );
    let path = dir.0.join("export.connections.json");
    store.export(&path, "client-1", "2026-11-13T00:30:00Z", hour).unwrap();
    let exported: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let hours = exported["coverage"]["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 1);
    assert_eq!(hours[0]["coverage"], "pending");
    assert_eq!(hours[0]["cores"][0]["counting"], "on");
}

#[test]
fn a_stop_is_vouched_for_only_by_a_read_nothing_followed() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(&mut store, &core("core-a", hour, hour + 1, &[]), secs(hour + 1));
    store.read_in_full("core-a", 1);
    // a collection of the same core comes between the final read and the stop
    store
        .observe(&core("core-a", hour, hour + 2, &[]), secs(hour + 2))
        .unwrap();
    store.mark_stopped("core-a", 1, secs(hour + 2)).unwrap();
    assert!(!store.state().instance.as_ref().unwrap().stopped);
    store.read_in_full("core-a", 2);
    store.mark_stopped("core-a", 1, secs(hour + 2)).unwrap();
    assert!(!store.state().instance.as_ref().unwrap().stopped, "another final read");
}

#[test]
fn appends_go_on_from_a_whole_line() {
    let dir = TempDir::new();
    let path = dir.0.join("lines.jsonl");
    std::fs::create_dir_all(&dir.0).unwrap();
    std::fs::write(&path, b"{\"old\":1}\n{\"new\":").unwrap();
    crate::files::append_lines(&path, &[b"{\"new\":2}".to_vec()]).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"old\":1}\n{\"new\":2}\n");
}

#[test]
fn an_hour_read_past_with_nothing_kept_is_unknown() {
    let dir = TempDir::new();
    let hour = MIDNIGHT;
    let mut store = recording(&dir.0, hour);
    collect(&mut store, &core("core-a", hour, hour + 2, &[]), secs(hour + 2));
    drop(store);
    std::fs::remove_dir_all(dir.0.join("hours")).unwrap();
    let mut store = open(&dir.0, hour + 2);
    assert_eq!(by_target(&mut store, hour, hour).hours[0].coverage, Coverage::Unknown);
}
