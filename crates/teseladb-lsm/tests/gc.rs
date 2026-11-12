//! GC in compaction (004 story 3, T016, FR-007): a threshold per range; the newest version at or
//! below it stays and older ones go; deletions go only where nothing older is left; intents never
//! go; a resolved intent goes with its `Retract`.
//!
//! The reads below the threshold here look at what a compaction left on disk; the layers above
//! never read there.

mod common;

use common::{get, open, small, ts};
use teseladb_lsm::{Commit, GcTable, Kind, Lsm};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(disk, Nanos::ZERO);
}

fn value(lsm: &mut Lsm, disk: &mut MemDisk, key: &[u8], at: u64) -> Option<Vec<u8>> {
    let v = lsm.view();
    get(lsm, disk, &v, key, ts(at, 0)).map(|r| r.3)
}

#[test]
fn the_newest_version_at_or_below_the_threshold_stays_per_range() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for (i, t) in [10u64, 20, 30].iter().enumerate() {
        let i = i as u64;
        lsm.put(G, 2 * i + 1, b"a", ts(*t, 0), format!("a{t}").as_bytes()).unwrap();
        lsm.put(G, 2 * i + 2, b"m", ts(*t, 0), format!("m{t}").as_bytes()).unwrap();
    }
    // Range [a, f) collects at 25; [f, z) has no threshold.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"f".to_vec(), ts(25, 0))]));
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"a", 15), None, "a@10 is gone");
    assert_eq!(value(&mut lsm, &mut disk, b"a", 22), Some(b"a20".to_vec()));
    assert_eq!(value(&mut lsm, &mut disk, b"a", 35), Some(b"a30".to_vec()));
    assert_eq!(value(&mut lsm, &mut disk, b"m", 15), Some(b"m10".to_vec()), "no threshold there");
}

#[test]
fn a_deletion_below_the_threshold_goes_at_the_last_level_with_what_it_deleted() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"d", ts(5, 0), b"old").unwrap();
    lsm.delete(G, 2, b"d", ts(8, 0)).unwrap();
    lsm.put(G, 3, b"e", ts(5, 0), b"keep").unwrap();
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"z".to_vec(), ts(10, 0))]));
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"d", 6), None, "d@5 went with its deletion");
    assert_eq!(value(&mut lsm, &mut disk, b"e", 6), Some(b"keep".to_vec()));
    assert_eq!(lsm.metrics().files, 1);
}

#[test]
fn intents_never_go_and_keep_the_version_below_them() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"k", ts(3, 0), b"committed").unwrap();
    lsm.put(G, 2, b"k", ts(4, 0), b"older").unwrap();
    lsm.put_intent(G, 3, b"k", ts(5, 0), b"txn").unwrap();
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"z".to_vec(), ts(10, 0))]));
    compact(&mut lsm, &mut disk);
    let v = lsm.view();
    let hit = get(&mut lsm, &mut disk, &v, b"k", ts(6, 0)).unwrap();
    assert_eq!((hit.2, hit.3), (Kind::Intent, b"txn".to_vec()));
    // The intent aborts: the newest version below it is still there.
    lsm.resolve_intent(G, 4, b"k", ts(5, 0), None).unwrap();
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"k", 6), Some(b"older".to_vec()));
    assert_eq!(value(&mut lsm, &mut disk, b"k", 3), None, "k@3 was below the kept one");
}

#[test]
fn a_resolved_intent_and_its_retract_leave_together() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put_intent(G, 1, b"k", ts(5, 0), b"txn").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let commit = Commit { ts: ts(7, 0), kind: Kind::Put, value: b"done" };
    lsm.resolve_intent(G, 2, b"k", ts(5, 0), Some(commit)).unwrap();
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"k", 6), None);
    assert_eq!(value(&mut lsm, &mut disk, b"k", 8), Some(b"done".to_vec()));
    // Only the commit is left on disk: one version in one file.
    let m = lsm.metrics();
    assert_eq!(m.files, 1, "{m:?}");
}

#[test]
fn a_retract_keeps_hiding_an_older_version_of_its_timestamp_below() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    // A value at 10 in the last level.
    lsm.put(G, 1, b"k", ts(10, 0), b"old").unwrap();
    compact(&mut lsm, &mut disk);
    // An intent at the same timestamp replaces it, and aborts.
    lsm.put_intent(G, 2, b"k", ts(10, 0), b"txn").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.resolve_intent(G, 3, b"k", ts(10, 0), None).unwrap();
    // Level 0 goes down to level 1, above the old value: the intent and its `Retract` meet there.
    for i in 0..4u64 {
        lsm.put(G, 4 + i, format!("other{i}").as_bytes(), ts(20, 0), b"x").unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let m = lsm.metrics();
    assert!(m.level_bytes[1] > 0 && m.level_bytes[small().levels] > 0, "{m:?}");
    assert_eq!(value(&mut lsm, &mut disk, b"k", 15), None, "the replaced value stays hidden");
    // At the last level the `Retract` and the value it hides go together.
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"k", 15), None);
}

#[test]
fn a_range_threshold_replaces_its_intervals_and_leaves_the_rest() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let mut index = 0;
    for t in [10u64, 20, 30] {
        for k in [b"a", b"g", b"m"] {
            index += 1;
            let v = format!("{}{t}", k[0] as char);
            lsm.put(G, index, k, ts(t, 0), v.as_bytes()).unwrap();
        }
    }
    lsm.set_gc_thresholds(GcTable::new(vec![
        (b"a".to_vec(), b"f".to_vec(), ts(25, 0)),
        (b"f".to_vec(), b"z".to_vec(), ts(25, 0)),
    ]));
    // A range [f, k) takes 15: [a, f) keeps 25, and so does what is left of [f, z), [k, z).
    lsm.set_range_gc_threshold(&[(b"f".to_vec(), b"k".to_vec())], ts(15, 0));
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"a", 15), None, "a@10 went at 25");
    assert_eq!(value(&mut lsm, &mut disk, b"g", 15), Some(b"g10".to_vec()), "15 kept g@10");
    assert_eq!(value(&mut lsm, &mut disk, b"m", 15), None, "m@10 went at 25");
    assert_eq!(value(&mut lsm, &mut disk, b"m", 22), Some(b"m20".to_vec()));
}

/// A version that only serves the history (014 FR-007, the change description a document's
/// version carries) is read at its own mark and never below the threshold: at or below it, every
/// one goes, also the newest.
#[test]
fn history_versions_at_or_below_the_threshold_all_go() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let history = |s: &str| [&[teseladb_lsm::HISTORY_VALUE][..], s.as_bytes()].concat();
    lsm.put(G, 1, b"h", ts(5, 0), &history("h5")).unwrap();
    lsm.put(G, 2, b"h", ts(8, 0), &history("h8")).unwrap();
    lsm.put(G, 3, b"h", ts(12, 0), &history("h12")).unwrap();
    lsm.put(G, 4, b"k", ts(5, 0), b"\x01doc").unwrap();
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"z".to_vec(), ts(10, 0))]));
    compact(&mut lsm, &mut disk);
    assert_eq!(value(&mut lsm, &mut disk, b"h", 9), None, "h@8 went too");
    assert_eq!(value(&mut lsm, &mut disk, b"h", 12), Some(history("h12")));
    assert_eq!(value(&mut lsm, &mut disk, b"k", 9), Some(b"\x01doc".to_vec()), "a document stays");
}

/// Bytes of every file of the LSM.
fn files_bytes(lsm: &Lsm) -> u64 {
    lsm.metrics().level_bytes.iter().sum()
}

#[test]
fn files_far_above_the_live_data_are_all_compacted_and_collected() {
    // Found by the sustained load of 002 (11-11-2026): a small data set rewritten many times
    // stayed in levels within their budgets, and in the last level, whose versions no
    // compaction collected, far above its live bytes.
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let value = vec![7u8; 1024];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|k| format!("k{k:04}").into_bytes()).collect();
    let mut index = 0;
    for round in 1..=100u64 {
        for k in &keys {
            index += 1;
            lsm.put(G, index, k, ts(round, 0), &value).unwrap();
        }
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let before = files_bytes(&lsm);
    let live = (keys.len() * (5 + 1 + value.len())) as u64;
    assert!(before > live * teseladb_lsm::SPACE_AMP + teseladb_lsm::SPACE_FLOOR, "{before}");
    // Every version but the newest is below the threshold.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(100, 0))]));
    // Without the live bytes the LSM keeps its leveled compactions only; the compactions' budget
    // refills with time.
    lsm.run_io(&mut disk, Nanos::from_secs(10));
    let leveled = files_bytes(&lsm);
    assert!(leveled > 4 * live, "the leveled compactions leave the old versions: {leveled}");
    lsm.set_live_bytes(live);
    lsm.run_io(&mut disk, Nanos::from_secs(20));
    let after = files_bytes(&lsm);
    assert!(after < 4 * live, "{after} bytes of files for {live} live");
    for k in &keys {
        assert_eq!(get_value(&mut lsm, &mut disk, k), Some(value.clone()));
    }
    // A second pass finds nothing to do: the space is within what the last one left.
    let compactions = lsm.metrics().compactions_total;
    lsm.run_io(&mut disk, Nanos::from_secs(30));
    assert_eq!(lsm.metrics().compactions_total, compactions);
}

fn get_value(lsm: &mut Lsm, disk: &mut MemDisk, key: &[u8]) -> Option<Vec<u8>> {
    let v = lsm.view();
    get(lsm, disk, &v, key, ts(1000, 0)).map(|r| r.3)
}

#[test]
fn a_full_compaction_waits_for_the_threshold_to_pass_half_of_what_the_last_one_kept() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let value = vec![3u8; 1024];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|k| format!("k{k:04}").into_bytes()).collect();
    let mut index = 0;
    for round in 1..=100u64 {
        for k in &keys {
            index += 1;
            lsm.put(G, index, k, ts(round, 0), &value).unwrap();
        }
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    let live = (keys.len() * (5 + 1 + value.len())) as u64;
    lsm.set_live_bytes(live);
    // The history keeps the versions of the last half: one compaction of everything leaves them.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(50, 0))]));
    lsm.run_io(&mut disk, Nanos::from_secs(10));
    let kept = files_bytes(&lsm);
    assert!(kept > live * teseladb_lsm::SPACE_AMP + teseladb_lsm::SPACE_FLOOR, "{kept}");
    let compactions = lsm.metrics().compactions_total;
    // The threshold moved, but not past half of what the last one kept (H-054): nothing new.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(75, 0))]));
    lsm.run_io(&mut disk, Nanos::from_secs(20));
    assert_eq!(lsm.metrics().compactions_total, compactions);
    // Past it, the next compaction of everything collects what the history no longer needs.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(100, 0))]));
    lsm.run_io(&mut disk, Nanos::from_secs(30));
    let after = files_bytes(&lsm);
    let m = lsm.metrics();
    assert!(
        after < 4 * live,
        "{after} bytes of files for {live} live; levels {:?}, compactions {} (before {compactions}), full after {}",
        m.level_bytes,
        m.compactions_total,
        m.full_after
    );
    for k in &keys {
        assert_eq!(get_value(&mut lsm, &mut disk, k), Some(value.clone()));
    }
}

/// A range whose threshold stays low (an idle range, whose leader no longer raises it) does not
/// hold back the compaction of everything that the threshold of another range lets collect: the
/// next one only needs some range's threshold past what the last one could not collect (H-051 of
/// the native tests: one idle range kept a core's files at 1 GiB for 50 MiB of live data).
#[test]
fn an_idle_range_with_a_low_threshold_does_not_hold_back_the_others_collection() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    // Not 5, which marks a version that only serves the history.
    let value = vec![6u8; 1024];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|k| format!("k{k:04}").into_bytes()).collect();
    let mut index = 0;
    // An idle range of one key, written once.
    index += 1;
    lsm.put(G, index, b"a", ts(1, 0), b"idle").unwrap();
    for round in 1..=100u64 {
        for k in &keys {
            index += 1;
            lsm.put(G, index, k, ts(round, 0), &value).unwrap();
        }
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    let live = (keys.len() * (5 + 1 + value.len())) as u64;
    lsm.set_live_bytes(live);
    let idle = (b"a".to_vec(), b"b".to_vec(), ts(1, 0));
    let table = |t: u64| GcTable::new(vec![idle.clone(), (b"k".to_vec(), b"l".to_vec(), ts(t, 0))]);
    // A first compaction of everything, with the history of the written range still kept.
    lsm.set_gc_thresholds(table(50));
    lsm.run_io(&mut disk, Nanos::from_secs(10));
    assert!(files_bytes(&lsm) > live * teseladb_lsm::SPACE_AMP + teseladb_lsm::SPACE_FLOOR);
    // The written range's threshold passes everything; the idle one's stays where it was.
    lsm.set_gc_thresholds(table(100));
    lsm.run_io(&mut disk, Nanos::from_secs(30));
    let after = files_bytes(&lsm);
    assert!(after < 4 * live, "{after} bytes of files for {live} live");
    for k in &keys {
        assert_eq!(get_value(&mut lsm, &mut disk, k), Some(value.clone()));
    }
}

#[test]
fn a_compaction_of_everything_follows_once_the_threshold_passed_half_of_what_the_last_kept() {
    // H-054: the next compaction of everything waited for the threshold to pass the newest
    // version the last one saw, a whole history window, so the history kept by a compaction near
    // the end of the writes stayed on disk for a window after they stopped.
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let value = vec![6u8; 1024];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|k| format!("k{k:04}").into_bytes()).collect();
    let mut index = 0;
    for round in 1..=100u64 {
        for k in &keys {
            index += 1;
            lsm.put(G, index, k, ts(round, 0), &value).unwrap();
        }
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    let live = (keys.len() * (5 + 1 + value.len())) as u64;
    lsm.set_live_bytes(live);
    let table = |t: u64| GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(t, 0))]);
    // The first compaction keeps the versions above 50: half of them.
    lsm.set_gc_thresholds(table(50));
    lsm.run_io(&mut disk, Nanos::from_secs(10));
    let first = files_bytes(&lsm);
    assert!(first > live * teseladb_lsm::SPACE_AMP + teseladb_lsm::SPACE_FLOOR);
    // Halfway to the newest version it saw (100), nothing more goes.
    lsm.set_gc_thresholds(table(75));
    lsm.run_io(&mut disk, Nanos::from_secs(20));
    assert_eq!(files_bytes(&lsm), first, "a quarter of the kept versions is not worth it yet");
    // A clearing elsewhere, whose threshold is above every version, does not open the gate: its
    // own compaction runs, and then no other until the thresholds handed move.
    lsm.clear_range(GroupId(9), vec![(b"x".to_vec(), b"y".to_vec())]);
    lsm.run_io(&mut disk, Nanos::from_secs(21));
    let compactions = lsm.metrics().compactions_total;
    for s in 22..30 {
        lsm.run_io(&mut disk, Nanos::from_secs(s));
    }
    assert_eq!(lsm.metrics().compactions_total, compactions, "no compaction after the clearing");
    // Past halfway, the next one runs and collects what aged out.
    lsm.set_gc_thresholds(table(76));
    lsm.run_io(&mut disk, Nanos::from_secs(40));
    let second = files_bytes(&lsm);
    assert!(second * 10 < first * 6, "{second} bytes of files after {first}");
    for k in &keys {
        assert_eq!(get_value(&mut lsm, &mut disk, k), Some(value.clone()));
    }
}

#[test]
fn a_compaction_asked_for_flushes_and_collects_everything_the_thresholds_allow() {
    // H-054, DV-017: `compact` freed nothing, so the history its threshold no longer needed stayed
    // on disk until the space of the files asked for a compaction of everything.
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let value = vec![6u8; 1024];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|k| format!("k{k:04}").into_bytes()).collect();
    let mut index = 0;
    for round in 1..=20u64 {
        for k in &keys {
            index += 1;
            lsm.put(G, index, k, ts(round, 0), &value).unwrap();
        }
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    // No live bytes handed: the space of the files never asks for a compaction by itself.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(20, 0))]));
    lsm.run_io(&mut disk, Nanos::from_secs(1));
    let before = files_bytes(&lsm);
    let id = lsm.compact_now();
    let mut done = Vec::new();
    for s in 2..40 {
        done.extend(lsm.run_io(&mut disk, Nanos::from_secs(s)).compacted);
        if !done.is_empty() {
            break;
        }
    }
    assert_eq!(done, vec![id], "its end is reported once");
    let live = (keys.len() * (5 + 1 + value.len())) as u64;
    let after = files_bytes(&lsm);
    assert!(after < 2 * live, "{after} bytes of files after {before}, for {live} live");
    assert_eq!(lsm.metrics().memtable_bytes, 0, "the memtable went to a file first");
    for k in &keys {
        assert_eq!(get_value(&mut lsm, &mut disk, k), Some(value.clone()));
    }
}
