//! Deleted keys in history files (004 FR-031): a compaction with nothing below a key sends the
//! whole chain of a key whose newest version is a deletion to the history file, so that a scan
//! at a recent mark does not walk its deletion. Alone in its binary: it reads the process's scan
//! counters, each test holding [`SERIAL`].

mod common;

use common::{get, open, scan, small, ts};
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u64 = 100;

/// Held by each test: they read counters of the whole process.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn key(i: u64) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    let id = lsm.compact_now();
    assert_eq!(lsm.run_io(disk, Nanos::ZERO).compacted, vec![id]);
}

#[test]
fn a_scan_at_a_recent_mark_walks_no_deletion_of_the_last_level() {
    // Every key written at 10 and the even ones deleted at 20 (002 run p46: scans walked 1.24
    // deletions for each item they gave).
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for i in 0..KEYS {
        index += 1;
        lsm.put(G, index, &key(i), ts(10, 0), b"v").unwrap();
    }
    for i in (0..KEYS).step_by(2) {
        index += 1;
        lsm.delete(G, index, &key(i), ts(20, 0)).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    let view = lsm.view();

    let (before, entries) = (teseladb_lsm::scan_breakdown(), teseladb_lsm::scan_totals().entries);
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(30, 0));
    let after = teseladb_lsm::scan_breakdown();
    assert_eq!(rows.len() as u64, KEYS / 2);
    assert_eq!(after.deleted, before.deleted, "{after:?}");
    assert_eq!(teseladb_lsm::scan_totals().entries, entries + KEYS / 2, "{after:?}");

    // Below the deletions every key is still there, from the history file.
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(15, 0));
    assert_eq!(rows.len() as u64, KEYS);
    assert!(get(&mut lsm, &mut disk, &view, &key(0), ts(15, 0)).is_some());
    assert!(get(&mut lsm, &mut disk, &view, &key(0), ts(25, 0)).is_none());
    assert!(get(&mut lsm, &mut disk, &view, &key(1), ts(25, 0)).is_some());
}

#[test]
fn a_file_below_whose_span_covers_a_deleted_key_it_does_not_hold_lets_the_deletion_go() {
    // The last level spans the deleted keys without holding them, and level 0 goes down to
    // level 1 (002 run p47: level 6 spanned nearly every key, and deletions stayed in the files).
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let mut index = 0;
    for k in [b"a", b"z"] {
        index += 1;
        lsm.put(G, index, k, ts(1, 0), b"v").unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    for i in 0..KEYS {
        index += 1;
        lsm.put(G, index, &key(i), ts(10, 0), b"v").unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    for i in (0..KEYS).step_by(2) {
        index += 1;
        lsm.delete(G, index, &key(i), ts(11, 0)).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    for extra in 0..6u64 {
        index += 1;
        lsm.put(G, index, format!("m{extra}").as_bytes(), ts(20 + extra, 0), b"v").unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.run_io(&mut disk, Nanos::ZERO);
    let m = lsm.metrics();
    assert!(m.level_files[1] > 0 && m.level_files[6] == 1, "{m:?}");
    let view = lsm.view();

    let before = teseladb_lsm::scan_breakdown();
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(30, 0));
    let after = teseladb_lsm::scan_breakdown();
    assert_eq!(rows.len() as u64, KEYS / 2);
    assert_eq!(after.deleted, before.deleted, "{after:?}");
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(10, 5));
    assert_eq!(rows.len() as u64, KEYS);
}

#[test]
fn a_write_below_a_deletion_set_apart_wakes_its_history_file() {
    // A key deleted at 20 and compacted whole into the history file, then written at 15 (writes
    // may come below a version flushed): a scan at 30 opens that history file at the key and
    // sees the deletion, which hides the write.
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, &key(1), ts(10, 0), b"v").unwrap();
    lsm.delete(G, 2, &key(1), ts(20, 0)).unwrap();
    lsm.put(G, 3, &key(2), ts(20, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    lsm.put(G, 4, &key(1), ts(15, 0), b"w").unwrap();
    let view = lsm.view();
    let before = teseladb_lsm::scan_breakdown();
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(30, 0));
    let after = teseladb_lsm::scan_breakdown();
    assert_eq!(rows.len(), 1);
    assert_eq!(after.history_woken, before.history_woken + 1, "{after:?}");
    assert_eq!(after.history_opened, before.history_opened, "{after:?}");
}

#[test]
fn a_scan_below_the_files_highest_mark_wakes_its_history_only_where_it_needs_it() {
    // Every key at 10 and 11, and a key outside the scan at 40, compacted together: the history
    // file holds the versions at 10. A scan at 30 needs none of them (002 run p48: history files
    // opened at the start of 0.66 scans each, 42 % of the entries scans took).
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 4 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for at in [10, 11] {
        for i in 0..KEYS {
            index += 1;
            lsm.put(G, index, &key(i), ts(at, 0), &[at as u8]).unwrap();
        }
    }
    index += 1;
    lsm.put(G, index, b"z", ts(40, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact(&mut lsm, &mut disk);
    let view = lsm.view();

    let before = teseladb_lsm::scan_breakdown();
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(30, 0));
    let after = teseladb_lsm::scan_breakdown();
    assert_eq!(rows.len() as u64, KEYS);
    assert!(rows.iter().all(|r| r.3 == [11]));
    assert_eq!(after.history, before.history, "{after:?}");
    assert_eq!(after.history_opened, before.history_opened, "{after:?}");

    // Below the versions of the file, the history file wakes at the first key and gives every
    // key its version of then.
    let rows = scan(&mut lsm, &mut disk, &view, b"k", b"l", ts(10, 5));
    assert_eq!(rows.len() as u64, KEYS);
    assert!(rows.iter().all(|r| r.3 == [10]), "{rows:?}");
}
