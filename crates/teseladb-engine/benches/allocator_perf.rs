//! Microbenchmarks of the per-shard memory allocator (allocation and free of small and
//! large objects through the C and Rust allocation interfaces, in several patterns) and
//! of the random distributions used by allocation sampling.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

use std::alloc::{Layout, alloc, dealloc};
use std::ffi::c_void;

use teseladb::engine::core::memory::PAGE_SIZE;
use teseladb::engine::testing::perf_tests::perf_tests::{
    do_not_optimize, start_measuring_time, stop_measuring_time,
};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(p: *mut c_void);
}

const SMALL_ALLOC_SIZE: usize = 8;
const LARGE_ALLOC_SIZE: usize = 32000;
const MAX_POINTERS: usize = 1000;

/// `enum alloc_test_flags`
const MEASURE_ALLOC: u8 = 1;
const MEASURE_FREE: u8 = 2;
const MEASURE_BOTH: u8 = MEASURE_ALLOC | MEASURE_FREE;

/// The allocation functions a test measures.
trait AllocFuncs {
    fn alloc(size: usize) -> *mut c_void;
    fn free(p: *mut c_void, size: usize);
}

/// `malloc` / `free` of the C library.
struct CFuncs;

impl AllocFuncs for CFuncs {
    #[inline(always)]
    fn alloc(size: usize) -> *mut c_void {
        // SAFETY: plain malloc.
        unsafe { malloc(size) }
    }
    #[inline(always)]
    fn free(p: *mut c_void, _size: usize) {
        // SAFETY: `p` came from malloc.
        unsafe { free(p) }
    }
}

/// `alloc` / `dealloc` of the global allocator.
struct GlobalAllocFuncs;

impl AllocFuncs for GlobalAllocFuncs {
    #[inline(always)]
    fn alloc(size: usize) -> *mut c_void {
        // SAFETY: the sizes used are nonzero.
        unsafe { alloc(Layout::from_size_align_unchecked(size, 1)) as *mut c_void }
    }
    #[inline(always)]
    fn free(p: *mut c_void, size: usize) {
        // SAFETY: `p` came from alloc() with the same layout.
        unsafe { dealloc(p as *mut u8, Layout::from_size_align_unchecked(size, 1)) }
    }
}

/// A boxed, uninitialized byte slice.
struct BoxSliceFuncs;

impl AllocFuncs for BoxSliceFuncs {
    #[inline(always)]
    fn alloc(size: usize) -> *mut c_void {
        // The bytes are left uninitialized.
        let b: Box<[std::mem::MaybeUninit<u8>]> = Box::new_uninit_slice(size);
        Box::into_raw(b) as *mut u8 as *mut c_void
    }
    #[inline(always)]
    fn free(p: *mut c_void, size: usize) {
        // SAFETY: `p` is the pointer of a boxed slice of `size` bytes made by alloc().
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                p as *mut std::mem::MaybeUninit<u8>,
                size,
            )))
        }
    }
}

/// The fixture of the allocator benchmarks: the pointers a run allocates and then frees,
/// `MAX_POINTERS` at a time.
#[allow(non_camel_case_types)]
pub struct alloc_bench {
    pointers: [*mut c_void; MAX_POINTERS],
}

impl Default for alloc_bench {
    fn default() -> Self {
        alloc_bench { pointers: [std::ptr::null_mut(); MAX_POINTERS] }
    }
}

// SAFETY: the bench state is only used on the shard that runs it.
unsafe impl Send for alloc_bench {}

impl alloc_bench {
    #[inline(always)]
    fn alloc_test<const ALLOC_SIZE: usize, const F: u8, A: AllocFuncs>(&mut self) -> usize {
        // in some cases we want to measure allocation, on deallocation or
        // both, and this helper facilitates that with a minimum of fuss
        // it always executes fn on every pointer in the array, but only
        // measures when should_measure is true
        run_maybe_measure(&mut self.pointers, F & MEASURE_ALLOC != 0, |p| {
            *p = A::alloc(ALLOC_SIZE)
        });
        run_maybe_measure(&mut self.pointers, F & MEASURE_FREE != 0, |p| A::free(*p, ALLOC_SIZE));

        self.pointers.len()
    }
}

/// Runs `fn` over the elements, measuring time only when asked. The closure is a generic
/// parameter, so each call instantiates the loop with the closure inlined (a `&dyn Fn`
/// would make every element an indirect call).
#[inline(always)]
fn run_maybe_measure<F: Fn(&mut *mut c_void)>(
    pointers: &mut [*mut c_void; MAX_POINTERS],
    should_measure: bool,
    fn_: F,
) {
    if should_measure {
        start_measuring_time();
    }
    for p in pointers.iter_mut() {
        fn_(p);
    }
    if should_measure {
        stop_measuring_time();
    }
}

teseladb::perf_test_f!(alloc_bench, malloc_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_ALLOC, CFuncs>()
});
teseladb::perf_test_f!(alloc_bench, free_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_FREE, CFuncs>()
});
teseladb::perf_test_f!(alloc_bench, malloc_free, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_BOTH, CFuncs>()
});

teseladb::perf_test_f!(alloc_bench, op_new_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_ALLOC, GlobalAllocFuncs>()
});
teseladb::perf_test_f!(alloc_bench, op_delete_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_FREE, GlobalAllocFuncs>()
});
teseladb::perf_test_f!(alloc_bench, op_new_delete, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_BOTH, GlobalAllocFuncs>()
});

teseladb::perf_test_f!(alloc_bench, new_array_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_ALLOC, BoxSliceFuncs>()
});
teseladb::perf_test_f!(alloc_bench, delete_array_only, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_FREE, BoxSliceFuncs>()
});
teseladb::perf_test_f!(alloc_bench, array_new_delete, |s| -> usize {
    s.alloc_test::<SMALL_ALLOC_SIZE, MEASURE_BOTH, BoxSliceFuncs>()
});

teseladb::perf_test_f!(alloc_bench, alloc_only_large, |s| -> usize {
    s.alloc_test::<LARGE_ALLOC_SIZE, MEASURE_ALLOC, CFuncs>()
});
teseladb::perf_test_f!(alloc_bench, free_only_large, |s| -> usize {
    s.alloc_test::<LARGE_ALLOC_SIZE, MEASURE_FREE, CFuncs>()
});
teseladb::perf_test_f!(alloc_bench, alloc_free_large, |s| -> usize {
    s.alloc_test::<LARGE_ALLOC_SIZE, MEASURE_BOTH, CFuncs>()
});

// this test doesn't serve much value. It should take about 10 times as the
// single alloc test above. If not, something is wrong.
teseladb::perf_test_f!(alloc_bench, single_alloc_and_free_small_many, |s| -> () {
    const ALLOCS: usize = 10;
    const _: () = assert!(ALLOCS <= MAX_POINTERS);

    for i in 0..ALLOCS {
        // SAFETY: plain malloc.
        let ptr = unsafe { malloc(10) };
        do_not_optimize(&ptr);
        s.pointers[i] = ptr;
    }

    for i in 0..ALLOCS {
        // SAFETY: allocated above.
        unsafe { free(s.pointers[i]) };
    }
});

// Allocate from more than one page. Should also not suffer a perf penalty
// As we should have at least min free of 50 objects (hard coded internally)
teseladb::perf_test_f!(alloc_bench, single_alloc_and_free_small_many_cross_page, |s| -> () {
    const ALLOC_SIZE: usize = 1024;
    const ALLOCS: usize = (PAGE_SIZE / ALLOC_SIZE) + 1;
    const _: () = assert!(ALLOCS <= MAX_POINTERS);

    for i in 0..ALLOCS {
        // SAFETY: plain malloc.
        let ptr = unsafe { malloc(ALLOC_SIZE) };
        do_not_optimize(&ptr);
        s.pointers[i] = ptr;
    }

    for i in 0..ALLOCS {
        // SAFETY: allocated above.
        unsafe { free(s.pointers[i]) };
    }
});

// Include an allocation in the benchmark that will require going to the large pool
// for more data for the small pool
teseladb::perf_test_f!(
    alloc_bench,
    single_alloc_and_free_small_many_cross_page_alloc_more,
    |s| -> () {
        const ALLOC_SIZE: usize = 1024;
        const ALLOCS: usize = 101; // at 1024 alloc size we will have a _max_free of 100 objects

        for i in 0..ALLOCS {
            // SAFETY: plain malloc.
            let ptr = unsafe { malloc(ALLOC_SIZE) };
            do_not_optimize(&ptr);
            s.pointers[i] = ptr;
        }

        for i in 0..ALLOCS {
            // SAFETY: allocated above.
            unsafe { free(s.pointers[i]) };
        }
    }
);

/// The 64-bit Mersenne Twister (MT19937-64).
struct Mt19937_64 {
    mt: [u64; 312],
    idx: usize,
}

impl Mt19937_64 {
    fn new(seed: u64) -> Self {
        let mut mt = [0u64; 312];
        mt[0] = seed;
        for i in 1..312 {
            mt[i] = 6364136223846793005u64
                .wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 62))
                .wrapping_add(i as u64);
        }
        Mt19937_64 { mt, idx: 312 }
    }

    #[inline]
    fn next(&mut self) -> u64 {
        const UPPER: u64 = 0xFFFF_FFFF_8000_0000;
        const LOWER: u64 = 0x7FFF_FFFF;
        if self.idx >= 312 {
            for i in 0..312 {
                let x = (self.mt[i] & UPPER) | (self.mt[(i + 1) % 312] & LOWER);
                let mut xa = x >> 1;
                if x & 1 != 0 {
                    xa ^= 0xB502_6F5A_A966_19E9;
                }
                self.mt[i] = self.mt[(i + 156) % 312] ^ xa;
            }
            self.idx = 0;
        }
        let mut y = self.mt[self.idx];
        self.idx += 1;
        y ^= (y >> 29) & 0x5555_5555_5555_5555;
        y ^= (y << 17) & 0x71D6_7FFF_EDA6_0000;
        y ^= (y << 37) & 0xFFF7_EEE0_0000_0000;
        y ^= y >> 43;
        y
    }

    /// A uniform `f64` in `[0, 1)` from one 64-bit draw.
    #[inline]
    fn canonical(&mut self) -> f64 {
        let r = (self.next() as f64) / 18446744073709551616.0;
        if r >= 1.0 { f64::from_bits(1.0f64.to_bits() - 1) } else { r }
    }
}

/// The distributions `dist_bench` instantiates.
trait Dist {
    type Result: Default + std::ops::AddAssign + Copy;
    fn new(rate: usize) -> Self;
    fn sample(&self, g: &mut Mt19937_64) -> Self::Result;
}

/// The exponential distribution with rate `rate`.
struct ExpDist {
    lambda: f64,
}

impl Dist for ExpDist {
    type Result = f64;
    fn new(rate: usize) -> Self {
        ExpDist { lambda: rate as f64 }
    }
    #[inline]
    fn sample(&self, g: &mut Mt19937_64) -> f64 {
        -(1.0 - g.canonical()).ln() / self.lambda
    }
}

/// The geometric distribution with success probability `rate`, drawn as
/// `floor(log(u) / log(1 - p))`.
struct GeoDist {
    log_1_p: f64,
}

impl Dist for GeoDist {
    type Result = usize;
    fn new(rate: usize) -> Self {
        GeoDist { log_1_p: (1.0 - rate as f64).ln() }
    }
    #[inline]
    fn sample(&self, g: &mut Mt19937_64) -> usize {
        let naf = 1.0 - f64::EPSILON;
        let mut u;
        loop {
            u = g.canonical();
            if u < naf {
                break;
            }
        }
        ((1.0 - u).ln() / self.log_1_p).floor() as usize
    }
}

fn dist_bench<D: Dist>() -> usize {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let mut random_gen = Mt19937_64::new(seed);
    const ITERS: usize = 10000;
    let rate: usize = 3_000_000;
    do_not_optimize(&rate);
    let dist = D::new(rate);
    let mut sum = D::Result::default();
    for _ in 0..ITERS {
        sum += dist.sample(&mut random_gen);
    }
    do_not_optimize(&sum);
    ITERS
}

teseladb::perf_test!(random_sampling, exp_dist, -> usize { dist_bench::<ExpDist>() });

teseladb::perf_test!(random_sampling, geo_dist, -> usize { dist_bench::<GeoDist>() });

pub(crate) fn main() {
    malloc_only::register();
    free_only::register();
    malloc_free::register();
    op_new_only::register();
    op_delete_only::register();
    op_new_delete::register();
    new_array_only::register();
    delete_array_only::register();
    array_new_delete::register();
    alloc_only_large::register();
    free_only_large::register();
    alloc_free_large::register();
    single_alloc_and_free_small_many::register();
    single_alloc_and_free_small_many_cross_page::register();
    single_alloc_and_free_small_many_cross_page_alloc_more::register();
    exp_dist::register();
    geo_dist::register();
    perf_tests::main();
}
