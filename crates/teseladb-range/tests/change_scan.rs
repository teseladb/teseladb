//! A change stream's read of a range whose history is larger than the block cache (014 FR-003):
//! the read goes on from where a missing block stopped it, instead of scanning again from the
//! first key. Found in the e2e battery of 04-11-2026: a read of a history of hundreds of MB on
//! disk never answered (each block it loaded pushed an earlier one out of the cache), the leader
//! missed its heartbeats meanwhile and the range went through hundreds of elections.

use std::cell::RefCell;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_range::changes::ChangeScan;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(7);
/// Versions written: about 1 MiB of history in 4 KiB blocks.
const VERSIONS: u64 = 1_000;
const VALUE_BYTES: usize = 1_000;
/// Most I/O rounds the read may take: a few per block of history.
const MAX_ROUNDS: usize = 5_000;

fn config() -> LsmConfig {
    LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 32 << 10,
        block_bytes: 4096,
        target_file_bytes: 64 << 10,
        l1_bytes: 256 << 10,
        // Room for two blocks only: the history does not fit.
        cache_bytes: 8 << 10,
        ..LsmConfig::default()
    }
}

#[test]
fn a_history_larger_than_the_cache_is_read_to_the_end() {
    let mut disk = MemDisk::new();
    let mut lsm = Lsm::recover(&config(), &mut disk).expect("recovery").0;
    let value = |i: u64| {
        let mut v = vec![1u8];
        v.extend(std::iter::repeat_n((i % 251) as u8, VALUE_BYTES));
        v
    };
    for i in 0..VERSIONS {
        let key = [&[1u8][..], &(i % 97).to_be_bytes()].concat();
        let ts = Hlc { physical: 1_000 + i, logical: 0 };
        lsm.put(G, i + 1, &key, ts, &value(i)).expect("written");
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    for _ in 0..100 {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let lsm = RefCell::new(lsm);
    let zero = Hlc { physical: 0, logical: 0 };
    let far = Hlc { physical: u64::MAX, logical: 0 };
    let mut scan = ChangeScan::new(&lsm, (&[1u8], &[2u8]), (zero, far));
    let mut rounds = 0;
    let got = loop {
        match scan.step(&lsm) {
            Some(r) => break r.expect("readable"),
            None => {
                rounds += 1;
                assert!(rounds < MAX_ROUNDS, "the read did not end in {MAX_ROUNDS} I/O rounds");
                lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
            }
        }
    };
    assert_eq!(got.rows.len() as u64, VERSIONS);
    assert!(rounds > 0, "the history should not fit the cache");
    // Every version once, in key order and, within a key, from the newest.
    let mut keys: Vec<(Vec<u8>, std::cmp::Reverse<Hlc>)> =
        got.rows.iter().map(|r| (r.key.clone(), std::cmp::Reverse(r.ts))).collect();
    let sorted = keys.clone();
    keys.sort();
    assert_eq!(keys, sorted);
}
