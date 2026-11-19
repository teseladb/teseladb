#![allow(unsafe_code)]
//! The blocks of stand-alone promise nodes that a thread keeps for reuse go back to the
//! allocator when the thread exits.
//!
//! A build with `--cfg engine_default_allocator` (the debug and sanitizer builds, as
//! the original's default-allocator build) allocates through the system allocator, which, unlike the
//! engine's per-shard memory, outlives the thread: blocks still cached when a thread exits
//! would leak (LeakSanitizer reported them after every engine that ran on a test thread).
//! The test counts the live bytes of the system allocator around a thread that leaves many
//! nodes in its cache. The engine's own allocator replaces the global allocator in the other
//! builds, so the test exists only in default-allocator builds.
#![cfg(all(target_os = "linux", engine_default_allocator))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use teseladb::engine::core::future::{Future, Promise};

/// The system allocator, counting the bytes live.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call forwards to the system allocator with the same arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Pending promises whose futures are taken at the same time: each needs a node of its own.
const NODES: usize = 1000;

#[test]
fn cached_node_blocks_are_freed_when_the_thread_exits() {
    let before = LIVE.load(Ordering::Relaxed);
    std::thread::spawn(|| {
        let mut promises: Vec<Promise<()>> = (0..NODES).map(|_| Promise::new()).collect();
        let futures: Vec<Future<()>> = promises.iter_mut().map(Promise::get_future).collect();
        for p in &mut promises {
            p.set_value(());
        }
        // Dropping both handles frees every node into this thread's cache.
        drop(futures);
        drop(promises);
    })
    .join()
    .unwrap();
    let leaked = LIVE.load(Ordering::Relaxed) - before;
    // One node block is at least 16 bytes; a thousand cached blocks would be well over this.
    assert!(leaked < 4096, "{leaked} bytes still allocated after the thread exited");
}
