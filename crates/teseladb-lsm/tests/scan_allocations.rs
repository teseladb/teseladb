//! Allocations of a scan (023 FR-012): a scan over blocks already in the cache gives each version
//! with at most one allocation, its key, and the value borrows the block it was read from.

// The counting allocator wraps the system's, as in the pipeline's `group_allocations.rs`.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{open, small, ts};
use teseladb_lsm::Step;
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

/// Counts the allocations of the thread that asks for them.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator unchanged; the count is a thread-local cell
// that allocates nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const KEYS: u32 = 4096;

fn key(i: u32) -> Vec<u8> {
    format!("doc-{i:08}").into_bytes()
}

/// Versions a whole scan gives, and the allocations it made, with the I/O it waits for run
/// outside the count.
fn counted_scan(
    lsm: &mut teseladb_lsm::Lsm,
    disk: &mut MemDisk,
    at: teseladb_keys::Hlc,
) -> (u64, u64, u64) {
    let view = lsm.view();
    let mut s = view.scan(b"doc-", b"doc.", at);
    let (mut items, mut bytes, mut made) = (0u64, 0u64, 0u64);
    loop {
        let before = allocations();
        let step = s.next(lsm);
        made += allocations() - before;
        match step {
            Step::Item(i) => {
                items += 1;
                bytes += i.value.len() as u64;
            }
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return (items, bytes, made),
            Step::Failed(e) => panic!("scan failed: {e}"),
        }
    }
}

#[test]
fn a_scan_over_cached_blocks_allocates_at_most_once_per_version() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..KEYS {
        // The first byte of a value is its type: none of these is one that only serves the
        // history, which no scan in key order gives.
        let fill = u8::try_from(i % 251).expect("a byte");
        let mut value = vec![fill; 150 + (i % 200) as usize];
        value[0] = 2;
        lsm.put(GroupId(1), u64::from(i) + 1, &key(i), ts(10, 0), &value).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    // The first scan brings the blocks into the cache; the second one is counted.
    let first = counted_scan(&mut lsm, &mut disk, ts(20, 0));
    let (items, bytes, made) = counted_scan(&mut lsm, &mut disk, ts(20, 0));
    assert_eq!((items, bytes), (first.0, first.1));
    assert_eq!(items, u64::from(KEYS));
    let per = made as f64 / items as f64;
    println!("{made} allocations for {items} versions: {per:.2} each");
    assert!(per <= 1.1, "{made} allocations for {items} versions");
}
