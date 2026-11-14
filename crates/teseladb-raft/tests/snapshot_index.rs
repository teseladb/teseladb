//! The index of the latest snapshot comes from [`LogStore::snapshot_index`], which a store can
//! answer without building the whole [`SnapshotMeta`]: a store that keeps its configuration
//! encoded decodes it only for a snapshot that is sent or installed.

mod common;

use std::cell::Cell;

use common::*;
use teseladb_raft::{Entry, LogGap, LogStore, MemLogStore, SnapshotMeta};

/// A store that counts how many times its whole snapshot metadata is built.
struct Counting {
    inner: MemLogStore,
    metas: Cell<u32>,
}

impl LogStore for Counting {
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<Vec<Entry>, LogGap> {
        self.inner.entries(lo, hi, max_bytes)
    }

    fn term(&self, index: u64) -> Result<u64, LogGap> {
        self.inner.term(index)
    }

    fn first_index(&self) -> u64 {
        self.inner.first_index()
    }

    fn last_index(&self) -> u64 {
        self.inner.last_index()
    }

    fn snapshot_meta(&self) -> SnapshotMeta {
        self.metas.set(self.metas.get() + 1);
        self.inner.snapshot_meta()
    }

    fn snapshot_index(&self) -> u64 {
        self.inner.snapshot_index()
    }
}

#[test]
fn the_status_and_the_log_bounds_take_only_the_snapshot_index() {
    let mut net = Net::new(3, 0, 31);
    let l = net.elect(300);
    net.propose(l, b"v").expect("a proposal");
    net.ticks(3);
    let node = &net.nodes[&l];
    let store = Counting { inner: node.store.clone(), metas: Cell::new(0) };
    let status = node.raw.status(&store);
    assert_eq!(status.last_index, node.store.last_index());
    assert_eq!(store.metas.get(), 0, "the status builds no snapshot metadata");
}

#[test]
fn the_default_snapshot_index_is_the_index_of_the_metadata() {
    let store = MemLogStore {
        snapshot: SnapshotMeta { index: 7, term: 2, ..SnapshotMeta::default() },
        entries: Vec::new(),
    };
    assert_eq!(store.snapshot_index(), 7);
}
