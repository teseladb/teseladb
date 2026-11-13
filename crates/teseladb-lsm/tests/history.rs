//! History files (004 FR-031, D-162): a flush and a compaction leave in each file the newest
//! version of every key that is a value or a deletion and put the older ones in the file's
//! history file, which a read at or above the file's highest mark does not open.

mod common;

use common::{check_all, get, history, open, scan, scan_back, small, ts};
use teseladb_lsm::{Commit, GcTable, Kind, Lsm, LsmConfig, Step};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u64 = 100;

fn key(i: u64) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

fn value(version: u64) -> Vec<u8> {
    vec![version as u8; 300]
}

/// Versions `from..=to` of every key, each version at the mark of its number, flushed at the end.
fn write_versions(lsm: &mut Lsm, disk: &mut MemDisk, index: &mut u64, (from, to): (u64, u64)) {
    for version in from..=to {
        for i in 0..KEYS {
            *index += 1;
            lsm.put(G, *index, &key(i), ts(version, 0), &value(version)).unwrap();
        }
    }
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

/// Runs a compaction of everything to its end.
fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    let id = lsm.compact_now();
    assert_eq!(lsm.run_io(disk, Nanos::ZERO).compacted, vec![id]);
}

/// A scan of every key at `at` on a cold cache: its rows and the blocks it waited for.
fn cold_scan(cfg: &LsmConfig, disk: &mut MemDisk, at: u64) -> (Vec<common::model::Row>, usize) {
    let mut lsm = open(cfg, disk);
    let view = lsm.view();
    let mut s = view.scan(b"k", b"l", ts(at, 0));
    let (mut rows, mut waits) = (Vec::new(), 0);
    loop {
        match s.next(&mut lsm) {
            Step::Item(i) => rows.push((i.key, i.ts, i.kind, i.value.to_vec())),
            Step::Miss(_) => {
                waits += 1;
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return (rows, waits),
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

#[test]
fn a_flush_and_a_compaction_put_the_older_versions_in_history_files() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    // One version of each key: nothing older to set apart.
    write_versions(&mut lsm, &mut disk, &mut index, (1, 1));
    let m = lsm.metrics();
    assert_eq!((m.files, m.history_files, m.history_bytes), (1, 0, 0));
    // Six more of each: the flush keeps the newest in its file and five in its history file.
    write_versions(&mut lsm, &mut disk, &mut index, (2, 7));
    let m = lsm.metrics();
    assert_eq!((m.files, m.history_files), (2, 1));
    let flushed = m.level_bytes[0];
    assert!(m.history_bytes * 2 > flushed, "{} of {flushed} bytes are history", m.history_bytes);
    // The compaction of both leaves one version of each key in the files of the last level.
    compact(&mut lsm, &mut disk);
    let m = lsm.metrics();
    assert_eq!(m.level_files[0], 0);
    assert!(m.history_files >= 1, "{m:?}");
    // The six older versions of every key are in history files; what is left in the files of
    // the level is one version of each and the pages every file has of its own.
    let all: u64 = m.level_bytes.iter().sum();
    assert!(m.history_bytes >= 6 * KEYS * 300, "{} bytes of history", m.history_bytes);
    assert!((all - m.history_bytes) * 2 < all, "{m:?}");
    // Every version is still there at its mark.
    let view = lsm.view();
    for at in 1..=7u64 {
        let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(at, 0));
        assert_eq!(rows.len() as u64, KEYS, "at {at}");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!((&r.0, r.1, &r.3), (&key(i as u64), ts(at, 0), &value(at)), "at {at}");
        }
        let back = scan_back(&mut lsm, &mut disk, &view, b"k", b"l", ts(at, 0));
        assert_eq!(back.len() as u64, KEYS, "backward at {at}");
        assert_eq!((back[0].0.clone(), back[0].1), (key(KEYS - 1), ts(at, 0)));
        let one = get(&mut lsm, &mut disk, &view, &key(42), ts(at, 0)).expect("a version");
        assert_eq!((one.1, one.3), (ts(at, 0), value(at)));
    }
}

#[test]
fn a_scan_at_the_newest_mark_reads_no_block_of_history() {
    // Thirty versions of each key in three flushes and a compaction of everything: a scan at
    // the newest mark waits for the blocks of the newest versions alone, a thirtieth of them.
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 8 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for part in 0..3 {
        write_versions(&mut lsm, &mut disk, &mut index, (part * 10 + 1, part * 10 + 10));
    }
    compact(&mut lsm, &mut disk);
    let blocks = lsm.metrics().level_bytes.iter().sum::<u64>() / cfg.block_bytes as u64;
    drop(lsm);
    let (rows, waits) = cold_scan(&cfg, &mut disk, 30);
    assert_eq!(rows.len() as u64, KEYS);
    assert!(rows.iter().all(|r| r.1 == ts(30, 0)));
    assert!((waits as u64) * 8 < blocks, "{waits} blocks read of {blocks}");
    // Below the newest mark the history is read, and every key shows its version of then.
    let (rows, old_waits) = cold_scan(&cfg, &mut disk, 12);
    assert_eq!(rows.len() as u64, KEYS);
    assert!(rows.iter().all(|r| r.1 == ts(12, 0) && r.3 == value(12)));
    assert!(old_waits > waits, "{old_waits} blocks at an old mark, {waits} at the newest");
}

#[test]
fn a_key_under_an_intent_or_its_retraction_keeps_its_versions_in_the_file() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    // Two values and an intent above them: the intent is no value, so nothing goes apart.
    lsm.put(G, 1, b"a", ts(1, 0), b"one").unwrap();
    lsm.put(G, 2, b"a", ts(2, 0), b"two").unwrap();
    lsm.put_intent(G, 3, b"a", ts(3, 0), b"three").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.metrics().history_files, 0);
    // The intent is retracted in another file, whose only version of the key is the `Retract`.
    lsm.resolve_intent(G, 4, b"a", ts(3, 0), None).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.metrics().history_files, 0);
    let view = lsm.view();
    let read = |lsm: &mut Lsm, disk: &mut MemDisk, at: u64| {
        get(lsm, disk, &view, b"a", ts(at, 0)).map(|r| (r.1, r.3))
    };
    assert_eq!(read(&mut lsm, &mut disk, 1), Some((ts(1, 0), b"one".to_vec())));
    for at in [2, 3, 9] {
        assert_eq!(read(&mut lsm, &mut disk, at), Some((ts(2, 0), b"two".to_vec())), "at {at}");
    }
    // Once compacted the intent and its retraction are gone: the newest value leads the key and
    // the older one is history.
    compact(&mut lsm, &mut disk);
    assert_eq!(lsm.metrics().history_files, 1);
    let view = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &view, b"a", ts(1, 0)).map(|r| r.3), Some(b"one".to_vec()));
    assert_eq!(get(&mut lsm, &mut disk, &view, b"a", ts(5, 0)).map(|r| r.3), Some(b"two".to_vec()));
    // A committed intent at another mark: the commit leads, the rest is history.
    lsm.put_intent(G, 5, b"a", ts(6, 0), b"six").unwrap();
    let commit = Commit { ts: ts(7, 0), kind: Kind::Put, value: b"seven" };
    lsm.resolve_intent(G, 6, b"a", ts(6, 0), Some(commit)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    let view = lsm.view();
    for (at, want) in [(1u64, &b"one"[..]), (2, b"two"), (6, b"two"), (7, b"seven"), (9, b"seven")]
    {
        let got = get(&mut lsm, &mut disk, &view, b"a", ts(at, 0)).map(|r| r.3);
        assert_eq!(got.as_deref(), Some(want), "at {at}");
    }
}

/// A `Retract` in a newer source takes away the version of its mark, also a value that leads
/// its key in an older file (the simulator writes an intent at the mark of a value): the read
/// passes over it and shows the version under it, which is in the history file.
#[test]
fn a_value_retracted_from_a_newer_source_shows_the_version_under_it() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let mut index = 0;
    for key in [b"a", b"b", b"c"] {
        for (at, value) in [(1, &b"one"[..]), (2, b"two")] {
            index += 1;
            lsm.put(G, index, key, ts(at, 0), value).unwrap();
        }
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.metrics().history_files, 1);
    // An intent at the mark of the newest value of `b`, and its retraction.
    lsm.put_intent(G, index + 1, b"b", ts(2, 0), b"intent").unwrap();
    lsm.resolve_intent(G, index + 2, b"b", ts(2, 0), None).unwrap();
    let shown = |lsm: &mut Lsm, disk: &mut MemDisk| {
        let view = lsm.view();
        let one = get(lsm, disk, &view, b"b", ts(9, 0)).map(|r| r.3);
        assert_eq!(one.as_deref(), Some(&b"one"[..]));
        let rows = scan(lsm, disk, &view, b"a", b"z", ts(9, 0));
        let values: Vec<&[u8]> = rows.iter().map(|r| r.3.as_slice()).collect();
        assert_eq!(values, [&b"two"[..], b"one", b"two"]);
        let back = scan_back(lsm, disk, &view, b"a", b"z", ts(9, 0));
        let values: Vec<&[u8]> = back.iter().map(|r| r.3.as_slice()).collect();
        assert_eq!(values, [&b"two"[..], b"one", b"two"]);
    };
    // With the retraction in the memtable, in a newer file and after a compaction of both.
    shown(&mut lsm, &mut disk);
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    shown(&mut lsm, &mut disk);
    compact(&mut lsm, &mut disk);
    shown(&mut lsm, &mut disk);
}

#[test]
fn history_files_come_back_after_a_restart_and_leave_with_what_the_threshold_frees() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    write_versions(&mut lsm, &mut disk, &mut index, (1, 5));
    write_versions(&mut lsm, &mut disk, &mut index, (6, 9));
    compact(&mut lsm, &mut disk);
    let before = lsm.metrics();
    assert!(before.history_files >= 1);
    drop(lsm);
    // Reopened, it has the same files and every version at its mark.
    let mut lsm = open(&cfg, &mut disk);
    let after = lsm.metrics();
    assert_eq!(
        (after.files, after.history_files, after.history_bytes),
        (before.files, before.history_files, before.history_bytes)
    );
    let view = lsm.view();
    for at in [1u64, 5, 6, 9] {
        let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(at, 0));
        assert_eq!(rows.len() as u64, KEYS, "at {at}");
        assert!(rows.iter().all(|r| r.1 == ts(at, 0) && r.3 == value(at)), "at {at}");
    }
    drop(view);
    // The threshold passes every version: the history files leave the LSM and the disk.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"k".to_vec(), b"l".to_vec(), ts(20, 0))]));
    compact(&mut lsm, &mut disk);
    // The version the readers of other threads hold lets go of the old files.
    lsm.publish();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let m = lsm.metrics();
    assert_eq!((m.history_files, m.history_bytes), (0, 0));
    let on_disk = disk
        .list(&cfg.dir)
        .unwrap()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sst"))
        .count();
    assert_eq!(on_disk, m.files, "no file of history is left behind");
    let view = lsm.view();
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(9, 0));
    assert!(rows.iter().all(|r| r.1 == ts(9, 0)));
}

#[test]
fn the_versions_between_two_marks_come_from_the_files_and_their_history() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    write_versions(&mut lsm, &mut disk, &mut index, (1, 6));
    compact(&mut lsm, &mut disk);
    let view = lsm.view();
    let mut s = view.scan_time(b"k", b"l", ts(2, 0), ts(5, 0));
    let mut seen = Vec::new();
    loop {
        match s.next(&mut lsm) {
            Step::Item(i) => seen.push((i.key, i.ts)),
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Done => break,
            Step::Failed(e) => panic!("{e}"),
        }
    }
    // The versions of marks 3, 4 and 5 of every key, the newest first.
    let want: Vec<(Vec<u8>, _)> =
        (0..KEYS).flat_map(|i| [5u64, 4, 3].map(|v| (key(i), ts(v, 0)))).collect();
    assert_eq!(seen, want);
}

#[test]
fn random_histories_read_as_the_model_after_their_files_are_compacted() {
    // Values, deletions, intents and their resolutions, range deletions and flushes: every read
    // of the model at old and new marks holds with the versions in files and history files.
    for seed in 0..40 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&small(), &mut disk);
        let model = history(seed, 500, &mut lsm, &mut disk);
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
        compact(&mut lsm, &mut disk);
        check_all(&mut lsm, &mut disk, &model, 500 + 100);
        // And after a restart.
        drop(lsm);
        let mut lsm = open(&small(), &mut disk);
        check_all(&mut lsm, &mut disk, &model, 500 + 100);
    }
}

/// A document of about 300 bytes at `version`: each version changes two fields of a few bytes.
fn document(i: u64, version: u64) -> Vec<u8> {
    let mut doc: Vec<u8> = (0..300u64).map(|b| ((b * 7 + i) % 251) as u8).collect();
    doc[40..48].copy_from_slice(&version.to_le_bytes());
    doc[200..208].copy_from_slice(&(version * 31).to_le_bytes());
    doc
}

/// FR-035: the history file keeps each old version as a delta against the next newer version
/// of its key, and every version reads whole at its mark, warm and on a cold cache.
#[test]
fn old_versions_take_the_bytes_of_their_changes() {
    const VERSIONS: u64 = 30;
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for version in 1..=VERSIONS {
        for i in 0..KEYS {
            index += 1;
            lsm.put(G, index, &key(i), ts(version, 0), &document(i, version)).unwrap();
        }
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    let m = lsm.metrics();
    assert!(m.history_files >= 1, "{m:?}");
    // Whole, the old versions would take 29 × 100 × 300 bytes.
    let whole = (VERSIONS - 1) * KEYS * 300;
    assert!(m.history_bytes * 4 < whole, "{} bytes of history for {whole}", m.history_bytes);
    let view = lsm.view();
    for at in 1..=VERSIONS {
        let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(at, 0));
        assert_eq!(rows.len() as u64, KEYS, "at {at}");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!((r.1, &r.3), (ts(at, 0), &document(i as u64, at)), "at {at}");
        }
        let back = scan_back(&mut lsm, &mut disk, &view, b"k", b"l", ts(at, 0));
        assert_eq!(back[0].3, document(KEYS - 1, at), "backward at {at}");
        let one = get(&mut lsm, &mut disk, &view, &key(42), ts(at, 0)).expect("a version");
        assert_eq!((one.1, one.3), (ts(at, 0), document(42, at)));
    }
    for at in [1, VERSIONS / 2, VERSIONS - 1] {
        let (rows, _) = cold_scan(&cfg, &mut disk, at);
        assert!(rows.iter().enumerate().all(|(i, r)| r.3 == document(i as u64, at)), "at {at}");
    }
}
