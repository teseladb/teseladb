//! The block caches let go of the blocks of the files a compaction replaced as soon as the
//! version moves on, not when the last reader of an older view lets the files go: an old view
//! held by a read under way kept every block of the files it held in both caches until then, and
//! the memory the caches held grew with the files compactions left behind (resident memory of
//! the one-node load, H-013). A reader of the old view still reads what it needs, from the disk.

mod common;

use common::{get, open, small, ts};
use teseladb_lsm::{Lsm, LsmConfig, SharedLsm, SharedRead, SharedView};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const KEYS: u32 = 300;

fn cfg() -> LsmConfig {
    LsmConfig { cache_bytes: 8 << 20, ..small() }
}

fn key(k: u32) -> Vec<u8> {
    format!("k{k:05}").into_bytes()
}

/// Rounds of every key, each flushed to files.
fn write(lsm: &mut Lsm, disk: &mut MemDisk, rounds: u64) {
    let mut index = 0;
    for round in 1..=rounds {
        for k in 0..KEYS {
            index += 1;
            let value = vec![(round as u8).wrapping_mul(7) ^ (k as u8); 200];
            lsm.put(G, index, &key(k), ts(round, 0), &value).unwrap();
        }
        lsm.flush();
        lsm.run_io(disk, Nanos::ZERO);
    }
}

/// The point read of `key` from another thread's side, supplying the blocks it misses.
fn shared_get(shared: &SharedLsm, view: &SharedView, disk: &mut MemDisk, k: &[u8]) -> Vec<u8> {
    loop {
        match view.get(shared, k, ts(100, 0)) {
            SharedRead::Hit(f) => return f.value.to_vec(),
            SharedRead::NotFound => panic!("{k:?} not found"),
            SharedRead::Need(need) => {
                let all = disk.read_all(std::path::Path::new(&need.path)).expect("the file");
                let at = usize::try_from(need.offset).expect("offset");
                let len = usize::try_from(need.len).expect("length");
                shared.supply(&need, &all[at..at + len]).expect("a good block");
            }
            SharedRead::Failed(e) => panic!("read failed: {e}"),
        }
    }
}

/// Compacts everything and runs the I/O until it is done.
fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    let id = lsm.compact_now();
    for s in 1..60 {
        if lsm.run_io(disk, Nanos::from_secs(s)).compacted.contains(&id) {
            return;
        }
    }
    panic!("the compaction never finished");
}

#[test]
fn a_compaction_drops_the_blocks_of_the_files_it_replaced_from_both_caches() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg(), &mut disk);
    write(&mut lsm, &mut disk, 3);
    // Every block in both caches: the core's reads and those of another thread.
    let view = lsm.view();
    lsm.publish();
    let shared = lsm.shared();
    let old = shared.view();
    for k in 0..KEYS {
        assert!(get(&mut lsm, &mut disk, &view, &key(k), ts(100, 0)).is_some());
        shared_get(&shared, &old, &mut disk, &key(k));
    }
    let before = lsm.metrics();
    assert!(before.shared_cache_bytes > 0 && before.cache_bytes > 0);
    assert_eq!(before.retired_cache_bytes, 0, "every cached block is of the current version");
    // Both views still hold the old files: the compaction cannot delete them yet.
    compact(&mut lsm, &mut disk);
    lsm.publish();
    let after = lsm.metrics();
    assert!(after.unused_file_bytes > 0, "the old views hold the replaced files");
    assert_eq!(after.retired_cache_bytes, 0, "their blocks left both caches at once");
    assert!(after.shared_cache_bytes < before.shared_cache_bytes / 4, "{after:?}");
    // The old views still read, from the disk; the core's cache does not take their blocks back.
    for k in (0..KEYS).step_by(37) {
        let found = get(&mut lsm, &mut disk, &view, &key(k), ts(100, 0)).expect("a value").3;
        assert_eq!(found, shared_get(&shared, &old, &mut disk, &key(k)));
    }
    let m = lsm.metrics();
    assert!(m.retired_cache_bytes <= m.shared_cache_bytes, "only the shared cache took any back");
    // Once the views go, the files go, and with them whatever blocks of them were put back.
    drop((view, old));
    lsm.run_io(&mut disk, Nanos::from_secs(100));
    let m = lsm.metrics();
    assert_eq!((m.unused_file_bytes, m.retired_cache_bytes), (0, 0));
}
