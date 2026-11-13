//! Size-tiered compaction above level 2 (004 FR-036): the flushes of level 0 join files of a
//! similar size among themselves, and level 1 is rewritten only once level 0 holds about as many
//! bytes as it; from level 2 down the compaction stays leveled.

mod common;

use common::{check_all, get, open, small, ts};
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn key(i: u64) -> Vec<u8> {
    format!("k{i:05}").into_bytes()
}

/// Puts keys `keys` at mark `at` and flushes them into a file of level 0.
fn flush(lsm: &mut Lsm, disk: &mut MemDisk, index: &mut u64, keys: std::ops::Range<u64>, at: u64) {
    for i in keys {
        *index += 1;
        lsm.put(G, *index, &key(i), ts(at, 0), &[at as u8; 60]).unwrap();
    }
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

fn cfg() -> LsmConfig {
    LsmConfig { l1_bytes: 8 << 20, memtable_bytes: 4 << 20, ..small() }
}

#[test]
fn flushes_join_in_level_0_until_it_weighs_as_much_as_level_1() {
    let mut disk = MemDisk::new();
    let cfg = cfg();
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    // Level 1 starts empty: the first four flushes go down to it.
    for round in 0..4 {
        flush(&mut lsm, &mut disk, &mut index, round * 1000..(round + 1) * 1000, 1);
    }
    let m = lsm.metrics();
    assert_eq!(m.level_files[0], 0, "{m:?}");
    let level_1 = m.level_bytes[1];
    assert!(level_1 > 0, "{m:?}");
    // Small flushes against a large level 1 join among themselves in level 0, and level 1 is
    // not rewritten: sixteen flushes leave a few files, one of them several flushes big.
    let mut largest_flush = 0;
    for round in 0..16 {
        flush(&mut lsm, &mut disk, &mut index, round * 50..round * 50 + 50, 2);
        let m = lsm.metrics();
        assert_eq!(m.level_bytes[1], level_1, "round {round}: {m:?}");
        assert!(m.level_files[0] < cfg.l0_trigger * 2, "round {round}: {m:?}");
        if round == 0 {
            largest_flush = m.level_bytes[0];
        }
    }
    let m = lsm.metrics();
    assert!(m.level_files[0] <= 4, "{m:?}");
    assert!(m.level_bytes[0] > 4 * largest_flush, "{m:?}");
    assert!(m.compactions_total >= 5, "{m:?}");
    // Level 0 as heavy as level 1: the next tier goes down.
    for round in 0..40 {
        flush(&mut lsm, &mut disk, &mut index, round * 100..round * 100 + 100, 4);
    }
    let m = lsm.metrics();
    assert!(m.level_bytes[1] != level_1, "level 1 never took level 0: {m:?}");
    assert!(m.level_files[0] < cfg.l0_slow, "{m:?}");
    // Every key reads its newest version.
    let view = lsm.view();
    for i in [0, 49, 799, 3999] {
        let row = get(&mut lsm, &mut disk, &view, &key(i), ts(10, 0)).expect("a version");
        assert_eq!(row.1, ts(4, 0), "key {i}");
    }
    assert_eq!(get(&mut lsm, &mut disk, &view, &key(3999), ts(1, 0)).map(|r| r.1), Some(ts(1, 0)));
}

/// A tier is never joined above a newer file: a version written later always wins, also after
/// a restart, whatever the tiers did in between.
#[test]
fn random_histories_read_as_the_model_through_the_tiers() {
    for seed in 1..=6 {
        let mut disk = MemDisk::new();
        let cfg = LsmConfig { memtable_bytes: 8 << 10, ..cfg() };
        let mut lsm = open(&cfg, &mut disk);
        let m = common::history(seed, 3000, &mut lsm, &mut disk);
        // The memtable is not durable on its own (the log replays it): it goes to a file first.
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
        drop(lsm);
        let mut lsm = open(&cfg, &mut disk);
        check_all(&mut lsm, &mut disk, &m, 1_000_000);
    }
}
