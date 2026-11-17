//! The pages a shard has touched stay close to the most memory it held at once: a workload
//! that keeps its live memory bounded (long-lived blocks of a cache, short-lived blocks and
//! transient buffers of several MiB) must not leave the shard with resident pages far past
//! that bound, nor so fragmented that a buffer of a few MiB can no longer be allocated (what
//! trimming every large span to its pages did: the free tails of the big buffers took the
//! cache's blocks, which kept the buffers' spans from merging back). Residency is read with
//! mincore(2) over the shard's memory.

#![cfg(target_os = "linux")]
// This file calls the allocator's C entry points and mincore(2) directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::c_void;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn mincore(addr: *mut c_void, length: usize, vec: *mut u8) -> i32;
}

const PAGE: usize = 4096;

/// A small xorshift generator: drawing a number must not allocate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo + 1) as u64) as usize
    }
}

/// Resident pages of the shard's memory, counted into `vec` (one byte per page).
fn resident_pages(layout: &memory::MemoryLayout, vec: &mut [u8]) -> usize {
    let len = layout.end - layout.start;
    assert!(vec.len() >= len / PAGE);
    // SAFETY: the range is the shard's mapping, page aligned, and `vec` has a byte per page.
    let r = unsafe { mincore(layout.start as *mut c_void, len, vec.as_mut_ptr()) };
    assert_eq!(r, 0, "mincore failed");
    vec[..len / PAGE].iter().filter(|&&b| b & 1 != 0).count()
}

/// Allocates `size` bytes and writes every page of them, as a user of the block would.
fn touched(size: usize) -> (usize, usize) {
    // SAFETY: malloc has no preconditions.
    let p = unsafe { malloc(size) } as *mut u8;
    if p.is_null() {
        println!("{}", memory::generate_memory_diagnostics_report().as_str());
    }
    assert!(!p.is_null(), "malloc({size}) failed");
    // SAFETY: `p` is a fresh block of `size` bytes.
    unsafe { std::ptr::write_bytes(p, 0x5a, size) };
    (p as usize, size)
}

fn release(b: (usize, usize)) {
    // SAFETY: a live block, freed once.
    unsafe { free(b.0 as *mut c_void) };
}

engine_test_case!(resident_pages_stay_near_the_live_peak, {
    const LONG: usize = 2000; // cache-like blocks of 16 to 40 KiB, about 56 MiB
    const SHORT: usize = 64; // short-lived blocks of 4 KiB to 1 MiB, about 32 MiB
    const BIG: usize = 2; // transient buffers of 1 to 32 MiB
    const OPS: usize = 60_000;
    let layout = memory::get_memory_layout().expect("the engine allocator's layout");
    let mut vec = vec![0u8; (layout.end - layout.start) / PAGE];
    let mut long: Vec<(usize, usize)> = Vec::with_capacity(LONG);
    let mut short: Vec<(usize, usize)> = Vec::with_capacity(SHORT);
    let mut big: Vec<(usize, usize)> = Vec::with_capacity(BIG);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let base_resident = resident_pages(&layout, &mut vec);
    let base_free = memory::free_memory();
    let mut peak_live = 0usize;
    let mut checkpoints = [0usize; 4];
    for op in 0..OPS {
        match rng.range(0, 99) {
            0..=59 => {
                if long.len() == LONG {
                    release(long.swap_remove(rng.range(0, LONG - 1)));
                }
                long.push(touched(rng.range(16 * 1024 + 1, 40 * 1024)));
            }
            60..=96 => {
                if short.len() == SHORT {
                    release(short.swap_remove(rng.range(0, SHORT - 1)));
                }
                short.push(touched(rng.range(PAGE, 1 << 20)));
            }
            _ => {
                if big.len() == BIG {
                    release(big.swap_remove(0));
                }
                big.push(touched(rng.range(1 << 20, 32 << 20)));
            }
        }
        peak_live = peak_live.max(base_free - memory::free_memory());
        if (op + 1) % (OPS / 4) == 0 {
            checkpoints[(op + 1) / (OPS / 4) - 1] = resident_pages(&layout, &mut vec) * PAGE;
        }
    }
    for b in long.drain(..).chain(short.drain(..)).chain(big.drain(..)) {
        release(b);
    }
    let mib = |b: usize| b >> 20;
    let grown: Vec<usize> =
        checkpoints.iter().map(|&r| mib(r.saturating_sub(base_resident * PAGE))).collect();
    println!(
        "live peak {} MiB, resident growth at each quarter {:?} MiB (shard {} MiB)",
        mib(peak_live),
        grown,
        mib(layout.end - layout.start)
    );
    let last = checkpoints[3].saturating_sub(base_resident * PAGE);
    assert!(
        last <= peak_live + peak_live / 2,
        "resident pages grew by {} MiB for a live peak of {} MiB (quarters {:?} MiB)",
        mib(last),
        mib(peak_live),
        grown
    );
    make_ready_future(())
});
