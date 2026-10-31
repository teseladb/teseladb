//! Range tombstones (004 story 3, T017, FR-008): reads hide what they cover from their timestamp
//! on, they survive compactions split across files, and the compaction that reaches the last
//! level past the GC threshold removes their data and them.

mod common;

use common::{get, open, scan, small, ts};
use teseladb_lsm::{GcTable, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn key(i: u64) -> Vec<u8> {
    format!("k{i:05}").into_bytes()
}

#[test]
fn a_tombstone_split_across_compacted_files_still_hides_everything() {
    // Small files so the compaction writes several of them.
    let cfg = LsmConfig { target_file_bytes: 16 << 10, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..2000 {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &[7; 40]).unwrap();
    }
    lsm.delete_range(G, 3000, &key(500), &key(1500), ts(20, 0)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert!(lsm.metrics().files > 2, "{:?}", lsm.metrics());
    let v = lsm.view();
    let rows = scan(&mut lsm, &mut disk, &v, b"", b"z", ts(25, 0));
    assert_eq!(rows.len(), 1000);
    assert!(rows.iter().all(|r| r.0 < key(500) || r.0 >= key(1500)));
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"", b"z", ts(15, 0)).len(), 2000, "before it");
    for i in [499, 500, 1000, 1499, 1500] {
        let hidden = (500..1500).contains(&i);
        assert_eq!(get(&mut lsm, &mut disk, &v, &key(i), ts(25, 0)).is_none(), hidden, "{i}");
    }
}

#[test]
fn past_the_threshold_at_the_last_level_the_data_and_the_tombstone_go() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for i in 0..100 {
        lsm.put(G, i + 1, &key(i), ts(10, 0), b"v").unwrap();
    }
    lsm.delete_range(G, 200, &key(0), &key(100), ts(20, 0)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    // Below the threshold of the tombstone nothing goes yet.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"z".to_vec(), ts(15, 0))]));
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let v = lsm.view();
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"", b"z", ts(15, 0)).len(), 100);
    // Past it, everything goes and no file is left.
    lsm.set_gc_thresholds(GcTable::new(vec![(b"a".to_vec(), b"z".to_vec(), ts(30, 0))]));
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(v);
    lsm.run_io(&mut disk, Nanos::ZERO);
    let m = lsm.metrics();
    assert_eq!(m.files, 0, "{m:?}");
    let v = lsm.view();
    assert!(scan(&mut lsm, &mut disk, &v, b"", b"z", ts(15, 0)).is_empty());
}

#[test]
fn a_compaction_with_only_tombstones_left_above_the_last_level_writes_them_alone() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    // Data in the last level, below what comes next.
    for i in 0..50 {
        lsm.put(G, i + 1, &key(i), ts(10, 0), b"v").unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    // Four flushes of tombstones alone start a compaction of level 0 into level 1.
    for n in 0..4u64 {
        lsm.delete_range(G, 100 + n, &key(n * 10), &key(n * 10 + 5), ts(20 + n, 0)).unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let m = lsm.metrics();
    assert_eq!(m.level_bytes[0], 0, "{m:?}");
    assert!(m.level_bytes[1] > 0, "the tombstones went down alone: {m:?}");
    let v = lsm.view();
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"", b"z", ts(30, 0)).len(), 30);
}
