//! Deleted keys in history files (004 FR-031): a compaction with nothing below a key sends the
//! whole chain of a key whose newest version is a deletion to the history file, so that a scan
//! at a recent mark does not walk its deletion. Alone in its binary: it reads the process's scan
//! counters.

mod common;

use common::{get, open, scan, small, ts};
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u64 = 100;

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
