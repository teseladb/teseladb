//! Allocations on the hot path, counted with a test allocator (per thread, so that other tests do
//! not interfere). Reading entries kept in memory does not allocate; appending a small entry
//! allocates a bounded amount (the entry itself, its WAL record and amortized growth of vectors).
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{LogEntry, MemDisk};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: delegates to `System` without changing anything; it only counts the calling thread's calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as `GlobalAlloc::alloc`, which the caller upholds.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with this `layout` (every allocation goes through here).
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as `GlobalAlloc::realloc`, which the caller upholds.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

fn entry(index: u64) -> LogEntry {
    LogEntry { index, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![7; 200]) }
}

#[test]
fn hot_path_allocations_are_bounded() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    // Warm-up: the group's log and the batch vectors already have their size.
    for i in 1..=10_000 {
        s.append_raft(g, &[entry(i)], None, Durability::Strict).unwrap();
        if i % 100 == 0 {
            settle(&mut s, &mut disk, &mut now);
        }
    }

    // Reading entries kept in memory: no allocation.
    let before = allocs();
    for i in 1..=10_000 {
        assert_eq!(s.entries(g, i, i + 1, u64::MAX).unwrap().len(), 1);
    }
    assert_eq!(allocs() - before, 0, "reading an entry kept in memory does not allocate");

    // Appending until durable: bounded and amortized.
    let before = allocs();
    let mut appends = 0u64;
    for i in 10_001..=20_000 {
        s.append_raft(g, &[entry(i)], None, Durability::Strict).unwrap();
        appends += 1;
        if i % 100 == 0 {
            settle(&mut s, &mut disk, &mut now);
        }
    }
    let per_append = (allocs() - before) as f64 / appends as f64;
    println!("allocations per append (including the batch and the test disk): {per_append:.2}");
    // Includes closing each batch and what `MemDisk` allocates to store the data (same thread): a
    // generous bound that trips if anything starts allocating per byte.
    assert!(per_append < 8.0, "{per_append} allocations per append");

    // Without closing batches: only `append_raft`.
    let before = allocs();
    for i in 20_001..=21_000 {
        s.append_raft(g, &[entry(i)], None, Durability::Strict).unwrap();
    }
    let per_append = (allocs() - before) as f64 / 1000.0;
    println!("allocations per append (the request only): {per_append:.3}");
    assert!(per_append < 4.0, "{per_append} allocations per append without closing batches");
}
