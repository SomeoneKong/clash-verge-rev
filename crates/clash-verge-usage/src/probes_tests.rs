#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{
    model::day_name,
    probes::{PROBE_RETENTION_DAYS, ProbeStore, SeqGap, SeqGapReason},
};

const INSTANCE: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "66666666-7777-4888-8999-000000000000";
/// 2026-11-15, as days since 1970-01-01.
const DAY: i64 = 20_407;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clash-verge-probes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn event(seq: u64, day: i64, config: u64) -> Value {
    let time = chrono::NaiveDate::default()
        .checked_add_signed(chrono::TimeDelta::days(day))
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339();
    json!({ "seq": seq, "time": time, "kind": "probe", "config": config, "exclusions": 1 })
}

fn events(range: std::ops::RangeInclusive<u64>, day: i64) -> Vec<Value> {
    range.map(|seq| event(seq, day, 1)).collect()
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn day_file(dir: &Path, day: i64) -> PathBuf {
    dir.join(format!("{}.jsonl", day_name(day)))
}

#[test]
fn appends_events_once_and_moves_the_cursor() {
    let dir = temp_dir("append");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    let plan = store.begin(INSTANCE, "2026-11-15T00:00:00Z", 1).unwrap();
    assert_eq!(plan.after, 0);
    assert_eq!(store.append(INSTANCE, 1, 0, &events(1..=3, DAY)).unwrap(), 3);
    // read again from an earlier cursor: nothing is written twice
    assert_eq!(store.append(INSTANCE, 1, 0, &events(1..=5, DAY)).unwrap(), 2);
    let instance = &store.meta().instances[INSTANCE];
    assert_eq!(instance.cursor, 5);
    assert_eq!(instance.written, vec![[1, 5]]);
    let stored = lines(&day_file(&dir, DAY));
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[0]["instance"], INSTANCE);
    assert_eq!(stored[0]["schema"], 1);
    assert!(store.begin(INSTANCE, "", 1).unwrap().after == 5);
}

#[test]
fn records_what_the_core_dropped_as_a_gap() {
    let dir = temp_dir("dropped");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    // first seen with the oldest kept event 10
    assert_eq!(store.begin(INSTANCE, "", 10).unwrap().after, 9);
    store.append(INSTANCE, 1, 9, &events(10..=12, DAY)).unwrap();
    // the core dropped 13..=19 before the next collection
    assert_eq!(store.begin(INSTANCE, "", 20).unwrap().after, 19);
    assert_eq!(
        store.meta().instances[INSTANCE].gaps,
        vec![
            SeqGap {
                from: 1,
                to: 9,
                reason: SeqGapReason::Dropped
            },
            SeqGap {
                from: 13,
                to: 19,
                reason: SeqGapReason::Dropped
            },
        ]
    );
}

#[test]
fn takes_lines_appended_after_the_index_into_it() {
    let dir = temp_dir("lagging");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=2, DAY)).unwrap();
    }
    // lines on disk whose index entry was not saved, and an incomplete last line
    let path = day_file(&dir, DAY);
    let mut bytes = std::fs::read(&path).unwrap();
    for seq in 3..=4 {
        let mut line = event(seq, DAY, 2);
        line["instance"] = INSTANCE.into();
        line["schema"] = 1.into();
        bytes.extend(serde_json::to_vec(&line).unwrap());
        bytes.push(b'\n');
    }
    bytes.extend(b"{\"seq\":5,");
    std::fs::write(&path, &bytes).unwrap();

    let store = ProbeStore::open(&dir, 8).unwrap();
    let index = &store.meta().days[&day_name(DAY)];
    assert_eq!(index.seqs[INSTANCE], vec![[1, 4]]);
    assert!(index.contexts[INSTANCE].contains("config:2"));
    assert_eq!(lines(&path).len(), 4, "the incomplete line is cut off");
    assert_eq!(store.meta().instances[INSTANCE].written, vec![[1, 4]]);
    // the cursor stays where the saved index left it: the lines are kept, the rest is read again
    assert_eq!(store.meta().instances[INSTANCE].cursor, 2);
}

#[test]
fn rebuilds_a_lost_index_with_the_coverage_unknown() {
    let dir = temp_dir("rebuild");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=3, DAY)).unwrap();
    }
    std::fs::remove_file(dir.join("meta.json")).unwrap();
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    let instance = &store.meta().instances[INSTANCE];
    assert!(instance.coverage_unknown);
    assert_eq!(
        instance.cursor, 3,
        "a running core is read on after what the files hold"
    );
    assert_eq!(store.begin(INSTANCE, "", 1).unwrap().after, 3);
}

#[test]
fn records_unreadable_events_as_corrupt_and_reads_them_again() {
    let dir = temp_dir("corrupt");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=4, DAY)).unwrap();
    store.append(OTHER, 1, 0, &[event(1, DAY, 1)]).unwrap();
    // damage the line of event 2
    let path = day_file(&dir, DAY);
    let text = std::fs::read_to_string(&path).unwrap();
    let damaged: Vec<String> = text
        .lines()
        .map(|line| {
            if line.contains("\"seq\":2,") && line.contains(INSTANCE) {
                "{not json".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&path, damaged.join("\n") + "\n").unwrap();

    let events_path = dir.join("out.probes.jsonl");
    let meta_path = dir.join("out.probes-meta.json");
    store.export(&events_path, &meta_path, "client", "now").unwrap();
    assert_eq!(
        store.meta().instances[INSTANCE].gaps,
        vec![SeqGap {
            from: 2,
            to: 2,
            reason: SeqGapReason::Corrupt
        }]
    );
    assert_eq!(lines(&events_path).len(), 4);
    assert!(dir.join(format!("{}.jsonl.corrupt", day_name(DAY))).exists());

    // the core still keeps it: read again, and the gap goes
    let plan = store.begin(INSTANCE, "", 1).unwrap();
    assert_eq!(plan.refetch, vec![[2, 2]]);
    store.append(INSTANCE, 1, 1, &events(2..=2, DAY)).unwrap();
    assert!(store.meta().instances[INSTANCE].gaps.is_empty());
}

#[test]
fn a_missing_day_file_is_a_corrupt_gap() {
    let dir = temp_dir("missing-day");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=2, DAY - 1)).unwrap();
        store.append(INSTANCE, 1, 2, &events(3..=4, DAY)).unwrap();
    }
    std::fs::remove_file(day_file(&dir, DAY - 1)).unwrap();
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert_eq!(
        store.meta().instances[INSTANCE].gaps,
        vec![SeqGap {
            from: 1,
            to: 2,
            reason: SeqGapReason::Corrupt
        }]
    );
}

#[test]
fn keeps_contexts_until_no_event_refers_to_them() {
    let dir = temp_dir("contexts");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store
        .append(INSTANCE, 1, 0, &[event(1, DAY - 100, 1), event(2, DAY, 2)])
        .unwrap();
    let missing = store.missing_contexts(INSTANCE).unwrap();
    assert!(missing.params);
    assert_eq!(missing.configs, vec![1, 2]);
    assert_eq!(missing.exclusions, vec![1]);
    store
        .save_contexts(
            INSTANCE,
            &[
                json!({"kind": "params", "schema": 1}),
                json!({"kind": "config", "version": 1}),
                json!({"kind": "config", "version": 2}),
            ],
            &[("exclusions".to_owned(), 1)],
        )
        .unwrap();
    assert!(store.missing_contexts(INSTANCE).unwrap().is_empty());

    store.retain(DAY).unwrap();
    assert!(!day_file(&dir, DAY - 100).exists());
    assert_eq!(
        store.meta().retained_since,
        Some(day_name(DAY - PROBE_RETENTION_DAYS + 1))
    );
    let kept: crate::probes::InstanceContexts =
        serde_json::from_slice(&std::fs::read(dir.join("contexts").join(format!("{INSTANCE}.json"))).unwrap()).unwrap();
    assert!(kept.contexts.contains_key("config:2"));
    assert!(
        !kept.contexts.contains_key("config:1"),
        "only the expired day referred to it"
    );
    assert!(kept.params.is_some());
}

#[test]
fn exports_the_events_with_what_explains_them() {
    let dir = temp_dir("export");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "2026-11-15T00:00:00Z", 3).unwrap();
    store.append(INSTANCE, 1, 2, &events(3..=4, DAY)).unwrap();
    store
        .save_contexts(INSTANCE, &[json!({"kind": "config", "version": 1})], &[])
        .unwrap();
    store.mark_stopped(INSTANCE).unwrap();
    let events_path = dir.join("usage.probes.jsonl");
    let meta_path = dir.join("usage.probes-meta.json");
    store
        .export(&events_path, &meta_path, "client-id", "2026-11-15T13:00:00Z")
        .unwrap();
    assert_eq!(lines(&events_path).len(), 2);
    let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["client"], "client-id");
    let instance = &meta["instances"][INSTANCE];
    assert_eq!(instance["first"], 3);
    assert_eq!(instance["last"], 4);
    assert_eq!(instance["gaps"][0]["reason"], "dropped");
    assert_eq!(instance["stoppedAfterLastCollection"], true);
    assert_eq!(instance["tail"], "unknown");
    assert!(instance["contexts"]["config:1"].is_object());
    assert_eq!(instance["missingContexts"], json!(["exclusions:1"]));
}

/// Appends a complete line for an event to a day file without the index knowing of it.
fn append_unindexed(dir: &Path, day: i64, seq: u64) {
    use std::io::Write as _;
    let mut line = event(seq, day, 1);
    line["instance"] = INSTANCE.into();
    line["schema"] = 1.into();
    let mut bytes = serde_json::to_vec(&line).unwrap();
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(day_file(dir, day))
        .unwrap();
    file.write_all(&bytes).unwrap();
}

/// Replaces the line of `seq` of the instance with one that cannot be read.
fn damage(dir: &Path, day: i64, seq: u64) {
    let path = day_file(dir, day);
    let text = std::fs::read_to_string(&path).unwrap();
    let damaged: Vec<String> = text
        .lines()
        .map(|line| {
            if line.contains(&format!("\"seq\":{seq},")) && line.contains(INSTANCE) {
                "{not json".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&path, damaged.join("\n") + "\n").unwrap();
}

fn edit_meta(dir: &Path, edit: impl FnOnce(&mut Value)) {
    let path = dir.join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut meta);
    std::fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
}

#[test]
fn keeps_a_damaged_day_file_whole_until_its_replacement_is_in_place() {
    let dir = temp_dir("rewrite-fails");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=3, DAY)).unwrap();
    damage(&dir, DAY, 2);
    // the replacement cannot be written
    let blocker = dir.join(format!("{}.jsonl.tmp", day_name(DAY)));
    std::fs::create_dir(&blocker).unwrap();
    let out = dir.join("out.probes.jsonl");
    let meta = dir.join("out.probes-meta.json");
    assert!(store.export(&out, &meta, "client", "now").is_err());
    assert_eq!(std::fs::read_to_string(day_file(&dir, DAY)).unwrap().lines().count(), 3);
    std::fs::remove_dir(&blocker).unwrap();

    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.export(&out, &meta, "client", "now").unwrap();
    let instance = &store.meta().instances[INSTANCE];
    assert_eq!(instance.written, vec![[1, 1], [3, 3]]);
    assert_eq!(
        instance.gaps,
        vec![SeqGap {
            from: 2,
            to: 2,
            reason: SeqGapReason::Corrupt
        }]
    );
}

#[test]
fn a_dropped_gap_leaves_out_what_is_on_disk() {
    let dir = temp_dir("dropped-written");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=2, DAY)).unwrap();
    }
    // 3 and 4 reached the disk, the index and the cursor did not
    append_unindexed(&dir, DAY, 3);
    append_unindexed(&dir, DAY, 4);
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    assert_eq!(store.begin(INSTANCE, "", 5).unwrap().after, 4);
    assert!(store.meta().instances[INSTANCE].gaps.is_empty());
}

#[test]
fn events_read_after_a_dropped_gap_was_recorded_fill_it() {
    let dir = temp_dir("dropped-filled");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 5).unwrap();
    assert_eq!(store.meta().instances[INSTANCE].gaps.len(), 1);
    // an earlier collection, still running, writes what the gap covers
    store.append(INSTANCE, 1, 0, &events(1..=4, DAY)).unwrap();
    assert!(store.meta().instances[INSTANCE].gaps.is_empty());
}

#[test]
fn reading_part_of_a_corrupt_gap_again_keeps_the_rest_of_it() {
    let dir = temp_dir("corrupt-split");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=10_000, DAY)).unwrap();
    }
    std::fs::remove_file(day_file(&dir, DAY)).unwrap();
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    assert_eq!(store.begin(INSTANCE, "", 1000).unwrap().refetch, vec![[1000, 10_000]]);
    store.append(INSTANCE, 1, 999, &events(1000..=5999, DAY)).unwrap();
    let gaps = &store.meta().instances[INSTANCE].gaps;
    assert_eq!(
        gaps,
        &vec![
            SeqGap {
                from: 1,
                to: 999,
                reason: SeqGapReason::Corrupt
            },
            SeqGap {
                from: 6000,
                to: 10_000,
                reason: SeqGapReason::Corrupt
            },
        ]
    );
    assert_eq!(store.begin(INSTANCE, "", 1000).unwrap().refetch, vec![[6000, 10_000]]);
}

#[test]
fn a_lost_index_with_damaged_lines_is_read_on_after_the_last_event() {
    let dir = temp_dir("rebuild-damaged");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store.append(INSTANCE, 1, 0, &events(1..=3, DAY)).unwrap();
    }
    damage(&dir, DAY, 2);
    std::fs::remove_file(dir.join("meta.json")).unwrap();
    let store = ProbeStore::open(&dir, 8).unwrap();
    let instance = &store.meta().instances[INSTANCE];
    assert!(instance.coverage_unknown);
    assert_eq!(instance.cursor, 3);
}

#[test]
fn appending_takes_lines_the_index_is_behind_on_into_it_first() {
    let dir = temp_dir("append-behind");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=2, DAY)).unwrap();
    // another collection wrote 3 and 4, then failed before indexing them
    append_unindexed(&dir, DAY, 3);
    append_unindexed(&dir, DAY, 4);
    assert_eq!(store.append(INSTANCE, 1, 2, &events(3..=3, DAY)).unwrap(), 0);
    assert_eq!(lines(&day_file(&dir, DAY)).len(), 4);
    drop(store);
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert_eq!(store.meta().days[&day_name(DAY)].seqs[INSTANCE], vec![[1, 4]]);
}

#[test]
fn a_stop_is_reported_only_once_on_disk() {
    let dir = temp_dir("stop-saved");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    let blocker = dir.join("meta.json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(store.mark_stopped(INSTANCE).is_err());
    assert!(!store.meta().instances[INSTANCE].stopped);
    std::fs::remove_dir(&blocker).unwrap();
    store.mark_stopped(INSTANCE).unwrap();
    drop(store);
    assert!(ProbeStore::open(&dir, 8).unwrap().meta().instances[INSTANCE].stopped);
}

#[test]
fn a_stop_noted_before_the_first_events_waits_for_them() {
    let dir = temp_dir("stop-pending");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.mark_stopped(OTHER).unwrap();
    store.retain(DAY).unwrap();
    store.begin(OTHER, "", 1).unwrap();
    assert!(store.meta().instances[OTHER].stopped);
    assert!(store.meta().pending_stops.is_empty());
}

#[test]
fn an_expiry_cut_short_is_finished_rather_than_taken_for_a_loss() {
    let dir = temp_dir("retain-cut-short");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        store
            .append(INSTANCE, 1, 0, &[event(1, DAY - 100, 1), event(2, DAY, 1)])
            .unwrap();
    }
    // the start of the retention was saved; the app ended before deleting the old day
    let first = day_name(DAY - PROBE_RETENTION_DAYS + 1);
    edit_meta(&dir, |meta| meta["retained_since"] = first.clone().into());
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert!(!day_file(&dir, DAY - 100).exists());
    let instance = &store.meta().instances[INSTANCE];
    assert!(instance.gaps.is_empty());
    assert_eq!(instance.written, vec![[2, 2]]);
    drop(store);

    // or after deleting it, before saving the index
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.append(INSTANCE, 1, 2, &[event(3, DAY - 95, 1)]).unwrap();
    }
    std::fs::remove_file(day_file(&dir, DAY - 95)).unwrap();
    let first = day_name(DAY - 90);
    edit_meta(&dir, |meta| meta["retained_since"] = first.clone().into());
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert!(store.meta().instances[INSTANCE].gaps.is_empty());
}

#[test]
fn contexts_a_need_or_test_started_under_are_kept_and_asked_for() {
    let dir = temp_dir("started-under");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    let mut need = event(1, DAY, 2);
    need["kind"] = "need".into();
    need["derivedConfig"] = 1.into();
    need["derivedExclusions"] = 3.into();
    let mut probe = event(2, DAY, 2);
    probe["startConfig"] = 4.into();
    store.append(INSTANCE, 1, 0, &[need, probe]).unwrap();
    let missing = store.missing_contexts(INSTANCE).unwrap();
    assert_eq!(missing.configs, vec![1, 2, 4]);
    assert_eq!(missing.exclusions, vec![1, 3]);

    store.save_contexts(INSTANCE, &[], &[("config".to_owned(), 1)]).unwrap();
    store
        .save_contexts(INSTANCE, &[json!({"kind": "config", "version": 1})], &[])
        .unwrap();
    let kept: crate::probes::InstanceContexts =
        serde_json::from_slice(&std::fs::read(dir.join("contexts").join(format!("{INSTANCE}.json"))).unwrap()).unwrap();
    assert!(kept.contexts.contains_key("config:1"));
    assert!(
        !kept.unavailable.contains("config:1"),
        "a context returned is no longer unavailable"
    );
    store.retain(DAY).unwrap();
    let kept: crate::probes::InstanceContexts =
        serde_json::from_slice(&std::fs::read(dir.join("contexts").join(format!("{INSTANCE}.json"))).unwrap()).unwrap();
    assert!(
        kept.contexts.contains_key("config:1"),
        "kept while the need refers to it"
    );
}

#[test]
fn an_index_of_an_earlier_format_is_read_anew() {
    let dir = temp_dir("format");
    {
        let mut store = ProbeStore::open(&dir, 8).unwrap();
        store.begin(INSTANCE, "", 1).unwrap();
        let mut need = event(1, DAY, 2);
        need["derivedConfig"] = 7.into();
        store.append(INSTANCE, 1, 0, &[need]).unwrap();
    }
    edit_meta(&dir, |meta| {
        meta["format"] = 1.into();
        let day = day_name(DAY);
        meta["days"][day.as_str()]["contexts"][INSTANCE] = json!(["config:2", "exclusions:1"]);
    });
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert!(store.meta().days[&day_name(DAY)].contexts[INSTANCE].contains("config:7"));
}

#[test]
fn exports_each_event_once_from_duplicated_lines() {
    let dir = temp_dir("export-dedup");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=2, DAY)).unwrap();
    // a write repeated after an interruption
    append_unindexed(&dir, DAY, 1);
    let out = dir.join("out.probes.jsonl");
    store
        .export(&out, &dir.join("out.probes-meta.json"), "client", "now")
        .unwrap();
    assert_eq!(lines(&out).len(), 2);
}

#[test]
fn only_a_core_the_usage_found_gone_gives_up_its_contexts() {
    let dir = temp_dir("ended");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "2026-11-15T12:00:00Z", 1).unwrap();
    store.append(INSTANCE, 1, 0, &[event(1, DAY, 3)]).unwrap();
    // the next core started after the clock was set back
    store.begin(OTHER, "2026-11-15T11:00:00Z", 1).unwrap();
    store.append(OTHER, 1, 0, &[event(1, DAY, 5)]).unwrap();
    // the usage was still collected from the first core: the second one read early stays
    store.end_before(INSTANCE).unwrap();
    store.give_up_ended().unwrap();
    assert_eq!(store.missing_contexts(INSTANCE).unwrap().configs, vec![3]);
    assert_eq!(store.missing_contexts(OTHER).unwrap().configs, vec![5]);
    drop(store);

    // after a restart, the usage collected from the second core tells the first one is gone
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.end_before(OTHER).unwrap();
    // a collection that read the first core comes late
    store.begin(INSTANCE, "2026-11-15T12:00:00Z", 1).unwrap();
    store.give_up_ended().unwrap();
    assert!(store.missing_contexts(INSTANCE).unwrap().is_empty());
    assert_eq!(store.missing_contexts(OTHER).unwrap().configs, vec![5]);
    assert!(!store.meta().instances[OTHER].ended);
}

#[test]
fn a_core_without_records_the_usage_moved_to_ends_the_one_before() {
    let dir = temp_dir("ended-old-core");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &[event(1, DAY, 3)]).unwrap();
    // the next core has no decision records: only the usage knows of it
    store.end_before(OTHER).unwrap();
    assert!(store.meta().instances[INSTANCE].ended);
    assert!(!store.meta().instances[OTHER].ended);
}

#[test]
fn a_confirmed_stop_ends_the_core() {
    let dir = temp_dir("stop-ends");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &[event(1, DAY, 3)]).unwrap();
    store.mark_stopped(INSTANCE).unwrap();
    assert!(store.meta().instances[INSTANCE].ended);
    store.give_up_ended().unwrap();
    assert!(store.missing_contexts(INSTANCE).unwrap().is_empty());
}

#[test]
fn exports_whether_params_are_missing_unavailable_or_kept() {
    let dir = temp_dir("export-params");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=1, DAY)).unwrap();
    let out = dir.join("out.probes.jsonl");
    let meta_path = dir.join("out.probes-meta.json");
    let mut params = || {
        store.export(&out, &meta_path, "client", "now").unwrap();
        let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        let instance = &meta["instances"][INSTANCE];
        (
            instance["params"].is_null(),
            instance["paramsUnavailable"].as_bool().unwrap(),
        )
    };
    assert_eq!(params(), (true, false), "not yet had");
    store.mark_stopped(INSTANCE).unwrap();
    store.give_up_ended().unwrap();
    let mut params = || {
        store.export(&out, &meta_path, "client", "now").unwrap();
        let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        let instance = &meta["instances"][INSTANCE];
        (
            instance["params"].is_null(),
            instance["paramsUnavailable"].as_bool().unwrap(),
        )
    };
    assert_eq!(params(), (true, true), "unavailable");
    store
        .save_contexts(INSTANCE, &[json!({"kind": "params"})], &[])
        .unwrap();
    store.export(&out, &meta_path, "client", "now").unwrap();
    let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert!(meta["instances"][INSTANCE]["params"].is_object());
    assert_eq!(meta["instances"][INSTANCE]["paramsUnavailable"], false);
}

#[test]
fn a_core_gone_with_gaps_has_its_coverage_unknown() {
    let dir = temp_dir("export-ended-gaps");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 3).unwrap();
    store.append(INSTANCE, 1, 2, &events(3..=3, DAY)).unwrap();
    let out = dir.join("out.probes.jsonl");
    let meta_path = dir.join("out.probes-meta.json");
    let unknown = |store: &mut ProbeStore| {
        store.export(&out, &meta_path, "client", "now").unwrap();
        let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta["instances"][INSTANCE]["coverageUnknown"].as_bool().unwrap()
    };
    assert!(!unknown(&mut store), "a running core may still fill its gaps");
    store.mark_stopped(INSTANCE).unwrap();
    assert!(unknown(&mut store));
    // a late collection fills the gaps after all
    store.append(INSTANCE, 1, 0, &events(1..=2, DAY)).unwrap();
    assert!(!unknown(&mut store));
}

#[test]
fn files_are_deleted_only_by_a_retention_start_on_disk() {
    let dir = temp_dir("retain-unsaved");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store
        .append(INSTANCE, 1, 0, &[event(1, DAY - 100, 1), event(2, DAY, 1)])
        .unwrap();
    let blocker = dir.join("meta.json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(store.retain(DAY).is_err());
    assert!(store.retain(DAY).is_err());
    assert!(store.begin(INSTANCE, "", 1).is_err());
    assert!(store.append(INSTANCE, 1, 2, &[event(3, DAY, 1)]).is_err());
    assert!(
        day_file(&dir, DAY - 100).exists(),
        "the old day stays until the retention is saved"
    );
    assert!(store.meta().retained_since.is_none());
    std::fs::remove_dir(&blocker).unwrap();
    store.retain(DAY).unwrap();
    drop(store);
    let store = ProbeStore::open(&dir, 8).unwrap();
    assert!(!day_file(&dir, DAY - 100).exists());
    assert!(store.meta().instances[INSTANCE].gaps.is_empty());
}

#[test]
fn lines_a_write_left_behind_are_indexed_only_once_on_disk() {
    let dir = temp_dir("tail-flush");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    store.append(INSTANCE, 1, 0, &events(1..=1, DAY)).unwrap();
    append_unindexed(&dir, DAY, 2);
    let path = day_file(&dir, DAY);
    let committed = store.meta().days[&day_name(DAY)].committed;
    // the file cannot be flushed
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions.clone()).unwrap();
    assert!(store.begin(INSTANCE, "", 1).is_err());
    assert_eq!(store.meta().days[&day_name(DAY)].committed, committed);
    assert_eq!(store.meta().instances[INSTANCE].written, vec![[1, 1]]);
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
    store.begin(INSTANCE, "", 1).unwrap();
    assert_eq!(store.meta().instances[INSTANCE].written, vec![[1, 2]]);
}

#[test]
fn a_context_kept_is_never_unavailable_whichever_answer_came_first() {
    let dir = temp_dir("contexts-exclusive");
    let mut store = ProbeStore::open(&dir, 8).unwrap();
    store
        .save_contexts(INSTANCE, &[json!({"kind": "config", "version": 1})], &[])
        .unwrap();
    // a later answer from a core that no longer keeps it
    store
        .save_contexts(INSTANCE, &[], &[("config".to_owned(), 1), ("config".to_owned(), 2)])
        .unwrap();
    let path = dir.join("contexts").join(format!("{INSTANCE}.json"));
    let kept: crate::probes::InstanceContexts = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(kept.contexts.contains_key("config:1"));
    assert!(!kept.unavailable.contains("config:1"));
    assert!(kept.unavailable.contains("config:2"));

    // a contradiction already on disk is cleared
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "params": {"kind": "params"},
            "contexts": {"config:3": {"kind": "config", "version": 3}},
            "unavailable": ["config:3", "params"]
        }))
        .unwrap(),
    )
    .unwrap();
    store.save_contexts(INSTANCE, &[], &[]).unwrap();
    let kept: crate::probes::InstanceContexts = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(kept.unavailable.is_empty());
}
