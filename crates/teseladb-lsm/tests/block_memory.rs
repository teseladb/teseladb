//! The memory the block caches keep their blocks in (012 FR-032): after `Lsm::place_blocks`,
//! every block a read brings into the core's cache goes through it, and a read served from the
//! cache returns the placed bytes.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{get, open, small, ts};
use teseladb_lsm::BlockMemory;
use teseladb_raftlog::MemDisk;
use teseladb_types::{Bytes, GroupId, Nanos};

/// Copies each block into a buffer of its own and counts them.
#[derive(Default)]
struct Counting(AtomicUsize);

impl BlockMemory for Counting {
    fn place(&self, block: Bytes) -> Bytes {
        self.0.fetch_add(1, Ordering::Relaxed);
        Bytes::from_owner(block.to_vec())
    }
}

#[test]
fn blocks_read_into_the_cache_go_through_the_memory_it_was_given() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let memory = Arc::new(Counting::default());
    lsm.place_blocks(memory.clone());
    lsm.put(GroupId(1), 1, b"a", ts(1, 0), &[7; 1000]).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let placed_by_flush = memory.0.load(Ordering::Relaxed);
    let v = lsm.view();
    let first = get(&mut lsm, &mut disk, &v, b"a", ts(9, 0)).unwrap();
    let second = get(&mut lsm, &mut disk, &v, b"a", ts(9, 0)).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.3, vec![7; 1000]);
    assert!(memory.0.load(Ordering::Relaxed) > placed_by_flush, "the block read was placed");
}
