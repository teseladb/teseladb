//! Large allocations of the engine allocator of up to `MAX_TRIMMED_SPAN_PAGES` take the pages
//! their size needs, not that count rounded up to a power of two (a block of a little more
//! than 16 KiB took 32 KiB, so a node's block cache of such blocks held about twice the memory
//! it counted), and the runs of pages they leave merge back into the spans they came from when
//! they are freed, shrunk or freed by another shard.

#![cfg(target_os = "linux")]
// This file calls the allocator's C entry points directly; every `unsafe` block states why it
// is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
}

const PAGE: usize = 4096;

/// A live block of the stress below: its address, its size and the byte it is filled with.
#[derive(Clone, Copy)]
struct Block {
    addr: usize,
    size: usize,
    fill: u8,
}

/// A small xorshift generator: the stress must not allocate to draw a number.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A draw in `[lo, hi]`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo + 1) as u64) as usize
    }
}

/// Allocates `size` bytes and fills them with `fill`.
fn alloc_filled(size: usize, fill: u8) -> Block {
    // SAFETY: malloc has no preconditions.
    let p = unsafe { malloc(size) } as *mut u8;
    assert!(!p.is_null(), "malloc({size}) failed");
    // SAFETY: `p` is a fresh block of `size` bytes.
    unsafe { std::ptr::write_bytes(p, fill, size) };
    Block { addr: p as usize, size, fill }
}

/// Checks that `b` still holds its fill byte.
fn check(b: &Block, ctx: impl std::fmt::Display) {
    // SAFETY: a live block of `b.size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(b.addr as *const u8, b.size) };
    assert!(bytes.iter().all(|&x| x == b.fill), "{ctx}: block at {:#x} overwritten", b.addr);
}

/// Checks that `b` overlaps none of `live`.
fn disjoint(b: &Block, live: &[Block], ctx: impl std::fmt::Display) {
    for o in live {
        assert!(
            b.addr + b.size <= o.addr || o.addr + o.size <= b.addr,
            "{ctx}: block {:#x}+{} overlaps {:#x}+{}",
            b.addr,
            b.size,
            o.addr,
            o.size
        );
    }
}

/// The largest power-of-two block this shard can allocate now, in bytes (freed again).
fn largest_block() -> usize {
    let mut size = memory::stats().total_memory().next_power_of_two();
    loop {
        // SAFETY: malloc has no preconditions; the block is freed at once.
        let p = unsafe { malloc(size) };
        if !p.is_null() {
            // SAFETY: a live block.
            unsafe { free(p) };
            return size;
        }
        size /= 2;
        assert!(size > PAGE, "not even a page can be allocated");
    }
}

// Blocks of sizes just past a power of two, which the reference rounds up to the next one,
// and of other page counts: each block of up to MAX_TRIMMED_SPAN_PAGES takes exactly its
// pages, a larger one its power of two, and freeing them gives every page back.
engine_test_case!(a_large_block_takes_its_pages_not_a_power_of_two_of_them, {
    const COUNT: usize = 32;
    let sizes: [usize; 7] =
        [16 * 1024 + 16, 20 * 1024 + 1, 40_000, 100_000, 128 * 1024, 300_000, (1 << 20) + 1];
    // memory::MAX_TRIMMED_SPAN_PAGES, written out so that the test also builds against an
    // allocator without it (where it fails, every block taking its power of two).
    let limit = 32;
    let mut blocks = Vec::with_capacity(COUNT);
    for size in sizes {
        let pages = size.div_ceil(PAGE);
        let pages = if pages <= limit { pages } else { pages.next_power_of_two() };
        let free_before = memory::free_memory();
        for i in 0..COUNT {
            blocks.push(alloc_filled(size, i as u8));
        }
        for b in &blocks {
            assert_eq!(memory::object_size(b.addr as *mut c_void), pages * PAGE, "size {size}");
        }
        let used = free_before - memory::free_memory();
        assert_eq!(used, COUNT * pages * PAGE, "size {size}: {used} bytes for {COUNT} blocks");
        for b in blocks.drain(..) {
            check(&b, "before free");
            // SAFETY: a live block, freed once.
            unsafe { free(b.addr as *mut c_void) };
        }
        assert_eq!(memory::free_memory(), free_before, "size {size}: pages not given back");
    }
    make_ready_future(())
});

// A random mix of allocations, frees and reallocations (smaller in place, larger by a copy)
// of every page count from 5 to 512: the blocks stay intact and disjoint, and once all are
// freed the shard has its free memory back and its largest block can be allocated again (the
// runs merged back into their buddy spans).
engine_test_case!(freed_and_shrunk_runs_merge_back_into_their_spans, {
    const LIVE: usize = 96;
    const OPS: usize = 6000;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut live: Vec<Block> = Vec::with_capacity(LIVE);
    let largest = largest_block();
    let free_before = memory::free_memory();
    for op in 0..OPS {
        // Nothing in the loop allocates but the blocks (a message is only formatted on a
        // failure), so the free memory at the end is comparable.
        let ctx = op;
        let choice = rng.range(0, 9);
        if live.len() < LIVE && (choice < 4 || live.is_empty()) {
            let size = rng.range(16 * 1024 + 1, 2 << 20);
            let b = alloc_filled(size, rng.next() as u8);
            disjoint(&b, &live, ctx);
            live.push(b);
        } else if choice < 7 {
            let b = live.swap_remove(rng.range(0, live.len() - 1));
            check(&b, ctx);
            // SAFETY: a live block, freed once.
            unsafe { free(b.addr as *mut c_void) };
        } else {
            let i = rng.range(0, live.len() - 1);
            let b = live[i];
            check(&b, ctx);
            let size = if choice == 7 {
                rng.range(16 * 1024 + 1, b.size)
            } else {
                rng.range(b.size, 2 << 20)
            };
            // SAFETY: a live block; on success the old address is no longer used.
            let p = unsafe { realloc(b.addr as *mut c_void, size) } as usize;
            assert_ne!(p, 0, "{ctx}: realloc to {size} failed");
            if size <= b.size {
                assert_eq!(p, b.addr, "{ctx}: a smaller size moved the block");
            }
            let kept = Block { addr: p, size: size.min(b.size), fill: b.fill };
            check(&kept, ctx);
            // SAFETY: the block holds `size` bytes.
            unsafe { std::ptr::write_bytes(p as *mut u8, b.fill, size) };
            live.swap_remove(i);
            let b = Block { addr: p, size, fill: b.fill };
            disjoint(&b, &live, ctx);
            live.push(b);
        }
    }
    for b in live.drain(..) {
        check(&b, "end");
        // SAFETY: a live block, freed once.
        unsafe { free(b.addr as *mut c_void) };
    }
    assert_eq!(memory::free_memory(), free_before, "pages lost");
    assert_eq!(largest_block(), largest, "the spans did not merge back");
    make_ready_future(())
});

// Blocks of odd page counts allocated on shard 1 and freed here go back through shard 1's
// cross-shard list: once it drains it, shard 1 has all its pages back.
engine_test_case!(large_blocks_freed_by_another_shard_give_their_pages_back, {
    if Smp::count() < 2 {
        return make_ready_future(());
    }
    thread_async(|| {
        let (addrs, free_before) = Smp::submit_to(1, || {
            memory::drain_cross_cpu_freelist();
            let free_before = memory::free_memory();
            let addrs: Vec<(usize, usize)> = (0..16)
                .map(|i| {
                    let size = (5 + 2 * i) * PAGE + 1;
                    (alloc_filled(size, i as u8).addr, size)
                })
                .collect();
            make_ready_future((addrs, free_before))
        })
        .get()?;
        for (i, &(a, size)) in addrs.iter().enumerate() {
            check(&Block { addr: a, size, fill: i as u8 }, "cross-shard");
            // SAFETY: a live block of shard 1, freed once.
            unsafe { free(a as *mut c_void) };
        }
        let free_after = Smp::submit_to(1, || {
            memory::drain_cross_cpu_freelist();
            make_ready_future(memory::free_memory())
        })
        .get()?;
        assert_eq!(free_after, free_before, "shard 1 did not get its pages back");
        Ok(())
    })
});
