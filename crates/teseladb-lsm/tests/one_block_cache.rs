//! The core and the readers on other threads keep their data blocks in one cache: a block the
//! core read is there for a reader, and the other way round, and the core's own cache holds the
//! open files' meta bytes only. Before, each kept its own copy of the hot blocks (the core's
//! cache, a quarter of the budget, and the shared one, the rest).

mod common;

use common::{get, open, small, ts};
use teseladb_lsm::{Lsm, LsmConfig, SharedRead};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u32 = 300;

fn key(k: u32) -> Vec<u8> {
    format!("k{k:05}").into_bytes()
}

fn written(cfg: &LsmConfig, disk: &mut MemDisk) -> Lsm {
    let mut lsm = open(cfg, disk);
    for k in 0..KEYS {
        lsm.put(G, u64::from(k) + 1, &key(k), ts(1, 0), &[k as u8; 200]).unwrap();
    }
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
    lsm.publish();
    lsm
}

#[test]
fn a_block_the_core_read_is_cached_once_for_every_reader() {
    let mut disk = MemDisk::new();
    let mut lsm = written(&LsmConfig { cache_bytes: 8 << 20, ..small() }, &mut disk);
    let meta = lsm.metrics().cache_bytes;
    let view = lsm.view();
    for k in 0..KEYS {
        assert!(get(&mut lsm, &mut disk, &view, &key(k), ts(5, 0)).is_some());
    }
    let m = lsm.metrics();
    assert_eq!(m.cache_bytes, meta, "the core's own cache keeps the meta bytes only");
    assert!(m.shared_cache_bytes > 0);
    // A reader on another thread finds every block there: it asks the disk for none.
    let shared = lsm.shared();
    let other = shared.view();
    for k in 0..KEYS {
        match other.get(&shared, &key(k), ts(5, 0)) {
            SharedRead::Hit(f) => assert_eq!(f.value.to_vec(), vec![k as u8; 200]),
            r => panic!("{k}: {r:?}, not a hit"),
        }
    }
    // And the core finds them again, as hits.
    let misses = m.cache_misses_total;
    for k in 0..KEYS {
        assert!(get(&mut lsm, &mut disk, &view, &key(k), ts(5, 0)).is_some());
    }
    let again = lsm.metrics();
    assert_eq!(again.cache_misses_total, misses, "no block read twice");
    assert!(again.cache_hits_total > m.cache_hits_total);
}

#[test]
fn a_small_budget_keeps_the_cores_blocks_in_its_own_cache() {
    // Shards of a few hundred bytes would hold no block: the core keeps its own.
    let mut disk = MemDisk::new();
    let mut lsm = written(&small(), &mut disk);
    let meta = lsm.metrics().cache_bytes;
    let view = lsm.view();
    for k in 0..KEYS {
        assert!(get(&mut lsm, &mut disk, &view, &key(k), ts(5, 0)).is_some());
    }
    assert!(lsm.metrics().cache_bytes > meta);
}
