//! Each metric of the LSM (004 T025, FR-019) against what the test did.

mod common;

use common::{get, open, small, ts};
use teseladb_lsm::LsmConfig;
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

#[test]
fn every_metric_follows_what_happened() {
    let cfg = LsmConfig { l0_trigger: 3, l0_slow: 2, l0_reject: 100, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let empty = lsm.metrics();
    assert_eq!((empty.files, empty.compactions_total, empty.memtable_bytes), (0, 0, 0));
    assert_eq!((empty.write_amp, empty.read_files_per_get, empty.cache_hit_ratio), (0.0, 0.0, 0.0));
    assert_eq!(empty.level_bytes.len(), cfg.levels + 1);
    // Writes fill the memtable; each flush adds a file to level 0.
    lsm.put(G, 1, b"a", ts(1, 0), &[1; 1000]).unwrap();
    assert!(lsm.metrics().memtable_bytes >= 1000);
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let one = lsm.metrics();
    assert_eq!((one.files, one.memtable_bytes), (1, 0));
    assert_eq!(one.level_files[..2], [1, 0]);
    assert!(one.level_bytes[0] > 0 && one.cache_bytes > 0, "{one:?}");
    assert!(one.write_amp > 1.0, "a file is larger than the bytes written: {one:?}");
    // With two files in level 0 the next write is made while the LSM asks to slow down.
    lsm.put(G, 2, b"b", ts(2, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.put(G, 3, b"c", ts(3, 0), b"v").unwrap();
    assert_eq!((lsm.metrics().stalls_total, lsm.metrics().rejects_total), (1, 0));
    // The third file starts a compaction into level 1.
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let compacted = lsm.metrics();
    assert_eq!(compacted.compactions_total, 1);
    assert_eq!(compacted.level_bytes[0], 0);
    assert_eq!(compacted.level_files[..2], [0, 1]);
    assert!(compacted.level_bytes[1] > 0);
    // A point read searches one file; the second read of the same block finds it cached.
    let v = lsm.view();
    get(&mut lsm, &mut disk, &v, b"a", ts(9, 0)).unwrap();
    get(&mut lsm, &mut disk, &v, b"a", ts(9, 0)).unwrap();
    let read = lsm.metrics();
    assert_eq!(read.read_files_per_get, 1.0);
    assert_eq!((read.gets_total, read.get_files_total), (2, 2));
    assert!(read.cache_hit_ratio > 0.0 && read.cache_hit_ratio < 1.0, "{read:?}");
}

#[test]
fn rejects_count_writes_made_under_the_reject_signal() {
    let cfg = LsmConfig { l0_trigger: 100, l0_slow: 1, l0_reject: 2, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 1..=3u64 {
        lsm.put(G, i, b"k", ts(i, 0), b"v").unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let m = lsm.metrics();
    assert_eq!((m.stalls_total, m.rejects_total), (1, 1));
}

#[test]
fn the_memtable_bytes_split_the_active_memtable_from_the_frozen_ones() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    assert_eq!(lsm.memtables_bytes(), (0, 0));
    lsm.put(G, 1, b"a", ts(1, 0), &[1; 1000]).unwrap();
    let (all, active) = lsm.memtables_bytes();
    assert!(active >= 1000, "{active}");
    assert_eq!(all, active);
    // Frozen and not yet flushed, its bytes count among all of them and no more as active.
    lsm.flush();
    lsm.put(G, 2, b"b", ts(2, 0), &[2; 300]).unwrap();
    let (all, active) = lsm.memtables_bytes();
    assert!((300..1000).contains(&active), "{active}");
    assert!(all >= active + 1000, "{all} {active}");
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.memtables_bytes(), (active, active));
}

/// FR-019, FR-043: the space of the files by kind adds up to the bytes of the levels and of the
/// value files, and the data blocks of a file follow one another, so only its last page has room
/// left.
#[test]
fn the_space_of_the_files_adds_up_and_their_data_blocks_leave_no_pages_half_empty() {
    let cfg =
        LsmConfig { codec: teseladb_lsm::Codec::Zlib, value_separation_bytes: 2_000, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    assert_eq!(lsm.space(), teseladb_lsm::SpaceUse::default());
    let mut index = 0;
    for round in 1..=3u64 {
        for i in 0..300u32 {
            index += 1;
            let key = format!("k{i:05}");
            let value = format!("{round}-{i}-").repeat(if i % 50 == 0 { 400 } else { 20 });
            lsm.put(G, index, key.as_bytes(), ts(round, i), value.as_bytes()).unwrap();
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.compact_all();
    for _ in 0..1_000 {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let (s, m) = (lsm.space(), lsm.metrics());
    assert!(s.data.files > 0 && s.history.files > 0 && s.values.files > 0, "{s:?}");
    assert_eq!(s.data.files + s.history.files, m.files + m.history_files);
    assert_eq!(s.data.bytes() + s.history.bytes(), m.level_bytes.iter().sum::<u64>());
    assert_eq!(s.values.bytes(), m.value_bytes);
    let page = teseladb_lsm::sst::ALIGN as u64;
    for kind in [s.data, s.history, s.values] {
        assert!(kind.blocks >= kind.files as u64 && kind.meta >= 2 * page * kind.files as u64);
        assert!(kind.stored < kind.raw, "zlib shrinks repeated text: {kind:?}");
        assert!(kind.pages - kind.stored < page * kind.files as u64, "{kind:?}");
    }
    assert!(s.data.blocks > 2 * s.data.files as u64, "{s:?}");
}
