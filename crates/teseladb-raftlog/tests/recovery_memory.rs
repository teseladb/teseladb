//! Recovery reads the WAL one segment at a time (production, 10-11-2026: a node with 6,4 GB of WAL,
//! almost all of it the padding of small batches, ran out of memory while starting because
//! recovery held every segment in memory at once). Measured with a test allocator that keeps the
//! peak of live bytes.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{CoreStore, Disk, LogEntry, MemDisk, StorageConfig};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

struct Peak;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn note(grown: usize) {
    let live = LIVE.fetch_add(grown, Ordering::Relaxed) + grown;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: delegates to `System` without changing anything; it only counts the live bytes.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as `GlobalAlloc::alloc`, which the caller upholds.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` was allocated by `System` with this `layout` (every allocation goes through here).
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        note(new_size);
        // SAFETY: same contract as `GlobalAlloc::realloc`, which the caller upholds.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Peak = Peak;

const SEGMENT: u64 = 1 << 20;
/// The most recovery may hold at once: a few segments and the records it keeps.
const MOST: usize = 6 << 20;

#[test]
fn recovery_holds_a_few_segments_in_memory_not_the_whole_wal() {
    let cfg = StorageConfig {
        wal_segment_size: SEGMENT,
        max_batch_bytes: 32 << 10,
        ..StorageConfig::default()
    };
    let mut disk = MemDisk::new();
    let (mut store, _) = CoreStore::recover(&cfg, 0, &mut disk).expect("an empty core");
    let g = GroupId(1);
    store.create_group(g, Bytes::new(), Durability::Strict).expect("group");
    let _ = store.run_io(&mut disk, Nanos::ZERO);
    // One small entry per batch, each batch padded to its block, and no snapshot: the WAL grows
    // to some 24 segments and none can be released.
    for index in 1..=6_000u64 {
        let entry = LogEntry {
            index,
            term: 1,
            kind: EntryKind::Normal,
            data: Bytes::from_vec(vec![7; 64]),
        };
        store.append_raft(g, &[entry], None, Durability::Strict).expect("append");
        let _ = store.run_io(&mut disk, Nanos::ZERO);
    }
    drop(store);
    let dir = teseladb_raftlog::paths::wal_dir(0);
    let segments = disk.list(&dir).expect("the WAL");
    let wal: u64 = segments.iter().map(|p| disk.read_all(p).expect("a segment").len() as u64).sum();
    assert!(wal >= 20 * SEGMENT, "the WAL has many segments: {wal} bytes");
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let (store, _) = CoreStore::recover(&cfg, 0, &mut disk).expect("recovers");
    let peak = PEAK.load(Ordering::Relaxed) - before;
    assert_eq!(store.group(g).expect("the group").last_index(), 6_000);
    assert!(peak < MOST, "recovery held {peak} bytes for a WAL of {wal}");
}
