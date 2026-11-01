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
