//! The per-shard memory allocator and its diagnostics.
//!
//! The engine allocator splits system memory into one pool per shard. Memory allocated on a shard
//! should be freed on the same shard; freeing it elsewhere works but is much slower, and sharing
//! memory between cores should be limited to avoid cache-coherency traffic. The layout of the
//! current shard's memory is returned by [`get_memory_layout`]. The allocator serves `malloc` and
//! friends, the native runtime's `operator new`/`delete` symbols and, through `EngineAllocator` (on
//! Linux builds of the engine allocator), the Rust global allocator.
//!
//! ## Critical allocation scopes
//!
//! Scopes can be marked as critical allocation scopes, which memory utilities treat specially
//! (see `ScopedCriticalAllocSection` in `util::critical_alloc_section`).
//!
//! ## Diagnostics and debugging features
//!
//! ### Allocation failure injector
//!
//! Allocation failures can be injected to test resilience against them (see
//! `util::alloc_failure_injector`).
//!
//! ### Large allocation warning
//!
//! Large allocations put great pressure on the allocator, which may be unable to serve them even
//! when enough memory is free, because of fragmentation; this matters most for long-running
//! applications. A warning with the backtrace is logged for allocations above a threshold: see
//! [`set_large_allocation_warning_threshold`], [`get_large_allocation_warning_threshold`],
//! [`ScopedLargeAllocationWarningThreshold`] and [`ScopedLargeAllocationWarningDisable`].
//!
//! ### Heap profiling
//!
//! Heap profiling records the backtrace of a sampled subset (or all) of the allocations, to find
//! out how memory is used: see [`set_heap_profiling_sampling_rate`], [`sampled_memory_profile`] and
//! [`ScopedHeapProfiling`].
//!
//! ### Abort on allocation failure
//!
//! The best way to debug an allocation failure is often a core dump. With
//! `abort_on_engine_bad_alloc` in the reactor options (or the `--abort-on-engine-bad-alloc` flag),
//! or [`set_abort_on_allocation_failure`] at run time, a failed allocation aborts the process with
//! the failing stack.
//!
//! ### Diagnostics report
//!
//! On an allocation failure a report of the allocator's state is logged on the `engine_memory`
//! logger at debug level, or at error level for the failure kinds chosen with
//! `set_dump_memory_diagnostics_on_alloc_failure_kind` (also the
//! `--dump-memory-diagnostics-on-alloc-failure-kind` option); see also
//! `set_additional_diagnostics_producer` and `generate_memory_diagnostics_report`.
//!
//! ## Build configuration
//!
//! `cfg` flags (set with `--cfg`) select variants: `engine_default_allocator` uses the system
//! allocator instead (implied on non-Linux targets, where `mbind`, `MADV_DONTDUMP` and
//! `MAP_POPULATE` do not exist), `engine_heapprof` enables heap profiling, `engine_debug` turns
//! the allocator's internal debug assertions on, and `engine_debug_allocations` fills every new
//! block with the byte `0xab`. The allocation failure injector is enabled by the
//! `alloc_failure_injection` cargo feature; `engine_enable_alloc_failure_injection` only adds
//! the unused [`internal`] critical-section counter.
//!
//! ## Design
//!
//! This is a share-nothing allocator. Each shard's pool sits at an address that encodes the
//! shard number, `0x0000'sccc'vvvv'vvvv`:
//!
//! * `0000`: required by the architecture (48-bit address space);
//! * `s`: chosen to stay clear of the system allocator (1-7);
//! * `ccc`: the shard number (0-12 bits, depending on the system);
//! * `v`: the address within the shard's pool (32-44 bits, whatever `ccc` leaves).
//!
//! Each page has a page structure that describes it. Within a pool, the page array starts at
//! offset 0 and describes every page of the pool; page 0 is not a valid page. A pool has at most
//! 2^32 pages (44 address bits), so a 32-bit integer identifies a page. Runs of pages form spans;
//! free spans are kept in lists by size, and move between lists as they are split or coalesced.
//! Spans have power-of-two sizes and are naturally aligned (a buddy allocator). Small objects come
//! from per-size pools carved out of spans.
//!
//! With `engine_heapprof`, allocations are sampled at random and tracked; the sampled live set is
//! returned by [`sampled_memory_profile`]. A sampled large allocation keeps a pointer to its
//! [`AllocationSite`] in its page structure; sampled small allocations come from a separate set of
//! small pools and carry the site pointer in 8 extra bytes at their end, so a free only has to look
//! at the pool's tag.
const _MEMORY_MODULE_DOC: () = ();

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
use std::sync::atomic::{AtomicBool, AtomicU8};

use crate::engine::core::bitops::log2ceil_usize;
use crate::engine::core::future::{ExceptionPtr, make_exception_ptr};
use crate::engine::core::resource;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::backtrace::SimpleBacktrace;
use crate::engine::util::log::{LogLevel, Logger};
pub use crate::engine::util::memory_diagnostics::memory::AllocFailureKind;
use crate::engine::util::sampler::DisableSamplingTemporarily;

/// Returns `b`, hinting that it is usually true.
#[allow(dead_code)]
#[inline(always)]
fn likely(b: bool) -> bool {
    if !b {
        std::hint::cold_path();
    }
    b
}

/// Returns `b`, hinting that it is usually false.
#[allow(dead_code)]
#[inline(always)]
fn unlikely(b: bool) -> bool {
    if b {
        std::hint::cold_path();
    }
    b
}

/// The allocator's page size in bytes.
pub const ENGINE_INTERNAL_ALLOCATOR_PAGE_SIZE: usize = 4096;

/// The allocator's page size in bytes (the same as [`ENGINE_INTERNAL_ALLOCATOR_PAGE_SIZE`]).
pub const PAGE_SIZE: usize = ENGINE_INTERNAL_ALLOCATOR_PAGE_SIZE;
/// `log2(PAGE_SIZE)`.
pub const PAGE_BITS: usize = log2ceil_usize(PAGE_SIZE) as usize;
/// The size of a transparent huge page on this architecture.
#[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "s390x"))]
pub const HUGE_PAGE_SIZE: usize = 1 << 21; // 2M
/// The size of a transparent huge page on this architecture.
#[cfg(target_arch = "aarch64")]
pub const HUGE_PAGE_SIZE: usize = 1 << 21; // 2M
/// The size of a transparent huge page on this architecture.
#[cfg(any(target_arch = "powerpc", target_arch = "powerpc64"))]
pub const HUGE_PAGE_SIZE: usize = 1 << 24; // 16M
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "x86",
    target_arch = "s390x",
    target_arch = "aarch64",
    target_arch = "powerpc",
    target_arch = "powerpc64"
)))]
compile_error!("Huge page size is not defined for this architecture");

/// Memory layout types used by the SMP start-up code.
pub mod internal {
    /// A range of a shard's memory on one NUMA node.
    #[derive(Debug, Clone, Copy)]
    pub struct MemoryRange {
        /// First byte of the range.
        pub start: *mut u8,
        /// One past the last byte.
        pub end: *mut u8,
        /// The NUMA node the range is bound to.
        pub numa_node_id: u32,
    }

    /// The memory ranges of one or more shards.
    #[derive(Debug, Clone, Default)]
    pub struct NumaLayout {
        /// The ranges, in no particular order.
        pub ranges: Vec<MemoryRange>,
    }

    /// Returns the ranges of both layouts together.
    pub fn merge(mut one: NumaLayout, two: NumaLayout) -> NumaLayout {
        // Ranges of different shards never touch, so they are concatenated, not merged.
        one.ranges.extend(two.ranges);
        one
    }

    #[cfg(engine_enable_alloc_failure_injection)]
    thread_local! {
        /// A per-thread nesting counter for critical allocation sections. Nothing reads or
        /// writes it: the critical sections use the failure injector's own counter.
        pub static CRITICAL_ALLOC_SECTION: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    }

    /// Logs the current shard's diagnostics report at level `lvl` on the `engine_memory`
    /// logger, with no rate limit; logs nothing without the engine allocator.
    pub fn log_memory_diagnostics_report(lvl: crate::engine::util::log::LogLevel) {
        super::log_memory_diagnostics_report_impl(lvl);
    }
}

// `configure` and `configure_minimal` are defined below, for both allocator builds.

/// Same as [`set_abort_on_allocation_failure`]`(true)`.
#[deprecated(note = "use set_abort_on_allocation_failure(true) instead")]
pub fn enable_abort_on_allocation_failure() {
    set_abort_on_allocation_failure(true);
}

/// The `engine_memory` logger of the allocator's diagnostics.
///
/// It exists in every build, but only logs when the engine allocator is in use.
pub static ENGINE_MEMORY_LOGGER: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("engine_memory").expect("logger registered once"));

thread_local! {
    static ABORT_ON_ALLOC_FAILURE_SUPPRESSED: Cell<i32> = const { Cell::new(0) };
}

/// While alive, an allocation failure on this thread does not abort the process even if aborting
/// is enabled. Can be nested.
pub struct DisableAbortOnAllocFailureTemporarily {
    _private: (),
}

impl DisableAbortOnAllocFailureTemporarily {
    /// Suppresses aborting until the returned guard is dropped.
    pub fn new() -> Self {
        ABORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.set(c.get() + 1));
        Self { _private: () }
    }
}

impl Default for DisableAbortOnAllocFailureTemporarily {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DisableAbortOnAllocFailureTemporarily {
    fn drop(&mut self) {
        ABORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.set(c.get() - 1));
    }
}

/// Disables heap profiling on this thread while alive. Can be nested: profiling resumes when the
/// last guard is dropped.
pub struct DisableBacktraceTemporarily {
    _disable_sampling: DisableSamplingTemporarily,
}

impl Default for DisableBacktraceTemporarily {
    fn default() -> Self {
        Self::new()
    }
}

/// What a reclaimer achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReclaimingResult {
    /// No memory was released.
    ReclaimedNothing,
    /// Some memory was released.
    ReclaimedSomething,
}

/// When a reclaimer may be invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReclaimerScope {
    /// Only in its own task, which runs with higher priority than the application's tasks.
    Async,
    /// Synchronously, from inside an allocation, as well as in the asynchronous task.
    ///
    /// The reclaimer may allocate, though it is discouraged because memory is low and the
    /// allocation may fail; a reclaimer that allocates must be prepared for re-entry.
    Sync,
}

/// What a reclaimer is asked to release.
#[derive(Debug, Clone, Copy)]
pub struct ReclaimerRequest {
    /// The number of bytes needed. The reclaimer may release a different amount; if it releases
    /// less it may be invoked again.
    pub bytes_to_reclaim: usize,
}

/// A reclaim function that does not look at the request.
pub type ReclaimFn = Box<dyn FnMut() -> ReclaimingResult>;

/// A function the allocator calls to release memory when the shard runs low.
///
/// A reclaimer registers its own address with the current shard's allocator, so it lives in a
/// `Box` and never moves; dropping it unregisters it, and it must be dropped on the thread that
/// created it. Synchronous reclaimers are called from inside the allocation that ran short and
/// from the asynchronous reclaim task; asynchronous ones only from that task, which the
/// [`ReclaimHook`] schedules. Without the engine allocator a reclaimer is never registered nor
/// called.
pub struct Reclaimer {
    _reclaim: Box<dyn FnMut(ReclaimerRequest) -> ReclaimingResult>,
    _scope: ReclaimerScope,
}

impl Reclaimer {
    /// Installs on the current shard a reclaimer that ignores the request; `scope` says when it
    /// may run. Same as [`Reclaimer::with_request`] otherwise.
    pub fn new(mut reclaim: ReclaimFn, scope: ReclaimerScope) -> Box<Reclaimer> {
        Self::with_request(Box::new(move |_req: ReclaimerRequest| reclaim()), scope)
    }

    /// Calls the reclaim function with a request for `bytes_to_reclaim` bytes and returns its
    /// result.
    pub fn do_reclaim(&mut self, bytes_to_reclaim: usize) -> ReclaimingResult {
        (self._reclaim)(ReclaimerRequest { bytes_to_reclaim })
    }

    /// Returns when the reclaimer may be invoked.
    pub fn scope(&self) -> ReclaimerScope {
        self._scope
    }
}

/// A stateless byte allocator that forwards to the Rust global allocator (`std::alloc::alloc`
/// and `dealloc` with alignment 1), whichever allocator the program installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct MallocAllocator;

impl MallocAllocator {
    /// Allocates `n` bytes with alignment 1; returns null on failure, and a dangling non-null
    /// pointer (no allocation) for `n == 0`.
    pub fn allocate(&self, n: usize) -> *mut u8 {
        // SAFETY: `n` is non-zero when the layout is built (a zero-sized request returns a
        // dangling but non-null pointer above), and alignment 1 is always valid.
        unsafe {
            if n == 0 {
                return std::ptr::NonNull::<u8>::dangling().as_ptr();
            }
            std::alloc::alloc(std::alloc::Layout::from_size_align_unchecked(n, 1))
        }
    }

    /// Frees `n` bytes at `p`; does nothing for `n == 0`.
    ///
    /// # Safety
    /// `p` was returned by `allocate(n)` on this allocator, with the same `n`, and is not
    /// freed again.
    pub unsafe fn deallocate(&self, p: *mut u8, n: usize) {
        if n == 0 {
            return;
        }
        // SAFETY: `p` was returned by `allocate(n)` on this allocator (by the caller).
        unsafe { std::alloc::dealloc(p, std::alloc::Layout::from_size_align_unchecked(n, 1)) }
    }
}

static STATIC_MALLOC_ALLOCATOR: MallocAllocator = MallocAllocator;
/// The shared [`MallocAllocator`].
pub static MALLOC_ALLOCATOR: &MallocAllocator = &STATIC_MALLOC_ALLOCATOR;

// `drain_cross_cpu_freelist` (defined below) is called periodically to recycle objects freed on
// another shard than the one that allocated them.

/// How the allocator asks the rest of the system to run reclaim: when memory is low, `hook(f)`
/// must arrange for `f` to be called at a point that is safe with respect to allocation. This keeps
/// the allocator from calling back into the rest of the system directly.
pub type ReclaimHook = Box<dyn Fn(Box<dyn FnOnce()>)>;

/// A snapshot of allocation statistics, returned by [`stats`].
///
/// The memory sizes are those of the calling thread's pool. The counters are the calling
/// thread's own: on a shard, plain thread-local counters; on another thread, one of the shared
/// atomic buckets (chosen by the thread's id, so threads may share one). Without the engine
/// allocator every counter is 0 and both memory sizes are 1 GiB.
#[derive(Debug, Clone, Copy)]
pub struct Statistics {
    _mallocs: u64,
    _frees: u64,
    _cross_cpu_frees: u64,
    _total_memory: usize,
    _free_memory: usize,
    _reclaims: u64,
    _large_allocs: u64,
    _failed_allocs: u64,

    _foreign_mallocs: u64,
    _foreign_frees: u64,
    _foreign_cross_frees: u64,
}

impl Statistics {
    /// Builds the statistics from their counters.
    #[allow(clippy::too_many_arguments)]
    fn new(
        mallocs: u64,
        frees: u64,
        cross_cpu_frees: u64,
        total_memory: u64,
        free_memory: u64,
        reclaims: u64,
        large_allocs: u64,
        failed_allocs: u64,
        foreign_mallocs: u64,
        foreign_frees: u64,
        foreign_cross_frees: u64,
    ) -> Self {
        Self {
            _mallocs: mallocs,
            _frees: frees,
            _cross_cpu_frees: cross_cpu_frees,
            _total_memory: total_memory as usize,
            _free_memory: free_memory as usize,
            _reclaims: reclaims,
            _large_allocs: large_allocs,
            _failed_allocs: failed_allocs,
            _foreign_mallocs: foreign_mallocs,
            _foreign_frees: foreign_frees,
            _foreign_cross_frees: foreign_cross_frees,
        }
    }
    /// Number of allocations counted since the thread started (every successful or failed
    /// allocation through the shard's pools, plus each `shrink`).
    pub fn mallocs(&self) -> u64 {
        self._mallocs
    }
    /// Number of frees of this shard's memory counted since the thread started, including the
    /// objects other shards freed and this shard recycled, plus each `shrink`.
    pub fn frees(&self) -> u64 {
        self._frees
    }
    /// Number of frees this thread made of memory owned by another shard.
    pub fn cross_cpu_frees(&self) -> u64 {
        self._cross_cpu_frees
    }
    /// `mallocs() - frees()` (wrapping): the allocations not freed yet, as far as the counters
    /// tell.
    pub fn live_objects(&self) -> usize {
        self.mallocs().wrapping_sub(self.frees()) as usize
    }
    /// Free memory of the shard's pool, in bytes.
    pub fn free_memory(&self) -> usize {
        self._free_memory
    }
    /// `total_memory() - free_memory()`, in bytes: memory handed out, including the small pools'
    /// unused objects and the page array.
    pub fn allocated_memory(&self) -> usize {
        self._total_memory - self._free_memory
    }
    /// Size of the shard's pool, in bytes.
    pub fn total_memory(&self) -> usize {
        self._total_memory
    }
    /// Number of reclaim rounds run because memory was low (each round calls every eligible
    /// reclaimer once).
    pub fn reclaims(&self) -> u64 {
        self._reclaims
    }
    /// Number of allocations at or above the large allocation warning threshold (each one logged
    /// a warning).
    pub fn large_allocations(&self) -> u64 {
        self._large_allocs
    }
    /// Number of allocations that failed: no memory could be found even after running the
    /// reclaimers.
    pub fn failed_allocations(&self) -> u64 {
        self._failed_allocs
    }
    /// Number of allocations this thread made from libc's allocator (as a thread that is not a
    /// shard).
    pub fn foreign_mallocs(&self) -> u64 {
        self._foreign_mallocs
    }
    /// Number of frees of memory no shard owns, made by this thread when it is not a shard.
    pub fn foreign_frees(&self) -> u64 {
        self._foreign_frees
    }
    /// Number of frees of memory no shard owns, made by this thread as a shard.
    pub fn foreign_cross_frees(&self) -> u64 {
        self._foreign_cross_frees
    }
}

/// The address range of a shard's memory pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLayout {
    /// First address of the pool.
    pub start: usize,
    /// One past the last address.
    pub end: usize,
}

/// Sets a different large allocation warning threshold on the current shard while alive; the
/// previous one is restored on drop (unless it was 0, which is then left in place). Without
/// the engine allocator both steps are no-ops.
pub struct ScopedLargeAllocationWarningThreshold {
    _old_threshold: usize,
}

impl ScopedLargeAllocationWarningThreshold {
    /// Sets the threshold to `threshold` bytes until the guard is dropped.
    pub fn new(threshold: usize) -> Self {
        let old = get_large_allocation_warning_threshold();
        set_large_allocation_warning_threshold(threshold);
        Self { _old_threshold: old }
    }
}

impl Drop for ScopedLargeAllocationWarningThreshold {
    fn drop(&mut self) {
        if self._old_threshold != 0 {
            set_large_allocation_warning_threshold(self._old_threshold);
        }
    }
}

/// Disables large allocation warnings on the current shard while alive; the previous threshold
/// is restored on drop (unless it was 0). Without the engine allocator both steps are no-ops.
pub struct ScopedLargeAllocationWarningDisable {
    _old_threshold: usize,
}

impl ScopedLargeAllocationWarningDisable {
    /// Disables the warnings until the guard is dropped.
    pub fn new() -> Self {
        let old = get_large_allocation_warning_threshold();
        disable_large_allocation_warning();
        Self { _old_threshold: old }
    }
}

impl Default for ScopedLargeAllocationWarningDisable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopedLargeAllocationWarningDisable {
    fn drop(&mut self) {
        if self._old_threshold != 0 {
            set_large_allocation_warning_threshold(self._old_threshold);
        }
    }
}

/// An allocation location in the code, identified by its backtrace, as recorded by the heap
/// profiler.
///
/// One site stands for every live sampled allocation at that location: `count` and `size` are
/// their totals. `size` is extrapolated from the sampling, so the sizes of all sites add up to
/// approximately the total memory in use. The counters and links are cells, so that sites in a
/// hash set can be updated in place.
#[derive(Clone)]
pub struct AllocationSite {
    /// Number of live objects allocated at the backtrace.
    pub count: Cell<usize>,
    /// Bytes in live objects allocated at the backtrace (extrapolated).
    pub size: Cell<usize>,
    /// The call site of the allocations.
    pub backtrace: SimpleBacktrace,

    // All allocation sites are linked to each other, so that a debugger script can walk them
    // without understanding the hash set.
    /// Next allocation site in the chain.
    pub next: Cell<*const AllocationSite>,
    /// Previous allocation site in the chain.
    pub prev: Cell<*const AllocationSite>,
}

impl Default for AllocationSite {
    fn default() -> Self {
        Self {
            count: Cell::new(0),
            size: Cell::new(0),
            backtrace: SimpleBacktrace::default(),
            next: Cell::new(std::ptr::null()),
            prev: Cell::new(std::ptr::null()),
        }
    }
}

impl PartialEq for AllocationSite {
    fn eq(&self, o: &Self) -> bool {
        self.backtrace == o.backtrace
    }
}

impl Eq for AllocationSite {}

// Sites are hashed and compared by backtrace only.
impl Hash for AllocationSite {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.backtrace.hash());
    }
}

/// Enables heap profiling on the current shard while alive (only in builds with
/// `engine_heapprof` and the engine allocator; elsewhere creating one only logs a warning).
///
/// Guards nest on a thread: profiling is turned off when the last one is dropped. A nested
/// guard cannot change the rate: while profiling is on, a new non-zero rate is ignored with a
/// warning. See [`set_heap_profiling_sampling_rate`].
pub struct ScopedHeapProfiling {
    _private: (),
}

// The allocator itself; its design is described in the module documentation.

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
#[cfg(engine_debug)]
macro_rules! dassert {
    ($e:expr) => {
        assert!($e)
    };
}
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
#[cfg(not(engine_debug))]
macro_rules! dassert {
    ($e:expr) => {
        if false {
            let _ = $e;
        }
    };
}

/// Assertion for the allocator paths that must not unwind (the span code under `free`, which
/// cannot fail): the failure is reported by an out-of-line `extern "C"`
/// function, so the check adds no unwind edge to its caller and the free path keeps its tail
/// calls. Like `engine_assert!`, it prints the expression and aborts.
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
macro_rules! alloc_assert {
    ($e:expr) => {
        if !($e) {
            #[cold]
            #[inline(never)]
            extern "C" fn assert_failed() -> ! {
                $crate::engine::util::assert::internal::assert_fail(
                    stringify!($e),
                    file!(),
                    line!() as i32,
                    module_path!(),
                )
            }
            assert_failed();
        }
    };
}

unsafe extern "C" {
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> c_int;
}

const ENOMEM: c_int = 12;
const EINVAL: c_int = 22;

/// Allocates `size` bytes aligned to `align` with `posix_memalign(3)`, for aligned buffers.
///
/// # Errors
/// Fails with a bad-allocation error if memory is exhausted, and with a runtime error if `align`
/// is not a power of two multiple of the pointer size.
///
/// # Panics
/// Panics if `posix_memalign` returns another error.
pub fn allocate_aligned_buffer_impl(
    size: usize,
    align: usize,
) -> Result<*mut c_void, ExceptionPtr> {
    let mut ret: *mut c_void = std::ptr::null_mut();
    // SAFETY: `ret` is a valid out pointer; posix_memalign only writes it on success.
    let r = unsafe { posix_memalign(&mut ret, align, size) };
    if r == ENOMEM {
        Err(make_exception_ptr(crate::engine::core::future::BadAlloc))
    } else if r == EINVAL {
        Err(make_exception_ptr(crate::engine::core::future::RuntimeError::new(format!(
            "Invalid alignment of {}; allocating {} bytes",
            align, size
        ))))
    } else {
        assert!(r == 0);
        Ok(ret)
    }
}

// The largest fundamental alignment (that of `max_align_t`) on the supported 64-bit ABIs.
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
const MAX_ALIGN_T_ALIGN: usize = 16;

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
static ABORT_ON_ALLOCATION_FAILURE: AtomicBool = AtomicBool::new(false);

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
fn alloc_failure_kind_to_u8(kind: AllocFailureKind) -> u8 {
    match kind {
        AllocFailureKind::None => 0,
        AllocFailureKind::Critical => 1,
        AllocFailureKind::All => 2,
    }
}

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
fn alloc_failure_kind_from_u8(v: u8) -> AllocFailureKind {
    match v {
        0 => AllocFailureKind::None,
        1 => AllocFailureKind::Critical,
        _ => AllocFailureKind::All,
    }
}

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
static DUMP_DIAGNOSTICS_ON_ALLOC_FAILURE_KIND: AtomicU8 =
    AtomicU8::new(1 /* AllocFailureKind::Critical */);

/// Receives one piece of the diagnostics report.
pub type MemoryDiagnosticsWriter<'a> = &'a mut dyn FnMut(&str);
/// Adds the application's own lines to the diagnostics report, through the writer it is given.
pub type MemoryDiagnosticsProducer = Box<dyn FnMut(MemoryDiagnosticsWriter<'_>)>;

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
pub use default_allocator::*;
#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
pub use engine_allocator::*;

#[cfg(all(target_os = "linux", not(engine_default_allocator)))]
mod engine_allocator {
    use super::*;
    use crate::engine::core::bitops::{log2ceil_u32, log2floor_u32};
    use crate::engine::core::posix::{FileDesc, MmapArea};
    use crate::engine::core::reactor::engine_logger;
    use crate::engine::core::shared_ptr::make_lw_shared;
    use crate::engine::util::alloc_failure_injector::memory::on_alloc_point;
    use crate::engine::util::backtrace::{current_backtrace, current_backtrace_tasklocal};
    use crate::engine::util::critical_alloc_section::memory::is_critical_alloc_section;
    use crate::engine::util::log::{FormatInfo, LambdaLogWriter, RateLimit};
    use crate::engine::util::log_impl::internal::InserterIterator;
    use crate::engine::util::sampler::Sampler;
    use std::cell::{RefCell, UnsafeCell};
    use std::collections::HashSet;
    use std::ffi::{c_char, c_long, c_ulong};
    use std::fmt;
    use std::mem::ManuallyDrop;
    use std::sync::Once;
    use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

    // System calls and libc symbols used by the allocator.
    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        fn syscall(num: c_long, ...) -> c_long;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn pthread_self() -> usize;
        fn abort() -> !;
    }

    const PROT_NONE: c_int = 0;
    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_SHARED: c_int = 0x01;
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_FIXED: c_int = 0x10;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MAP_NORESERVE: c_int = 0x4000;
    const MAP_POPULATE: c_int = 0x8000;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
    const MADV_HUGEPAGE: c_int = 14;
    const MADV_NOHUGEPAGE: c_int = 15;
    const MADV_DONTDUMP: c_int = 16;
    const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
    const MPOL_PREFERRED: c_int = 1;
    const MPOL_MF_MOVE: u32 = 1 << 1;
    #[cfg(target_arch = "x86_64")]
    const SYS_MBIND: c_long = 237;
    #[cfg(target_arch = "aarch64")]
    const SYS_MBIND: c_long = 235;
    #[cfg(target_arch = "s390x")]
    const SYS_MBIND: c_long = 268;
    #[cfg(target_arch = "powerpc64")]
    const SYS_MBIND: c_long = 259;
    #[cfg(target_arch = "x86")]
    const SYS_MBIND: c_long = 274;

    /// Pointer to a heap profiler allocation site (null for an unsampled allocation).
    pub type AllocationSitePtr = *const AllocationSite;

    const CPU_ID_SHIFT: u32 = 36; // fixed: 64 GiB of address space per shard
    const MAX_CPUS: usize = 256;
    const CPU_ID_AND_MEM_BASE_MASK: usize = !((1usize << CPU_ID_SHIFT) - 1);

    type Pageidx = u32;

    static LIVE_CPUS: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

    /// The thread-local variables that the allocation and free fast paths read:
    /// `is_reactor_thread`, the allocation statistics, `local_expected_cpu_id` and
    /// `cpu_mem_ptr`, gathered in one block, so that the fields a `malloc` or `free` touches sit
    /// next to each other. Every field starts as all zero bits
    /// except `local_expected_cpu_id` (all one bits), which the `.tdata` image below relies on.
    /// See `with_allocator_tls!` for where the block lives.
    #[repr(C)]
    struct AllocatorTls {
        /// Set when `configure` runs on the thread: it tells reactor threads, which use the shard
        /// pools, from other threads, which use the system allocator.
        is_reactor_thread: Cell<bool>,
        /// The thread's allocation counters.
        stats: UnsafeCell<alloc_stats::StatsArray>,
        /// The shard id this thread's pool belongs to, or all ones before it has one.
        local_expected_cpu_id: Cell<usize>,
        /// This thread's pool, or null.
        cpu_mem_ptr: Cell<*mut CpuPages>,
    }

    /// `AllocatorTls` as the initial image of every thread's copy.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    const ALLOCATOR_TLS_INIT: AllocatorTls = AllocatorTls {
        is_reactor_thread: Cell::new(false),
        stats: UnsafeCell::new([0; alloc_stats::ENUM_SIZE]),
        local_expected_cpu_id: Cell::new(usize::MAX),
        cpu_mem_ptr: Cell::new(std::ptr::null_mut()),
    };

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    thread_local! {
        static ALLOCATOR_TLS: AllocatorTls = const { ALLOCATOR_TLS_INIT };
    }

    /// `with_allocator_tls!(field, |c| ...)`: runs the closure on the calling thread's
    /// `AllocatorTls::field`, like `LocalKey::with` on a thread-local of its own.
    ///
    /// Elsewhere this is a const `thread_local!` (no lazy initialization, no destructor).
    /// On aarch64 (the measured and deployed target) and x86-64 the block is ELF TLS data of
    /// the initial-exec model, defined by the `global_asm!` below with one symbol per field and
    /// addressed with inline assembly, the usual way for an allocator to keep its fast paths
    /// short. A `thread_local!` in a library crate is compiled for the general-dynamic model, whatever
    /// executable it ends up in: a TLS descriptor call, which the linker relaxes to
    /// `movz/movk/nop/nop` but only after the compiler has given the function a frame (the
    /// call clobbers the link register) and moved its first argument out of `x0`. Initial-exec is
    /// the fastest model that stays valid in a shared object
    /// (one loaded at start-up, as any library exporting `malloc` must be), and the linker
    /// relaxes it to local-exec in an executable: a `movz/movk` (x86-64: `mov $imm`) of the
    /// field's offset, and the load or store addresses the thread pointer plus that register.
    ///
    /// One symbol per field rather than one for the block: with a single base the compiler
    /// adds the offset to the thread pointer first and every access waits for that add, which
    /// measured about 10 % slower in the `malloc_only` loop than one offset register per
    /// field.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    macro_rules! with_allocator_tls {
        ($field:ident, $f:expr) => {{
            let off = initial_exec_offset!(allocator_tls_symbol!(), ".", stringify!($field));
            let block = thread_pointer()
                .wrapping_add(off)
                .wrapping_sub(std::mem::offset_of!(AllocatorTls, $field))
                as *const AllocatorTls;
            // SAFETY: `block` is the calling thread's copy of the block (the field's symbol
            // lies at the field's offset in the image, see below), which the C runtime
            // allocates and fills from the initial image (a valid bit pattern for every field)
            // before the thread runs, aligned as the image declares, and keeps until the
            // thread is gone. The fields are `Cell` and `UnsafeCell`, so a shared reference is
            // sound; the closure takes it for its own call only (`with_ref`), and
            // `AllocatorTls` is `!Sync`, so it cannot reach another thread.
            unsafe { with_ref(&raw const (*block).$field, $f) }
        }};
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    macro_rules! with_allocator_tls {
        ($field:ident, $f:expr) => {
            ALLOCATOR_TLS.with(|t| ($f)(&t.$field))
        };
    }

    /// Calls `f` with `&*p`, for the duration of the call only.
    ///
    /// # Safety
    /// `p` is valid for shared reads (through interior mutability) for the call.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[inline(always)]
    unsafe fn with_ref<T, R>(p: *const T, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: by the caller.
        f(unsafe { &*p })
    }

    /// The thread pointer and the initial-exec offset of a TLS symbol, shared with the other
    /// initial-exec thread-locals of the crate.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    use crate::engine::util::initial_exec_tls::{initial_exec_offset, thread_pointer};

    /// The ELF symbol of the thread-local [`AllocatorTls`] block; each field has the symbol
    /// `<block>.<field>`.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    macro_rules! allocator_tls_symbol {
        () => {
            "__engine_rs_memory_allocator_tls"
        };
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    const TLS_STATS: usize = std::mem::offset_of!(AllocatorTls, stats);
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    const TLS_CPU_ID: usize = std::mem::offset_of!(AllocatorTls, local_expected_cpu_id);
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    const TLS_CPU_MEM: usize = std::mem::offset_of!(AllocatorTls, cpu_mem_ptr);
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    const TLS_SIZE: usize = std::mem::size_of::<AllocatorTls>();

    // The initial image of the block in `.tdata`: all zero bits (false, zero counters, a null
    // cpu_mem_ptr) except the all-ones `local_expected_cpu_id` (`usize::MAX`), with a
    // label at each field's offset. Hidden, so the symbols never interpose or are interposed;
    // global, so code of other crates into which the `#[inline]` fast paths are inlined links
    // to the same block. The assertions pin the field order the image is written in.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    const _: () = {
        assert!(std::mem::size_of::<usize>() == 8);
        assert!(std::mem::align_of::<AllocatorTls>() == 8); // the .p2align 3 of the image
        assert!(std::mem::offset_of!(AllocatorTls, is_reactor_thread) == 0);
        assert!(0 < TLS_STATS && TLS_STATS < TLS_CPU_ID);
        assert!(TLS_CPU_ID + 8 <= TLS_CPU_MEM && TLS_CPU_MEM + 8 <= TLS_SIZE);
    };

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    macro_rules! allocator_tls_label {
        ($field:literal) => {
            concat!(
                ".globl ",
                allocator_tls_symbol!(),
                $field,
                "\n",
                ".hidden ",
                allocator_tls_symbol!(),
                $field,
                "\n",
                ".type ",
                allocator_tls_symbol!(),
                $field,
                ",%object\n",
                allocator_tls_symbol!(),
                $field,
                ":"
            )
        };
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    std::arch::global_asm!(
        concat!(".pushsection .tdata.", allocator_tls_symbol!(), ",\"awT\",%progbits"),
        ".p2align 3",
        allocator_tls_label!(""),
        concat!(".size ", allocator_tls_symbol!(), ", {size}"),
        allocator_tls_label!(".is_reactor_thread"),
        ".zero {stats}",
        allocator_tls_label!(".stats"),
        ".zero {cpu_id} - {stats}",
        allocator_tls_label!(".local_expected_cpu_id"),
        ".quad -1",
        ".zero {cpu_mem} - {cpu_id} - 8",
        allocator_tls_label!(".cpu_mem_ptr"),
        ".zero {size} - {cpu_mem}",
        ".popsection",
        size = const TLS_SIZE,
        stats = const TLS_STATS,
        cpu_id = const TLS_CPU_ID,
        cpu_mem = const TLS_CPU_MEM,
    );

    /// Runs `f` on the calling thread's whole [`AllocatorTls`], addressed from one base (the
    /// block's symbol): for a path that reads several fields and gains nothing from one
    /// offset register per field (see `CpuPages::try_free_fastpath`).
    #[inline(always)]
    fn with_allocator_tls_block<R>(f: impl FnOnce(&AllocatorTls) -> R) -> R {
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        {
            let off = initial_exec_offset!(allocator_tls_symbol!());
            // SAFETY: the calling thread's copy of the block, as in `with_allocator_tls!`.
            unsafe { with_ref(thread_pointer().wrapping_add(off) as *const AllocatorTls, f) }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            ALLOCATOR_TLS.with(f)
        }
    }

    #[inline(always)]
    fn is_reactor_thread() -> bool {
        with_allocator_tls!(is_reactor_thread, |c: &Cell<bool>| c.get())
    }

    // We default transparent hugepages to true since we prefer to transiently
    // use a transparent hugepage and then break it, to having the kernel
    // work to rearrange a broken transparent hugepage.
    /// Whether shard memory is advised for transparent huge pages: while it is true, every
    /// mapping the pools add gets `MADV_HUGEPAGE`. Starts true; [`configure`] stores its
    /// `transparent_hugepages` argument here and, when it is false, advises the memory already
    /// mapped `MADV_NOHUGEPAGE`. Shared by every thread (relaxed loads and stores).
    pub static USE_TRANSPARENT_HUGEPAGES: AtomicBool = AtomicBool::new(true);

    pub(super) mod alloc_stats {
        use super::*;

        /// The kinds of allocation events counted per thread.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(usize)]
        pub enum Types {
            Allocs,
            Frees,
            CrossCpuFrees,
            Reclaims,
            LargeAllocs,
            FailedAllocs,
            ForeignMallocs,
            ForeignFrees,
            ForeignCrossFrees,
            EnumSize,
        }

        /// Number of kinds in [`Types`].
        pub const ENUM_SIZE: usize = Types::EnumSize as usize;

        /// One counter per kind, for a reactor thread.
        pub type StatsArray = [u64; ENUM_SIZE];
        /// One atomic counter per kind, for events on threads that are not shards.
        pub type StatsAtomicArray = [AtomicU64; ENUM_SIZE];

        // The per-thread counters live in the `stats` field of AllocatorTls.
        /// Counters of events made on threads that are not shards, in `MAX_CPUS` buckets: a
        /// thread uses bucket `pthread_self() % MAX_CPUS`, so unrelated threads whose ids
        /// collide share one. Updated with relaxed atomic adds; read only by [`get`] on such a
        /// thread, never summed into a shard's statistics.
        pub static ALIEN_STATS: [StatsAtomicArray; MAX_CPUS] =
            [const { [const { AtomicU64::new(0) }; ENUM_SIZE] }; MAX_CPUS];

        // The closure's unsafe block is needed by the thread_local! variant of
        // with_allocator_tls; the initial-exec variant already encloses it in its own.
        /// Adds `size` to the calling thread's own counter of `stat_type` (the `stats` field of
        /// its thread-local block), without checking that the thread is a shard: one
        /// thread-local add, no atomics; used on the allocation and free fast paths.
        #[cfg_attr(any(target_arch = "aarch64", target_arch = "x86_64"), allow(unused_unsafe))]
        #[inline(always)]
        pub fn increment_local(stat_type: Types, size: u64) {
            // SAFETY: the array is thread-local and no reference to it escapes this call.
            with_allocator_tls!(stats, |s: &UnsafeCell<StatsArray>| unsafe {
                (*s.get())[stat_type as usize] += size
            });
        }

        #[inline(always)]
        fn thread_hash() -> usize {
            // The thread's pthread_t, used as is as the hash.
            // SAFETY: pthread_self has no preconditions.
            unsafe { pthread_self() }
        }

        /// Adds `size` to the counter of `stat_type`: the thread's own (see [`increment_local`])
        /// on a shard, otherwise the thread's bucket of [`ALIEN_STATS`] with a relaxed atomic
        /// add.
        pub fn increment(stat_type: Types, size: u64) {
            // fast path, reactor threads takes thread local statistics
            if is_reactor_thread() {
                increment_local(stat_type, size);
            } else {
                let hash = thread_hash();
                let i = stat_type as usize;
                ALIEN_STATS[hash % ALIEN_STATS.len()][i].fetch_add(size, Ordering::Relaxed);
            }
        }

        // As in increment_local.
        /// Returns the calling thread's counter of `stat_type`: its own on a shard, otherwise its
        /// bucket of [`ALIEN_STATS`] (a sequentially consistent load). Events other threads
        /// counted are not included.
        #[cfg_attr(any(target_arch = "aarch64", target_arch = "x86_64"), allow(unused_unsafe))]
        pub fn get(stat_type: Types) -> u64 {
            let i = stat_type as usize;
            // fast path, reactor threads takes thread local statistics
            if is_reactor_thread() {
                // SAFETY: the array is thread-local and no reference to it escapes this call.
                with_allocator_tls!(stats, |s: &UnsafeCell<StatsArray>| unsafe { (*s.get())[i] })
            } else {
                let hash = thread_hash();
                ALIEN_STATS[hash % ALIEN_STATS.len()][i].load(Ordering::SeqCst)
            }
        }
    }

    // The next allocator in symbol resolution (libc's): it serves non-reactor threads and memory
    // that no shard owns. Its functions are looked up with `dlsym(RTLD_NEXT)` into the `LIBC_*`
    // slots, read through `libc!`; allocations made before the lookup use the engine allocator.
    type MallocFuncType = unsafe extern "C" fn(usize) -> *mut c_void;
    type FreeFuncType = unsafe extern "C" fn(*mut c_void);
    type ReallocFuncType = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
    type AlignedAllocType = unsafe extern "C" fn(usize, usize) -> *mut c_void;
    type MallocTrimType = unsafe extern "C" fn(usize) -> c_int;
    type MallocUsableSizeType = unsafe extern "C" fn(*mut c_void) -> usize;

    static LIBC_MALLOC_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LIBC_FREE_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LIBC_REALLOC_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LIBC_ALIGNED_ALLOC_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LIBC_MALLOC_TRIM_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LIBC_MALLOC_USABLE_SIZE_FUNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    /// Looks up libc's allocation functions with `dlsym(RTLD_NEXT)`.
    fn load_libc_functions() {
        // SAFETY: dlsym with RTLD_NEXT and NUL-terminated names has no other precondition.
        unsafe {
            LIBC_MALLOC_FUNC.store(dlsym(RTLD_NEXT, c"malloc".as_ptr()), Ordering::Relaxed);
            LIBC_FREE_FUNC.store(dlsym(RTLD_NEXT, c"free".as_ptr()), Ordering::Relaxed);
            LIBC_REALLOC_FUNC.store(dlsym(RTLD_NEXT, c"realloc".as_ptr()), Ordering::Relaxed);
            LIBC_ALIGNED_ALLOC_FUNC
                .store(dlsym(RTLD_NEXT, c"aligned_alloc".as_ptr()), Ordering::Relaxed);
            LIBC_MALLOC_TRIM_FUNC
                .store(dlsym(RTLD_NEXT, c"malloc_trim".as_ptr()), Ordering::Relaxed);
            LIBC_MALLOC_USABLE_SIZE_FUNC
                .store(dlsym(RTLD_NEXT, c"malloc_usable_size".as_ptr()), Ordering::Relaxed);
        }
    }

    // Runs the lookup from the ELF initializers, before `main`.
    #[used]
    #[unsafe(link_section = ".init_array")]
    static INIT_LIBC_FUNCTIONS: extern "C" fn() = {
        extern "C" fn init() {
            load_libc_functions();
        }
        init
    };

    /// `libc!(SLOT, FnType)`: the libc function stored in `SLOT` by `load_libc_functions`, as a
    /// `FnType`, or `None` while the lookup has not run (or found no such symbol).
    macro_rules! libc {
        ($slot:ident, $ty:ty) => {{
            let p = $slot.load(Ordering::Relaxed);
            if p.is_null() {
                None
            } else {
                // SAFETY: the slot only ever holds the address dlsym returned for this symbol,
                // whose C signature is `$ty`.
                Some(unsafe { std::mem::transmute::<*mut c_void, $ty>(p) })
            }
        }};
    }

    /// Maps `how_much` bytes of system memory at a given address of the shard's window.
    pub type AllocateSystemMemoryFn =
        std::rc::Rc<dyn Fn(*mut c_void, usize) -> Result<MmapArea, ExceptionPtr>>;

    // The per-thread expected shard id lives in the `local_expected_cpu_id` field of AllocatorTls.

    /// Returns the shard number encoded in the address of `ptr`: bits 36 to 43, with no check.
    /// Meaningful only for a pointer into shard memory (see [`is_engine_memory`]); for any
    /// other it is an arbitrary number below 256. A shift and a mask.
    #[inline]
    pub fn object_cpu_id(ptr: *const c_void) -> u32 {
        ((ptr as usize >> CPU_ID_SHIFT) & 0xff) as u32
    }

    /// The links of a page in a [`PageList`], as page indexes (0 for none).
    #[derive(Debug, Clone, Copy, Default)]
    #[repr(C)]
    pub struct PageListLink {
        _prev: u32,
        _next: u32,
    }

    const MEM_BASE_ALLOC: usize = 1usize << 44;

    static MEM_BASE_KNOWN: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
    static MEM_BASE_FLAG: Once = Once::new();

    fn mem_base() -> *mut u8 {
        MEM_BASE_FLAG.call_once(|| {
            // SAFETY: reserving address space with PROT_NONE and trimming it to an aligned
            // window of mem_base_alloc bytes; the unmapped ranges belong to this mapping.
            unsafe {
                let r = mmap(
                    std::ptr::null_mut(),
                    2 * MEM_BASE_ALLOC,
                    PROT_NONE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                    -1,
                    0,
                );
                if r == MAP_FAILED {
                    abort();
                }
                madvise(r, 2 * MEM_BASE_ALLOC, MADV_DONTDUMP);
                let cr = r as *mut u8;
                let known = crate::engine::core::align::align_up_ptr(cr, MEM_BASE_ALLOC);
                munmap(cr as *mut c_void, known as usize - cr as usize);
                let tail = known.add(MEM_BASE_ALLOC);
                munmap(tail as *mut c_void, cr.add(2 * MEM_BASE_ALLOC) as usize - tail as usize);
                // extremely unlikely for mmap to return a mapping at 0, but our detection of free(null)
                // depends on it not doing that so check it
                assert!(!known.is_null());
                assert!(known as usize != 0);
                MEM_BASE_KNOWN.store(known, Ordering::Release);
            }
        });
        MEM_BASE_KNOWN.load(Ordering::Acquire)
    }

    /// Returns whether `ptr` lies in the 16 TiB address window reserved for all shards' pools
    /// (as opposed to memory of libc's allocator or any other mapping). The address need not
    /// be allocated. The first call in the process reserves the window (an `mmap` of
    /// `PROT_NONE` address space, aborting the process if it fails); later calls are two
    /// comparisons after a `Once` check.
    pub fn is_engine_memory(ptr: *const c_void) -> bool {
        let begin = mem_base() as usize;
        let end = begin + MEM_BASE_ALLOC;
        let p = ptr as usize;
        p >= begin && p < end
    }

    /// Returns whether `size` is a multiple of [`PAGE_SIZE`].
    pub const fn is_page_aligned(size: usize) -> bool {
        (size & (PAGE_SIZE - 1)) == 0
    }

    /// Returns `size` rounded up to a multiple of [`PAGE_SIZE`].
    pub const fn next_page_aligned(size: usize) -> usize {
        (size + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1)
    }

    /// A free object of a small pool: the link to the next free object, stored in the object
    /// itself. A small pool lays its objects every `object_size` bytes, and size classes such as
    /// 12 or 20 are not multiples of 8, so a free object is not always pointer-aligned. `packed` makes
    /// every `next` read and write an unaligned access, which costs the same on x86-64 and
    /// aarch64 and is defined behaviour in Rust; the field is only ever copied, never borrowed.
    #[repr(C, packed)]
    pub struct FreeObject {
        next: *mut FreeObject,
    }

    /// The descriptor of one page of a shard's pool: span bookkeeping and, for the first page of a span, its pool or large allocation data.
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct Page {
        free: bool,
        offset_in_span: u8,
        nr_small_alloc: u16,
        span_size: u32, // in pages, if we're the head or the tail
        link: PageListLink,
        pool: *mut SmallPool, // if used in a small_pool
        freelist: *mut FreeObject,
        #[cfg(engine_heapprof)]
        alloc_site: AllocationSitePtr, // for objects whose size is multiple of page size, valid for head only
    }

    /// Returns page `idx` of the page array `ary`, as a raw pointer: the allocator re-enters itself
    /// while it works on a page (a one-page span is its own head and tail), so no
    /// reference to a page outlives a single field access.
    ///
    /// # Safety
    /// `idx` lies within the page array `ary` (which has one sentinel past nr_pages).
    #[inline(always)]
    unsafe fn pg(ary: *mut Page, idx: usize) -> *mut Page {
        // SAFETY: `idx` is in bounds of the array by the caller.
        unsafe { ary.add(idx) }
    }

    #[inline(always)]
    fn idx_of(ary: *mut Page, span: *const Page) -> usize {
        // SAFETY: `span` points into the same page array as `ary`.
        unsafe { span.offset_from(ary) as usize }
    }

    /// A doubly linked list of spans, threaded through the page array by index.
    #[derive(Clone, Copy, Default)]
    pub struct PageList {
        _front: u32,
        _back: u32,
    }

    impl PageList {
        /// Returns an empty list.
        pub const fn new() -> Self {
            Self { _front: 0, _back: 0 }
        }
        /// The first span of the list (the sentinel page 0 when it is empty).
        ///
        /// # Safety
        /// `ary` is the page array the list links into.
        #[inline(always)]
        pub unsafe fn front(&self, ary: *mut Page) -> *mut Page {
            // SAFETY: list links are indexes of the page array by the caller.
            unsafe { pg(ary, self._front as usize) }
        }
        /// The last span of the list (the sentinel page 0 when it is empty).
        ///
        /// # Safety
        /// As for [`PageList::front`].
        #[inline(always)]
        pub unsafe fn back(&self, ary: *mut Page) -> *mut Page {
            // SAFETY: as in `front`.
            unsafe { pg(ary, self._back as usize) }
        }
        /// Returns whether the list is empty.
        pub fn empty(&self) -> bool {
            self._front == 0
        }
        /// Unlinks `span`.
        ///
        /// # Safety
        /// `span` is an element of this list in the page array `ary`.
        pub unsafe fn erase(&mut self, ary: *mut Page, span: *mut Page) {
            // SAFETY: `span` and its links are pages of `ary` by the caller; each access is
            // a raw place, so `span` may be any of its neighbours.
            unsafe {
                let next = (*span).link._next;
                let prev = (*span).link._prev;
                if next != 0 {
                    (*pg(ary, next as usize)).link._prev = prev;
                } else {
                    self._back = prev;
                }
                if prev != 0 {
                    (*pg(ary, prev as usize)).link._next = next;
                } else {
                    self._front = next;
                }
            }
        }
        /// Links `span` at the front.
        ///
        /// # Safety
        /// `span` is a page of `ary` not linked in any list.
        pub unsafe fn push_front(&mut self, ary: *mut Page, span: *mut Page) {
            let idx = idx_of(ary, span) as u32;
            // Read once: LLVM reloads the field after the store to the old front page.
            let front = self._front;
            // SAFETY: `span` and the list links are pages of `ary` by the caller.
            unsafe {
                if front != 0 {
                    (*pg(ary, front as usize)).link._prev = idx;
                } else {
                    self._back = idx;
                }
                (*span).link._next = front;
                (*span).link._prev = 0;
            }
            self._front = idx;
        }
        /// Unlinks the first span.
        ///
        /// # Safety
        /// The list is not empty and links into `ary`.
        pub unsafe fn pop_front(&mut self, ary: *mut Page) {
            // SAFETY: the list links are pages of `ary` by the caller.
            unsafe {
                let next = (*pg(ary, self._front as usize)).link._next;
                if next != 0 {
                    (*pg(ary, next as usize)).link._prev = 0;
                } else {
                    self._back = 0;
                }
                self._front = next;
            }
        }
    }

    #[derive(Clone, Copy)]
    struct SpanSizes {
        preferred: u8,
        fallback: u8,
    }

    /// The free objects of one small size class on one shard, refilled from spans.
    pub struct SmallPool {
        _free: *mut FreeObject,
        _object_size: u32,
        _span_sizes: SpanSizes,
        _free_count: u32,
        _min_free: u32,
        _max_free: u32,
        _pages_in_use: u32,
        // Flag to indicate whether this pool stores sampled allocations.
        // When freeing small allocations this flag is checked to see whether an
        // allocation site pointer is part of the object and the allocation needs
        // removal from the allocation_site tracking
        #[cfg(engine_heapprof)]
        _sampled_pool: bool,
        _span_list: PageList,
        // A pool is reached through `&mut self` on paths that re-enter the allocator (adding
        // objects runs the reclaimers, which may free into this very pool). `!Unpin` keeps those
        // `&mut` from asserting uniqueness, the same exemption the crate's intrusive types rely
        // on; the pool never moves (it
        // lives in the thread's `cpu_pages`).
        _pin: std::marker::PhantomPinned,
    }

    impl SmallPool {
        const IDX_FRAC_BITS: u32 = 2;

        /// Returns the size in bytes of this pool's objects (its size class).
        pub fn object_size(&self) -> u32 {
            self._object_size
        }
        /// Returns whether this pool holds heap-profiler-sampled objects, which carry their
        /// allocation site (see `SmallPool::alloc_site_holder`); always false without
        /// `engine_heapprof`, where it compiles to a constant.
        #[inline(always)]
        pub fn is_sampled_pool(&self) -> bool {
            #[cfg(engine_heapprof)]
            {
                self._sampled_pool
            }
            #[cfg(not(engine_heapprof))]
            {
                false
            }
        }
        /// Returns whether this class's objects are whole pages.
        pub fn objects_page_aligned(&self) -> bool {
            is_page_aligned(self._object_size as usize)
        }

        // index 0b0001'1100 -> size (1 << 4) + 0b11 << (4 - 2)

        /// Returns the object size of size class `idx`. Classes are four per power of two
        /// (`(4 + idx % 4) << (idx / 4) >> 2`), and sizes above 16 bytes are rounded up to a
        /// multiple of 16. Used to build the pools (const evaluation), not on a hot path.
        pub const fn idx_to_size(idx: u32) -> u32 {
            let mut s: usize = (((1usize << Self::IDX_FRAC_BITS)
                | (idx as usize & ((1 << Self::IDX_FRAC_BITS) - 1)))
                << (idx >> Self::IDX_FRAC_BITS))
                >> Self::IDX_FRAC_BITS;
            // If size is larger than max_align_t, force it to be a multiple of
            // max_align_t. Clang relies in this property to use aligned mov
            // instructions (e.g. movaps)
            //
            // Note this function is used at initialization time only, so it doesn't
            // need to be especially fast.
            if s > MAX_ALIGN_T_ALIGN {
                s = crate::engine::core::align::align_up_usize(s, MAX_ALIGN_T_ALIGN);
            }
            s as u32
        }

        /// Returns the smallest size class that holds `size` bytes: a few shifts and adds, no
        /// table. `size` must be at least 4 (the callers pass at least 8): for 0 the logarithm
        /// is undefined, and below 4 the shift underflows.
        #[inline(always)]
        pub const fn size_to_idx(size: u32) -> u32 {
            ((log2floor_u32(size) << Self::IDX_FRAC_BITS) - ((1 << Self::IDX_FRAC_BITS) - 1))
                + ((size - 1) >> (log2floor_u32(size) - Self::IDX_FRAC_BITS))
        }

        /// Returns an empty pool of `object_size`-byte objects (`object_size` must not be 0);
        /// `is_sampled` marks the pool of sampled allocations and is ignored without
        /// `engine_heapprof`.
        ///
        /// It picks the span sizes the pool refills from: the preferred one is the first power
        /// of two up to 32 pages that holds at least four objects and wastes under 5 %
        /// (otherwise the least wasteful of those), the fallback the fewest pages that hold one
        /// object. The free list is trimmed back to halfway between `max_free / 2` and
        /// `max_free` once it reaches `max_free` objects: twice the objects of a span of the
        /// preferred size (of 64 pages when no size met the 5 % goal), and at least 100.
        pub const fn new(object_size: u32, is_sampled: bool) -> Self {
            let mut span_size: u32 = 1;
            // waste(span) = (span bytes % object_size) / span bytes
            while object_size as usize > span_size as usize * PAGE_SIZE {
                span_size += 1;
            }
            let fallback = span_size as u8;

            // Choose a preferred span size which keeps waste (internal fragmentation) below
            // 5% and fits at least 4 objects. If there is no span size (up to 32 pages) that
            // satisfies this, just go with the minimum waste out of the checked span sizes.
            let mut min_waste = f32::MAX;
            let mut min_waste_span_size: u32 = 0;
            span_size = 1;
            while span_size <= 32 {
                let span_bytes = span_size as usize * PAGE_SIZE;
                if span_bytes / object_size as usize >= 4 {
                    let w = ((span_bytes % object_size as usize) as f64 / (1.0 * span_bytes as f64))
                        as f32;
                    if w < min_waste {
                        min_waste = w;
                        min_waste_span_size = span_size;
                        if w < 0.05 {
                            break;
                        }
                    }
                }
                span_size *= 2;
            }
            let preferred =
                if min_waste_span_size != 0 { min_waste_span_size as u8 } else { fallback };

            let span_bytes = span_size as usize * PAGE_SIZE;
            let mut max_free = (span_bytes * 2 / object_size as usize) as u32;
            if max_free < 100 {
                max_free = 100;
            }
            let _ = is_sampled;
            Self {
                _free: std::ptr::null_mut(),
                _object_size: object_size,
                _span_sizes: SpanSizes { preferred, fallback },
                _free_count: 0,
                _min_free: max_free / 2,
                _max_free: max_free,
                _pages_in_use: 0,
                #[cfg(engine_heapprof)]
                _sampled_pool: is_sampled,
                _span_list: PageList::new(),
                _pin: std::marker::PhantomPinned,
            }
        }

        /// Removes the first object of the free list and returns it.
        ///
        /// # Safety
        /// `this` is a live pool whose free list is not empty (not checked).
        #[inline(always)]
        unsafe fn pop_free(this: *mut Self) -> *mut c_void {
            // SAFETY: by the caller; each access is a raw place of the pool.
            unsafe {
                let obj = (*this)._free;
                (*this)._free = (*obj).next;
                (*this)._free_count -= 1;
                obj as *mut c_void
            }
        }

        // Must not raise an error when memory runs out, to avoid infinite recursion: raising an
        // allocation failure exception itself allocates. The runtime's __cxa_allocate_exception
        // falls back to its emergency pool when malloc() returns null.
        //
        // The pool is reached through `*mut Self`: refilling it runs the
        // reclaimers, which may free into this very pool, so no reference to it is held
        // across that call.
        /// Returns an object of this pool's size class: the head of the free list, or, when the
        /// list is empty, the result of refilling it out of line (from the pool's partly used
        /// spans, then from new spans, which may run the synchronous reclaimers). Returns null
        /// when no span can be had; it never panics or reports the failure itself.
        ///
        /// # Safety
        /// `this` is a pool of the calling thread's cpu_pages.
        #[inline(always)]
        pub unsafe fn allocate(this: *mut Self) -> *mut c_void {
            // SAFETY: by the caller.
            unsafe {
                if likely(!(*this)._free.is_null()) {
                    Self::pop_free(this)
                } else {
                    Self::add_more_objects(this)
                }
            }
        }

        /// Pushes `object` on the pool's free list; when the list reaches `max_free` objects it
        /// is trimmed out of line, returning objects to their spans and wholly free spans to the
        /// shard's buddy lists. Cannot fail or unwind.
        ///
        /// # Safety
        /// `this` is a pool of the calling thread's cpu_pages and `object` a block of its size
        /// class allocated from this shard.
        #[inline(always)]
        pub unsafe fn deallocate(this: *mut Self, object: *mut c_void) {
            let o = object as *mut FreeObject;
            // SAFETY: by the caller; `object` is at least sizeof(free_object) bytes.
            unsafe {
                (*o).next = (*this)._free;
                (*this)._free = o;
                (*this)._free_count += 1;
                if (*this)._free_count >= (*this)._max_free {
                    Self::trim_free_list(this);
                }
            }
        }

        /// # Safety
        /// As for [`SmallPool::allocate`].
        #[inline(never)]
        unsafe fn add_more_objects(this: *mut Self) -> *mut c_void {
            // SAFETY: `this` is live by the caller and each access (here and every `(*this)`
            // below) is a raw place, never a reference held across allocate_large.
            let goal = unsafe { ((*this)._min_free + (*this)._max_free) / 2 };
            // The pool is a field of the thread's cpu_pages: reach it through its raw pointer
            // and never form a reference to the whole object here.
            let mem = cpu_mem_ptr();
            // SAFETY: the thread's cpu_pages is initialized before any pool is used and lives
            // as long as the thread; only the `pages` field is read.
            let pages = unsafe { (*mem).pages };
            // SAFETY: the span list links pages of this shard's array; `span` stays a raw
            // pointer across pop_front, which writes its neighbours' links (and its own when
            // the list has one element).
            unsafe {
                while !(*this)._span_list.empty() && (*this)._free_count < goal {
                    let span = (*this)._span_list.front(pages);
                    (*this)._span_list.pop_front(pages);
                    // The span's objects move to the pool's list in registers and the four
                    // fields are written once after the walk. A `FreeObject` store cannot change the
                    // pool or the page (the objects lie in the shard's pages, the pool and the
                    // page array in cpu_pages), but LLVM cannot prove that and reloaded
                    // `span.freelist` from the store just made before every `next` load, a
                    // store-to-load round trip on the miss chain.
                    let mut obj = (*span).freelist;
                    let mut free = (*this)._free;
                    let mut moved: u32 = 0;
                    while !obj.is_null() {
                        // `obj` is a free object of this span.
                        let next = (*obj).next;
                        (*obj).next = free;
                        free = obj;
                        obj = next;
                        moved += 1;
                    }
                    (*span).freelist = std::ptr::null_mut();
                    (*this)._free = free;
                    (*this)._free_count += moved;
                    // `moved` increments of the `u16` counter, wrapping at 2^16.
                    (*span).nr_small_alloc = (*span).nr_small_alloc.wrapping_add(moved as u16);
                }
            }
            // SAFETY: as above; allocate_large may run the reclaimers, which may free into
            // this pool through `this`; `span` heads the run of span_size pages just allocated.
            unsafe {
                while (*this)._free_count < goal {
                    let mut span_size = (*this)._span_sizes.preferred as u32;
                    let mut data = CpuPages::allocate_large(mem, span_size, false) as *mut u8;
                    if data.is_null() {
                        span_size = (*this)._span_sizes.fallback as u32;
                        data = CpuPages::allocate_large(mem, span_size, false) as *mut u8;
                        if data.is_null() {
                            break;
                        }
                    }
                    // to_page only reads the page array pointer.
                    let span = (*mem).to_page(data as *const c_void);
                    span_size = (*span).span_size;
                    (*this)._pages_in_use += span_size;
                    for i in 0..span_size {
                        let p = span.add(i as usize);
                        (*p).offset_in_span = i as u8;
                        (*p).pool = this;
                    }
                    (*span).freelist = std::ptr::null_mut();
                    // As in the walk above, the list head and the two counters stay in
                    // registers while the objects are linked, and are stored once after it; LLVM
                    // otherwise reloaded all three after every object store.
                    let object_size = (*this)._object_size as usize;
                    let mut free = (*this)._free;
                    let mut carved: u32 = 0;
                    let mut offset = 0usize;
                    while offset <= span_size as usize * PAGE_SIZE - object_size {
                        let h = data.add(offset) as *mut FreeObject;
                        (*h).next = free;
                        free = h;
                        carved += 1;
                        offset += object_size;
                    }
                    (*this)._free = free;
                    (*this)._free_count += carved;
                    // `nr_small_alloc = 0` then `carved` increments of the `u16` counter.
                    (*span).nr_small_alloc = carved as u16;
                }

                if !(*this)._free.is_null() { Self::pop_free(this) } else { std::ptr::null_mut() }
            }
        }

        /// Out of line, and `extern "C"` so that it cannot unwind (it only moves objects to
        /// their spans and spans to the buddy lists, and a panic in it aborts): `deallocate`,
        /// inlined into every free path, then calls it with no landing pad.
        ///
        /// # Safety
        /// As for [`SmallPool::allocate`].
        #[inline(never)]
        unsafe extern "C" fn trim_free_list(this: *mut Self) {
            // SAFETY: `this` is live by the caller; each access is a raw place. `obj` heads
            // the freelist and its page belongs to a span of this pool, a buddy span (power-of-two
            // size) from allocate_large; free_span touches the page array and the span lists,
            // not the pools.
            unsafe {
                let goal = ((*this)._min_free + (*this)._max_free) / 2;
                // The pool is a field of the thread's cpu_pages: work on its raw pointer.
                let mem = cpu_mem_ptr();
                let pages = (*mem).pages;
                // The list head and its count are carried in registers and stored once when
                // the walk ends: the stores to the object and its page below cannot change the
                // pool (it lives in cpu_pages, outside the shard's pages and the page array),
                // and free_span only touches the page array, the span lists and nr_free_pages,
                // never a pool, so nothing reads the two fields before they are written back.
                // Without the local copy LLVM reloaded `_free` from the store just made, a
                // store-to-load round trip before every `next` load of the walk.
                let mut free = (*this)._free;
                let mut free_count = (*this)._free_count;
                while !free.is_null() && free_count > goal {
                    let obj = free;
                    free = (*obj).next;
                    free_count -= 1;
                    // to_page from the page array read above (the stores of the walk would
                    // otherwise make LLVM reload `pages` from cpu_pages each step).
                    let mut span = pg(pages, CpuPages::page_idx(obj as *const c_void));
                    span = span.sub((*span).offset_in_span as usize);
                    // Read once: linking the span writes only its `link` and its neighbours'.
                    let span_free = (*span).freelist;
                    if span_free.is_null() {
                        (*span).link = PageListLink::default();
                        (*this)._span_list.push_front(pages, span);
                    }
                    (*obj).next = span_free;
                    (*span).freelist = obj;
                    (*span).nr_small_alloc -= 1;
                    if (*span).nr_small_alloc == 0 {
                        (*this)._pages_in_use -= (*span).span_size;
                        (*this)._span_list.erase(pages, span);
                        (*mem).free_span(idx_of(pages, span) as u32, (*span).span_size);
                    }
                }
                (*this)._free = free;
                (*this)._free_count = free_count;
            }
        }

        /// Returns the slot that holds the allocation site of `ptr`, an object of this sampled
        /// pool: the last pointer-sized bytes of the object, or, for a class of whole pages,
        /// the `alloc_site` field of the object's page structure. The `'static` lifetime is not
        /// checked: the slot lives as long as the object, and the caller must not keep it longer.
        #[cfg(engine_heapprof)]
        pub fn alloc_site_holder(&mut self, ptr: *mut c_void) -> &'static mut AllocationSitePtr {
            if self.objects_page_aligned() {
                // SAFETY: `ptr` is an object of this pool, so its page structure exists.
                unsafe { &mut (*(*get_cpu_mem()).to_page(ptr)).alloc_site }
            } else {
                // SAFETY: sampled objects carry the allocation_site pointer in their last bytes.
                unsafe {
                    &mut *((ptr as *mut u8)
                        .add(self._object_size as usize - size_of::<AllocationSitePtr>())
                        as *mut AllocationSitePtr)
                }
            }
        }
    }

    impl Drop for SmallPool {
        fn drop(&mut self) {
            self._min_free = 0;
            self._max_free = 0;
            // SAFETY: a dropped pool is a pool of this thread's cpu_pages (the arrays hold
            // them in ManuallyDrop, so this only runs for an explicit drop on its thread).
            unsafe { Self::trim_free_list(self) };
        }
    }

    // tag the pools in this array as sampled, see small_pool._sampled_pool
    //
    // Cache-line aligned, so that each pool sits at `idx * size_of::<SmallPool>()` from a 64-byte
    // boundary (the 1024-byte pool of the
    // `cross_page` bench on one line). Rust reorders the fields of `CpuPages` and put the
    // array at offset 7848, which split that pool's `_free`/`_free_count`/`_max_free` over two
    // lines and measured 10 % slower on five frees then five allocations of it.
    /// The small pools of a shard, one per size class; `SAMPLED` selects the pools of sampled allocations.
    #[repr(align(64))]
    pub struct SmallPoolArray<const SAMPLED: bool> {
        // union u: the pools are never destroyed: cannot really call destructor, since other
        // objects may be freed after we are gone.
        _u: [ManuallyDrop<SmallPool>; NR_SMALL_POOLS],
    }

    /// Number of small size classes.
    pub const NR_SMALL_POOLS: usize = SmallPool::size_to_idx(4 * PAGE_SIZE as u32) as usize + 1;

    impl<const SAMPLED: bool> Default for SmallPoolArray<SAMPLED> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<const SAMPLED: bool> SmallPoolArray<SAMPLED> {
        /// Number of small size classes.
        pub const NR_SMALL_POOLS: usize = NR_SMALL_POOLS;

        const EMPTY: ManuallyDrop<SmallPool> = ManuallyDrop::new(SmallPool::new(1, false));

        /// Returns the pools of every size class, empty.
        pub const fn new() -> Self {
            let mut a = [Self::EMPTY; NR_SMALL_POOLS];
            let mut i = 0;
            while i < NR_SMALL_POOLS {
                a[i] = ManuallyDrop::new(SmallPool::new(SmallPool::idx_to_size(i as u32), SAMPLED));
                i += 1;
            }
            Self { _u: a }
        }

        /// Returns the pool of index `idx`, as a pointer (pools are used through
        /// `*mut SmallPool`, see [`SmallPool::allocate`]). No bounds check: this is a hot path.
        ///
        /// # Safety
        /// `this` is live and `idx < NR_SMALL_POOLS` (callers derive it from
        /// size_to_idx(size) with size <= max_small_allocation).
        #[inline(always)]
        pub unsafe fn get(this: *mut Self, idx: u32) -> *mut SmallPool {
            dassert!((idx as usize) < NR_SMALL_POOLS);
            // SAFETY: by the caller; ManuallyDrop<SmallPool> is repr(transparent).
            unsafe { (&raw mut (*this)._u).cast::<SmallPool>().add(idx as usize) }
        }
    }

    /// The largest size served by the small pools (16 KiB, four pages); larger requests take
    /// whole spans.
    pub const MAX_SMALL_ALLOCATION: usize =
        SmallPool::idx_to_size(NR_SMALL_POOLS as u32 - 1) as usize;

    /// Returns the size to allocate for a sampled small object of `size` bytes: `size` plus one
    /// pointer for its allocation site, or, when that would not leave the pointer room before
    /// the next page boundary, `size` rounded up to whole pages (the site then lives in the
    /// page structure).
    #[cfg(engine_heapprof)]
    pub const fn object_size_with_alloc_site(mut size: usize) -> usize {
        // For page-aligned sizes, allocation_site* lives in page::alloc_site, not with the object.
        let next_page_aligned_size = next_page_aligned(size);
        if next_page_aligned_size - size > size_of::<AllocationSitePtr>() {
            size += size_of::<AllocationSitePtr>();
        } else {
            return next_page_aligned_size;
        }
        size
    }

    #[cfg(engine_heapprof)]
    const _: () = {
        assert!(
            is_page_aligned(MAX_SMALL_ALLOCATION),
            "assuming that max_small_allocation is page aligned so that we don't need to add allocation_site_ptr to objects of size close to it"
        );
        // Ensure that object_size_with_alloc_site() does not exceed max_small_allocation
        assert!(object_size_with_alloc_site(MAX_SMALL_ALLOCATION) == MAX_SMALL_ALLOCATION);
        assert!(object_size_with_alloc_site(MAX_SMALL_ALLOCATION - 1) == MAX_SMALL_ALLOCATION);
        assert!(
            object_size_with_alloc_site(MAX_SMALL_ALLOCATION - size_of::<AllocationSitePtr>() + 1)
                == MAX_SMALL_ALLOCATION
        );
        assert!(
            object_size_with_alloc_site(MAX_SMALL_ALLOCATION - size_of::<AllocationSitePtr>())
                == MAX_SMALL_ALLOCATION
        );
        assert!(
            object_size_with_alloc_site(MAX_SMALL_ALLOCATION - size_of::<AllocationSitePtr>() - 1)
                == MAX_SMALL_ALLOCATION - 1
        );
        assert!(
            object_size_with_alloc_site(MAX_SMALL_ALLOCATION - size_of::<AllocationSitePtr>() - 2)
                == MAX_SMALL_ALLOCATION - 2
        );
    };

    /// An object freed by another thread, waiting in its owner's cross-shard list: the link to
    /// the next one, stored in the object itself. As with `FreeObject`, objects of size
    /// classes such as 12 bytes are not always pointer-aligned, so the struct is `packed`:
    /// every `next` read and write is an unaligned access, which costs the same as an aligned
    /// one on x86-64 and aarch64 and is defined behaviour in Rust. The field is only ever
    /// copied, never borrowed.
    #[repr(C, packed)]
    struct CrossCpuFreeItem {
        next: *mut CrossCpuFreeItem,
    }

    // Kept for the layout of `CpuPages`; not used.
    #[allow(dead_code)]
    #[derive(Clone, Copy)]
    struct Trim {
        offset: u32,
        nr_pages: u32,
    }

    /// Number of free span lists (one per power-of-two span size).
    pub const NR_SPAN_LISTS: usize = 32;

    /// A shard's whole allocator state: the page array, free span lists, small pools, reclaimers and statistics.
    pub struct CpuPages {
        small_pools: SmallPoolArray<false>,
        min_free_pages: u32,
        memory: *mut u8,
        pages: *mut Page,
        nr_pages: u32,
        nr_free_pages: u32,
        current_min_free_pages: u32,
        large_allocation_warning_threshold: usize,
        cpu_id: u32,
        reclaim_hook: Option<ReclaimHook>,
        reclaimers: Vec<*mut Reclaimer>,
        free_spans: [PageList; NR_SPAN_LISTS], // contains aligned spans with span_size == 2^idx
        xcpu_freelist:
            crate::engine::core::cacheline::CacheLineAligned<AtomicPtr<CrossCpuFreeItem>>,
        // The heap profiler's allocation sites: created on first use, never destroyed.
        asu: ManuallyDrop<Option<HashSet<Box<AllocationSite>>>>,
        // Read only on the engine_heapprof paths; a member in every build, so that the layout
        // does not change.
        #[cfg_attr(not(engine_heapprof), allow(dead_code))]
        alloc_site_list_head: AllocationSitePtr, // for easy traversal of the sites from a debugger script
        heap_prof_sampler: ManuallyDrop<Option<Sampler>>,
        // Read only on the engine_heapprof paths; a member in every build, so that the layout
        // does not change.
        #[cfg_attr(not(engine_heapprof), allow(dead_code))]
        sampled_small_pools: SmallPoolArray<true>,
        // The allocator re-enters itself through the thread's `cpu_mem` while a `&mut` to it
        // (or to one of its pools) is live up the stack: user reclaimers free memory, pools
        // allocate spans. The paths that call out
        // (`run_reclaimers`, `SmallPool::add_more_objects`, `trim_free_list`) work on a raw
        // `*mut CpuPages`, and `!Unpin` keeps the remaining `&mut CpuPages` from asserting
        // uniqueness over the object (as for the crate's intrusive types); it never moves.
        _pin: std::marker::PhantomPinned,
    }

    static CPU_ID_GEN: AtomicU32 = AtomicU32::new(0);
    static ALL_CPUS: [AtomicPtr<CpuPages>; MAX_CPUS] =
        [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_CPUS];

    impl CpuPages {
        const fn new() -> Self {
            Self {
                small_pools: SmallPoolArray::new(),
                min_free_pages: (20000000 / PAGE_SIZE) as u32,
                memory: std::ptr::null_mut(),
                pages: std::ptr::null_mut(),
                nr_pages: 0,
                nr_free_pages: 0,
                current_min_free_pages: 0,
                large_allocation_warning_threshold: usize::MAX,
                cpu_id: u32::MAX,
                reclaim_hook: None,
                reclaimers: Vec::new(),
                free_spans: [PageList::new(); NR_SPAN_LISTS],
                xcpu_freelist: crate::engine::core::cacheline::CacheLineAligned(AtomicPtr::new(
                    std::ptr::null_mut(),
                )),
                asu: ManuallyDrop::new(None),
                alloc_site_list_head: std::ptr::null(),
                heap_prof_sampler: ManuallyDrop::new(None),
                sampled_small_pools: SmallPoolArray::new(),
                _pin: std::marker::PhantomPinned,
            }
        }

        fn mem(&self) -> *mut u8 {
            self.memory
        }

        // The set of allocation sites: the counters are Cells, and hashing and equality read only
        // the backtrace.
        #[allow(clippy::mutable_key_type)]
        fn alloc_sites(&mut self) -> &mut HashSet<Box<AllocationSite>> {
            self.asu.get_or_insert_with(HashSet::new)
        }

        fn heap_prof_sampler(&mut self) -> &mut Sampler {
            self.heap_prof_sampler.get_or_insert_with(Sampler::new)
        }

        #[inline(always)]
        fn to_page(&self, p: *const c_void) -> *mut Page {
            // SAFETY: a pointer of this shard's region indexes a page structure of its array.
            unsafe { self.pages.add(Self::page_idx(p)) }
        }

        /// The index in the page array of the page `p` lies in, from the pointer's bits alone
        /// (the shard's memory starts at a multiple of 2^CPU_ID_SHIFT, so for a pointer of this
        /// shard it equals `(p - mem()) / page_size`).
        #[inline(always)]
        fn page_idx(p: *const c_void) -> usize {
            ((p as usize) << (64 - CPU_ID_SHIFT)) >> (64 - CPU_ID_SHIFT + PAGE_BITS as u32)
        }

        /// # Safety
        /// `pages` is this shard's page array, `list_idx < NR_SPAN_LISTS` and `span` is
        /// linked in `free_spans[list_idx]`.
        #[inline(always)]
        unsafe fn unlink(&mut self, pages: *mut Page, list_idx: usize, span: *mut Page) {
            dassert!(list_idx < NR_SPAN_LISTS);
            // SAFETY: by the caller, `idx` is in range (no bounds check: hot path).
            unsafe { self.free_spans.get_unchecked_mut(list_idx).erase(pages, span) };
        }

        /// # Safety
        /// `pages` is this shard's page array, `list_idx < NR_SPAN_LISTS` and `span` is an
        /// unlinked page of it.
        #[inline(always)]
        unsafe fn link(&mut self, pages: *mut Page, list_idx: usize, span: *mut Page) {
            dassert!(list_idx < NR_SPAN_LISTS);
            // SAFETY: by the caller.
            unsafe { self.free_spans.get_unchecked_mut(list_idx).push_front(pages, span) };
        }

        /// # Safety
        /// `[span_start, span_start + nr_pages)` is a free run of this shard's page array and
        /// `nr_pages` is a power of two: every span of the buddy allocator is one, and a `u32`
        /// power of two is at most 2^31, so `index_of(nr_pages) <= 31 < NR_SPAN_LISTS`.
        unsafe fn free_span_no_merge(&mut self, span_start: u32, nr_pages: u32) {
            alloc_assert!(nr_pages != 0);
            // SAFETY: by the caller.
            unsafe { self.link_free_span(self.pages, span_start, nr_pages) };
        }

        /// The body of `free_span_no_merge` after its assertion, for `free_span`, whose
        /// merged size is a power of two and so never zero. The page array is passed in:
        /// the stores to the page links below could alias `self.pages` as far as LLVM knows
        /// (`CpuPages` is `!Unpin`, so `&mut self` is not `noalias`), and it would reload it.
        ///
        /// # Safety
        /// `pages` is this shard's page array; otherwise as for `free_span_no_merge`.
        #[inline(always)]
        unsafe fn link_free_span(&mut self, pages: *mut Page, span_start: u32, nr_pages: u32) {
            dassert!(nr_pages != 0);
            self.nr_free_pages += nr_pages;
            // SAFETY: the span [span_start, span_start + nr_pages) lies within the page array
            // (by the caller); head and tail are the same page for a one-page span, so both are
            // raw places. `index_of(nr_pages) < NR_SPAN_LISTS` by the caller.
            unsafe {
                let span = pg(pages, span_start as usize);
                (*span).free = true;
                (*span).span_size = nr_pages;
                let span_end = pg(pages, (span_start + nr_pages - 1) as usize);
                (*span_end).free = true;
                (*span_end).span_size = nr_pages;
                let idx = index_of(nr_pages);
                self.link(pages, idx as usize, span);
            }
        }

        /// # Safety
        /// `pages` is this shard's page array, `[*span_start, *span_start + *nr_pages)` is a
        /// span of it, `*nr_pages` is a power of two and `idx == index_of(*nr_pages)`.
        #[inline(always)]
        unsafe fn grow_span(
            &mut self,
            pages: *mut Page,
            span_start: &mut u32,
            nr_pages: &mut u32,
            idx: u32,
        ) -> bool {
            let which = (*span_start >> idx) & 1; // 0=lower, 1=upper
            // locate first page of upper buddy or last page of lower buddy
            // examples: span_start = 0x10 nr_pages = 0x08 -> buddy = 0x18  (which = 0)
            //           span_start = 0x18 nr_pages = 0x08 -> buddy = 0x17  (which = 1)
            // delta = which ? -1u : nr_pages
            let delta = ((which ^ 1) << idx) | which.wrapping_neg();
            let buddy = span_start.wrapping_add(delta);
            // SAFETY: the buddy's edge page lies within the page array (page 0 and the page
            // past nr_pages are reserved and never free, which stops the merge at the ends).
            let b = unsafe { pg(pages, buddy as usize) };
            // `free` is read first and `span_size` only when the page is free (the second load is
            // not hoisted: LLVM cannot prove the page readable).
            // SAFETY: as above.
            if unsafe { (*b).free } && unsafe { (*b).span_size } == *nr_pages {
                // SAFETY: the buddy is a free span of size nr_pages, linked in free_spans[idx];
                // idx == index_of(nr_pages) < NR_SPAN_LISTS (a power of two, by the caller).
                unsafe {
                    let other = pg(pages, (*span_start ^ *nr_pages) as usize);
                    self.unlink(pages, idx as usize, other);
                }
                self.nr_free_pages -= *nr_pages; // free_span_no_merge() will restore
                *span_start &= !*nr_pages;
                *nr_pages *= 2;
                return true;
            }
            false
        }

        /// Frees the span and merges it with its free buddies. Nothing in it unwinds or calls
        /// out (no bounds checks; the merged size needs no `nr_pages != 0` check), so it has no
        /// frame and `free` reaches it with a tail call.
        ///
        /// # Safety
        /// `[span_start, span_start + nr_pages)` is a span of this shard's page array that is
        /// not free, and `nr_pages` is a power of two (every buddy span is). Merging keeps both:
        /// the merged span is the aligned pair of two such spans, it lies within the array
        /// (page 0 and the page past the end are never free), so it stays below 2^32 pages
        /// and `index_of` of its size below `NR_SPAN_LISTS`.
        unsafe fn free_span(&mut self, mut span_start: u32, mut nr_pages: u32) {
            let pages = self.pages;
            let mut idx = index_of(nr_pages);
            // SAFETY: by the caller, and each merge doubles nr_pages with idx += 1.
            unsafe {
                while self.grow_span(pages, &mut span_start, &mut nr_pages, idx) {
                    idx += 1;
                }
                self.link_free_span(pages, span_start, nr_pages);
            }
        }

        // Internal, used during startup. Span is not aligned so needs to be broken up
        fn free_span_unaligned(&mut self, mut span_start: u32, mut nr_pages: u32) {
            while nr_pages != 0 {
                let start_nr_bits = if span_start != 0 { span_start.trailing_zeros() } else { 32 };
                let size_nr_bits = nr_pages.trailing_zeros();
                let now = 1u32 << start_nr_bits.min(size_nr_bits);
                // SAFETY: `now` is a power of two that divides span_start, and the run lies in
                // the page array (the callers free pages of the array they just set up).
                unsafe { self.free_span(span_start, now) };
                span_start += now;
                nr_pages -= now;
            }
        }

        fn find_and_unlink_span(&mut self, n_pages: u32) -> *mut Page {
            let mut idx = index_of(n_pages);
            if n_pages as u64 >= (2u64 << idx) {
                return std::ptr::null_mut();
            }
            while (idx as usize) < NR_SPAN_LISTS && self.free_spans[idx as usize].empty() {
                idx += 1;
            }
            if idx as usize == NR_SPAN_LISTS {
                if self.initialize() {
                    return self.find_and_unlink_span(n_pages);
                }
                return std::ptr::null_mut();
            }
            // SAFETY: idx < NR_SPAN_LISTS (checked above) and free_spans[idx] is not empty, so
            // its front is a linked span of the array.
            unsafe {
                let span = self.free_spans[idx as usize].front(self.pages);
                self.unlink(self.pages, idx as usize, span);
                span
            }
        }

        fn find_and_unlink_span_reclaiming(&mut self, n_pages: u32) -> *mut Page {
            loop {
                let span = self.find_and_unlink_span(n_pages);
                if !span.is_null() {
                    return span;
                }
                // SAFETY: `self` is the thread's live cpu_pages; the reclaimers re-enter it.
                if unsafe { Self::run_reclaimers(self, ReclaimerScope::Sync, n_pages as usize) }
                    == ReclaimingResult::ReclaimedNothing
                {
                    return std::ptr::null_mut();
                }
            }
        }

        /// Runs reclaim if memory is low: the test inline, the work out of line, so that the
        /// common path needs no frame (LLVM does not shrink-wrap the function once the drain loop
        /// is inlined into it).
        #[inline(always)]
        fn maybe_reclaim(&mut self) {
            if self.nr_free_pages < self.current_min_free_pages {
                self.reclaim_to_min_free_pages();
            }
        }

        #[cold]
        #[inline(never)]
        fn reclaim_to_min_free_pages(&mut self) {
            self.drain_cross_cpu_freelist();
            if self.nr_free_pages < self.current_min_free_pages {
                let n = (self.current_min_free_pages - self.nr_free_pages) as usize;
                // SAFETY: as in find_and_unlink_span_reclaiming.
                unsafe { Self::run_reclaimers(self, ReclaimerScope::Sync, n) };
            }
            if self.nr_free_pages < self.current_min_free_pages {
                self.schedule_reclaim();
            }
        }

        fn allocate_large_and_trim(&mut self, n_pages: u32, should_sample: bool) -> *mut c_void {
            // Avoid exercising the reclaimers for requests we'll not be able to satisfy
            // nr_pages might be zero during startup, so check for that too
            if self.nr_pages != 0 && n_pages >= self.nr_pages {
                return std::ptr::null_mut();
            }
            let span = self.find_and_unlink_span_reclaiming(n_pages);
            if span.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: `span` heads a free span of the page array and `span_end` (below) is its
            // last page, the same page as `span` when the span is one page long; both stay raw
            // pointers and each access, here and below, is a raw place.
            let mut span_size = unsafe { (*span).span_size };
            let span_idx = idx_of(self.pages, span);
            self.nr_free_pages -= span_size;
            while span_size >= n_pages * 2 {
                span_size /= 2;
                let other_span_idx = span_idx + span_size as usize;
                // SAFETY: the upper half of the span just unlinked, a power of two since the
                // span's size is one.
                unsafe { self.free_span_no_merge(other_span_idx as u32, span_size) };
            }
            // SAFETY: as above; `span_idx + span_size - 1` is the last page of the span kept, inside
            // the page array.
            unsafe {
                let span_end = pg(self.pages, span_idx + span_size as usize - 1);
                (*span_end).free = false;
                (*span_end).span_size = span_size;
                (*span).free = false;
                (*span).span_size = span_size;
                (*span).pool = std::ptr::null_mut();
            }
            #[cfg(engine_heapprof)]
            {
                if should_sample {
                    let alloc_site = self.add_alloc_site(span_size as usize * PAGE_SIZE);
                    // SAFETY: as above.
                    unsafe { (*span).alloc_site = alloc_site };
                } else {
                    // SAFETY: as above.
                    unsafe { (*span).alloc_site = std::ptr::null() };
                }
            }
            let _ = should_sample;
            self.maybe_reclaim();
            // SAFETY: the span lies within this shard's memory.
            unsafe { self.mem().add(span_idx * PAGE_SIZE) as *mut c_void }
        }

        #[cold]
        fn warn_large_allocation(&mut self, size: usize) {
            alloc_stats::increment_local(alloc_stats::Types::LargeAllocs, 1);
            crate::log_warn!(
                ENGINE_MEMORY_LOGGER,
                "oversized allocation: {} bytes. This is non-fatal, but could lead to latency and/or fragmentation issues. Please report: at {}",
                size,
                current_backtrace()
            );
            self.large_allocation_warning_threshold =
                (self.large_allocation_warning_threshold as f64 * 1.618) as usize; // prevent spam
        }

        // Called only on the engine_heapprof paths; compiled in every build.
        #[cfg_attr(not(engine_heapprof), allow(dead_code))]
        fn add_alloc_site(&mut self, allocated_size: usize) -> AllocationSitePtr {
            let alloc_site = get_allocation_site();
            if !alloc_site.is_null() {
                // SAFETY: allocation sites live in alloc_sites boxes until their count drops to 0.
                let s = unsafe { &*alloc_site };
                s.count.set(s.count.get() + 1);
                s.size.set(s.size.get() + self.heap_prof_sampler().sample_size(allocated_size));
            }

            alloc_site
        }

        // Called only on the engine_heapprof paths; compiled in every build.
        #[cfg_attr(not(engine_heapprof), allow(dead_code))]
        fn remove_alloc_site(&mut self, alloc_site: AllocationSitePtr, deallocated_size: usize) {
            if !alloc_site.is_null() {
                // SAFETY: allocation sites live in alloc_sites boxes until their count drops to 0.
                let s = unsafe { &*alloc_site };
                s.count.set(s.count.get() - 1);
                let sample_size = self.heap_prof_sampler().sample_size(deallocated_size);
                // prevent underflow in case sample rate changed
                s.size.set(
                    s.size.get()
                        - if s.size.get() < sample_size { s.size.get() } else { sample_size },
                );
                if s.count.get() == 0 {
                    if !s.prev.get().is_null() {
                        // SAFETY: linked sites are alive.
                        unsafe { (*s.prev.get()).next.set(s.next.get()) };
                    }
                    if !s.next.get().is_null() {
                        // SAFETY: linked sites are alive.
                        unsafe { (*s.next.get()).prev.set(s.prev.get()) };
                    }
                    if self.alloc_site_list_head == alloc_site {
                        self.alloc_site_list_head = s.next.get();
                    }

                    self.alloc_sites().remove(s);
                }
            }
        }

        #[inline(always)]
        fn maybe_sample(&mut self, size: usize) -> bool {
            #[cfg(engine_heapprof)]
            {
                self.heap_prof_sampler().maybe_sample(size)
            }
            #[cfg(not(engine_heapprof))]
            {
                let _ = size;
                false
            }
        }

        #[inline(always)]
        fn definitely_sample(&mut self, size: usize) -> bool {
            #[cfg(engine_heapprof)]
            {
                self.heap_prof_sampler().definitely_sample(size)
            }
            #[cfg(not(engine_heapprof))]
            {
                let _ = size;
                false
            }
        }

        #[inline(always)]
        fn check_large_allocation(&mut self, size: usize) {
            if size >= self.large_allocation_warning_threshold {
                self.warn_large_allocation(size);
            }
        }

        /// Allocates `n_pages` pages (may run the reclaimers, which re-enter the allocator).
        ///
        /// # Safety
        /// `this` must be the thread's live cpu_pages.
        #[inline(always)]
        unsafe fn allocate_large(
            this: *mut Self,
            n_pages: u32,
            should_sample: bool,
        ) -> *mut c_void {
            // SAFETY: `this` is live by the caller; each call borrows it only for its duration
            // (`CpuPages` is `!Unpin`, see `_pin`).
            unsafe {
                (*this).check_large_allocation(n_pages as usize * PAGE_SIZE);
                (*this).allocate_large_and_trim(n_pages, should_sample)
            }
        }

        /// Allocates `n_pages` pages; `_align_pages` is not used: the result is a buddy span,
        /// aligned to its own size (`n_pages` rounded up to a power of two), so it is aligned
        /// to `_align_pages` only when that size is at least `_align_pages`.
        ///
        /// # Safety
        /// As for [`CpuPages::allocate_large`].
        unsafe fn allocate_large_aligned(
            this: *mut Self,
            _align_pages: u32,
            n_pages: u32,
            should_sample: bool,
        ) -> *mut c_void {
            // SAFETY: as in allocate_large.
            unsafe {
                (*this).check_large_allocation(n_pages as usize * PAGE_SIZE);
                // buddy allocation is always aligned
                (*this).allocate_large_and_trim(n_pages, should_sample)
            }
        }

        fn free_large(&mut self, ptr: *mut c_void) {
            let idx = ((ptr as usize - self.mem() as usize) / PAGE_SIZE) as Pageidx;
            // SAFETY: `ptr` is a large allocation of this shard, so `idx` heads a span of the
            // page array.
            let span = unsafe { pg(self.pages, idx as usize) };
            // SAFETY: as above.
            unsafe { self.free_large_span(idx, span) };
        }

        /// The rest of `free_large` once the span's head page is known.
        ///
        /// # Safety
        /// `span` is `&pages[idx]` and heads a live large allocation of this shard.
        #[inline(always)]
        unsafe fn free_large_span(&mut self, idx: Pageidx, span: *mut Page) {
            // SAFETY: by the caller.
            let sz = unsafe { (*span).span_size };
            #[cfg(engine_heapprof)]
            {
                // SAFETY: as above.
                let alloc_site = unsafe { (*span).alloc_site };
                if !alloc_site.is_null() {
                    self.remove_alloc_site(alloc_site, sz as usize * PAGE_SIZE);
                }
            }
            // SAFETY: a large allocation is a buddy span of the array (power-of-two size).
            unsafe { self.free_span(idx, sz) };
        }

        fn object_size(&self, ptr: *mut c_void) -> usize {
            // SAFETY: `ptr` is an allocation of this shard, so its page structure exists.
            let span = unsafe { &*self.to_page(ptr) };
            if !span.pool.is_null() {
                // SAFETY: the pool pointer of a small-object page is a live small_pool.
                let pool = unsafe { &*span.pool };
                #[allow(unused_mut)]
                let mut s = pool.object_size() as usize;
                #[cfg(engine_heapprof)]
                {
                    if pool.is_sampled_pool() {
                        // We must not allow the object to be extended onto the allocation_site_ptr field.
                        if !pool.objects_page_aligned() {
                            s -= size_of::<AllocationSitePtr>();
                        }
                    }
                }
                s
            } else {
                span.span_size as usize * PAGE_SIZE
            }
        }

        fn free_cross_cpu(cpu_id: u32, ptr: *mut c_void) {
            if !LIVE_CPUS[cpu_id as usize].load(Ordering::Relaxed) {
                // The owning thread is gone: leak the object. Only happens in tests that start and
                // stop reactors.
                return;
            }
            let p = ptr as *mut CrossCpuFreeItem;
            // SAFETY: a live cpu's cpu_pages stays registered in all_cpus.
            let list =
                unsafe { &(*ALL_CPUS[cpu_id as usize].load(Ordering::Relaxed)).xcpu_freelist.0 };
            let mut old = list.load(Ordering::Relaxed);
            loop {
                // SAFETY: `p` is the freed block, owned by this call until it is published.
                unsafe { (*p).next = old };
                match list.compare_exchange_weak(old, p, Ordering::Release, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(cur) => old = cur,
                }
            }
            alloc_stats::increment(alloc_stats::Types::CrossCpuFrees, 1);
        }

        fn drain_cross_cpu_freelist(&mut self) -> bool {
            if self.xcpu_freelist.0.load(Ordering::Relaxed).is_null() {
                return false;
            }
            let mut p = self.xcpu_freelist.0.swap(std::ptr::null_mut(), Ordering::Acquire);
            while !p.is_null() {
                // SAFETY: items of the cross-cpu list are blocks of this shard pushed by free_cross_cpu.
                let n = unsafe { (*p).next };
                alloc_stats::increment_local(alloc_stats::Types::Frees, 1);
                self.free(p as *mut c_void);
                p = n;
            }
            true
        }

        #[inline(always)]
        fn free(&mut self, ptr: *mut c_void) {
            // SAFETY: `ptr` is an allocation of this shard, so its page structure exists.
            let span = unsafe { &*self.to_page(ptr) };
            if !span.pool.is_null() {
                // The pool pointer of a small-object page is a live small_pool of this shard.
                let pool = span.pool;
                #[cfg(engine_heapprof)]
                {
                    // SAFETY: as above; the reads end before remove_alloc_site.
                    if unsafe { (*pool).is_sampled_pool() } {
                        // SAFETY: as above.
                        let (alloc_site, object_size) =
                            unsafe { (*(*pool).alloc_site_holder(ptr), (*pool).object_size()) };
                        self.remove_alloc_site(alloc_site, object_size as usize);
                    }
                }
                // SAFETY: as above; `ptr` is a block of that pool.
                unsafe { SmallPool::deallocate(pool, ptr) };
            } else {
                self.free_large(ptr);
            }
        }

        #[cfg_attr(engine_heapprof, allow(unused_mut))]
        fn free_sized(&mut self, ptr: *mut c_void, mut size: usize) {
            #[cfg(engine_heapprof)]
            {
                // sized free can avoid accessing the `page` structure as an optimization.
                // With memory sampling we always need to check the pool though to see
                // whether this allocation was sampled. Hence, just defer to the non-sized
                // implementation
                let _ = size;
                self.free(ptr);
            }
            #[cfg(not(engine_heapprof))]
            {
                // match action on allocate() so hit the right pool
                if size <= size_of::<FreeObject>() {
                    size = size_of::<FreeObject>();
                }
                if size <= MAX_SMALL_ALLOCATION {
                    // SAFETY: size <= max_small_allocation gives an index in range, and a
                    // sized free names the size class `ptr` was allocated from.
                    unsafe {
                        let pool = SmallPoolArray::get(
                            &raw mut self.small_pools,
                            SmallPool::size_to_idx(size as u32),
                        );
                        SmallPool::deallocate(pool, ptr);
                    }
                } else {
                    self.free_large(ptr);
                }
            }
        }

        // Is the passed pointer a local pointer, i.e., allocated on the current shard from the
        // per-shard allocator.
        #[inline(always)]
        fn is_local_pointer(ptr: *const c_void) -> bool {
            (ptr as usize & CPU_ID_AND_MEM_BASE_MASK)
                == with_allocator_tls!(local_expected_cpu_id, |c: &Cell<usize>| c.get())
        }

        // Try to execute free on the fast path, which succeeds if:
        //
        // 1) The pointer is local to this shard
        // 2) The pointer is from a small pool
        // 3) The small pool is not sampled
        //
        // In this case, complete the de-allocation and return true.
        // Otherwise, modify nothing and return false.
        #[inline(always)]
        //
        // The three thread-locals it reads (is_local_pointer's local_expected_cpu_id,
        // get_cpu_mem's cpu_mem_ptr and the frees counter) are reached from one base here,
        // which is three instructions shorter than one offset per field and as fast: unlike
        // the allocation path, no load of the free path waits on another through the base
        // (see `with_allocator_tls!`).
        fn try_free_fastpath(ptr: *mut c_void) -> bool {
            with_allocator_tls_block(|tls| {
                // is_local_pointer(ptr)
                if likely(
                    (ptr as usize & CPU_ID_AND_MEM_BASE_MASK) == tls.local_expected_cpu_id.get(),
                ) {
                    // SAFETY: a local pointer indexes this shard's page array, and cpu_mem_ptr
                    // is set (init_cpu_mem) before any pointer is local.
                    let page = unsafe { (*tls.cpu_mem_ptr.get()).to_page(ptr) };
                    // SAFETY: as above.
                    let pool = unsafe { (*page).pool };
                    // SAFETY: a non-null pool pointer of a page is a live small_pool.
                    if likely(!pool.is_null() && !unsafe { &*pool }.is_sampled_pool()) {
                        // alloc_stats::increment_local(alloc_stats::types::frees)
                        // SAFETY: the counters are only touched by this thread, through no
                        // reference that outlives one access.
                        unsafe { (*tls.stats.get())[alloc_stats::Types::Frees as usize] += 1 };
                        // SAFETY: as above.
                        unsafe { SmallPool::deallocate(pool, ptr) };
                        return true;
                    }
                    if pool.is_null() {
                        // A large allocation of this shard: what free_slowpath does with it
                        // (the pointer is local, so count the free; cpu_pages::free and
                        // free(ptr, size) both end in free_large), without testing again that
                        // the pointer is local and loading cpu_mem and its page a second time.
                        // SAFETY: as above; `ptr` heads a large allocation of this shard and
                        // `page` is its page.
                        unsafe { Self::free_large_local(ptr, tls, page) };
                        return true;
                    }
                }
                false
            })
        }

        /// The large-allocation leg of `free_slowpath` for a local pointer, entered from
        /// `try_free_fastpath`. `extern "C"` (it cannot unwind), so the inlined free paths
        /// reach it with a tail call.
        ///
        /// The page is passed in and its index taken from the pointer's bits, so the span's
        /// size is one load away from the fast path's page, not three (cpu_mem, mem(), pages).
        ///
        /// # Safety
        /// `tls` is the calling thread's block, `ptr` a live large allocation of this shard and
        /// `page` its page (`to_page(ptr)`).
        #[inline(never)]
        unsafe extern "C" fn free_large_local(
            ptr: *mut c_void,
            tls: &AllocatorTls,
            page: *mut Page,
        ) {
            // alloc_stats::increment_local(alloc_stats::types::frees)
            // SAFETY: the counters are only touched by this thread, through no reference that
            // outlives one access.
            unsafe { (*tls.stats.get())[alloc_stats::Types::Frees as usize] += 1 };
            // SAFETY: the thread's cpu_mem (set before any pointer is local), borrowed for this
            // one call; `page` is `pages[page_idx(ptr)]` and heads the allocation `ptr` (a large
            // allocation starts at its span's first page).
            unsafe {
                (*tls.cpu_mem_ptr.get()).free_large_span(Self::page_idx(ptr) as Pageidx, page)
            };
        }

        /// `extern "C"`, so that it cannot unwind (as `free_slowpath_no_size`): the free slow
        /// paths reach it with a tail call and need no frame.
        #[inline(never)]
        extern "C" fn do_foreign_free(ptr: *mut c_void) {
            // handles:
            // 1) non-engine pointers
            // 2) cross-shard frees
            // 3) null pointer

            if ptr.is_null() {
                return;
            }

            if !is_engine_memory(ptr) {
                if is_reactor_thread() {
                    alloc_stats::increment_local(alloc_stats::Types::ForeignCrossFrees, 1);
                } else {
                    alloc_stats::increment(alloc_stats::Types::ForeignFrees, 1);
                }
                if let Some(f) = libc!(LIBC_FREE_FUNC, FreeFuncType) {
                    // SAFETY: `ptr` was allocated by libc's allocator, which frees it.
                    unsafe { f(ptr) };
                }
                return;
            }
            Self::free_cross_cpu(object_cpu_id(ptr), ptr);
        }

        fn shrink(&mut self, ptr: *mut c_void, new_size: usize) {
            assert!(object_cpu_id(ptr) == self.cpu_id);
            let span_ptr = self.to_page(ptr);
            // SAFETY: `ptr` is an allocation of this shard, so its page structure exists.
            let span = unsafe { &mut *span_ptr };
            if !span.pool.is_null() {
                return;
            }
            let old_size_pages = span.span_size as usize;
            let mut new_size_pages = old_size_pages;
            while new_size_pages / 2 * PAGE_SIZE >= new_size {
                new_size_pages /= 2;
            }
            if new_size_pages == old_size_pages {
                return;
            }
            #[cfg(engine_heapprof)]
            {
                let alloc_site = span.alloc_site;
                if !alloc_site.is_null() {
                    // SAFETY: allocation sites live until their count drops to 0.
                    let s = unsafe { &*alloc_site };
                    s.size.set(s.size.get() - span.span_size as usize * PAGE_SIZE);
                    s.size.set(s.size.get() + new_size_pages * PAGE_SIZE);
                }
            }
            span.span_size = new_size_pages as u32;
            // SAFETY: the span covers old_size_pages pages.
            let last = unsafe { &mut *span_ptr.add(new_size_pages - 1) };
            last.free = false;
            last.span_size = new_size_pages as u32;
            let idx = idx_of(self.pages, span_ptr) as Pageidx;
            self.free_span_unaligned(
                idx + new_size_pages as u32,
                (old_size_pages - new_size_pages) as u32,
            );
        }

        fn is_initialized(&self) -> bool {
            self.nr_pages != 0
        }

        fn initialize(&mut self) -> bool {
            if self.is_initialized() {
                return false;
            }
            self.cpu_id = CPU_ID_GEN.fetch_add(1, Ordering::Relaxed);
            with_allocator_tls!(local_expected_cpu_id, |c: &Cell<usize>| c
                .set(((self.cpu_id as u64) << CPU_ID_SHIFT) as usize | mem_base() as usize));
            assert!((self.cpu_id as usize) < MAX_CPUS);
            ALL_CPUS[self.cpu_id as usize].store(self, Ordering::Relaxed);
            // SAFETY: the shard's window lies inside the reserved mem_base region.
            let base = unsafe { mem_base().add((self.cpu_id as usize) << CPU_ID_SHIFT) };
            let size: usize = 32 << 20; // Small size for bootstrap
            // SAFETY: mapping fixed memory inside our own PROT_NONE reservation.
            let r = unsafe {
                mmap(
                    base as *mut c_void,
                    size,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    -1,
                    0,
                )
            };
            if r == MAP_FAILED {
                // SAFETY: abort has no preconditions.
                unsafe { abort() };
            }
            maybe_enable_transparent_hugepages(base as *mut c_void, size);
            self.pages = base as *mut Page;
            self.memory = base;
            self.nr_pages = (size / PAGE_SIZE) as u32;
            // we reserve the end page so we don't have to special case
            // the last span.
            let mut reserved = crate::engine::core::align::align_up_usize(
                size_of::<Page>() * (self.nr_pages as usize + 1),
                PAGE_SIZE,
            ) / PAGE_SIZE;
            reserved = 1usize << log2ceil_usize(reserved);
            // SAFETY: the reserved pages and the sentinel past nr_pages lie inside the mapping,
            // which holds the page array at its start.
            unsafe {
                for i in 0..reserved {
                    (*pg(self.pages, i)).free = false;
                }
                (*pg(self.pages, self.nr_pages as usize)).free = false;
            }
            self.free_span_unaligned(reserved as u32, self.nr_pages - reserved as u32);
            LIVE_CPUS[self.cpu_id as usize].store(true, Ordering::Relaxed);
            true
        }

        fn replace_memory_backing(
            &mut self,
            alloc_sys_mem: &AllocateSystemMemoryFn,
        ) -> Result<(), ExceptionPtr> {
            // We would like to use ::mremap() to atomically replace the old anonymous
            // memory with hugetlbfs backed memory, but mremap() does not support hugetlbfs
            // (for no reason at all).  So we must copy the anonymous memory to some other
            // place, map hugetlbfs in place, and copy it back, without modifying it during
            // the operation.
            let bytes = self.nr_pages as usize * PAGE_SIZE;
            let old_mem = self.mem();
            // SAFETY: no MAP_FIXED, so the kernel chooses where the mapping goes.
            let relocated_old_mem = unsafe {
                crate::engine::core::posix::mmap_anonymous(
                    std::ptr::null_mut(),
                    bytes,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE,
                )
            }?;
            // SAFETY: both regions are `bytes` long, mapped read-write and disjoint.
            unsafe { std::ptr::copy_nonoverlapping(old_mem, relocated_old_mem.get(), bytes) };
            alloc_sys_mem(old_mem as *mut c_void, bytes)?.release();
            // SAFETY: as above; old_mem is now backed by the new mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(relocated_old_mem.get() as *const u8, old_mem, bytes)
            };
            Ok(())
        }

        fn do_resize(
            &mut self,
            new_size: usize,
            alloc_sys_mem: &AllocateSystemMemoryFn,
        ) -> Result<(), ExceptionPtr> {
            let new_pages = new_size / PAGE_SIZE;
            if new_pages <= self.nr_pages as usize {
                return Ok(());
            }
            let old_size = self.nr_pages as usize * PAGE_SIZE;
            // SAFETY: the shard's window extends past the current mapping.
            let mmap_start = unsafe { self.memory.add(old_size) };
            let mmap_size = new_size - old_size;
            let mem = alloc_sys_mem(mmap_start as *mut c_void, mmap_size)?;
            mem.release();
            maybe_enable_transparent_hugepages(mmap_start as *mut c_void, mmap_size);
            // one past last page structure is a sentinel
            let new_page_array_pages = crate::engine::core::align::align_up_usize(
                size_of::<Page>() * (new_pages + 1),
                PAGE_SIZE,
            ) / PAGE_SIZE;
            let new_page_array
                // SAFETY: `self` is the thread's live cpu_pages.
                = unsafe { Self::allocate_large(self, new_page_array_pages as u32, false) } as *mut Page;
            if new_page_array.is_null() {
                return Err(make_exception_ptr(crate::engine::core::future::BadAlloc));
            }
            // SAFETY: the new array has room for new_pages + 1 entries; the old one holds nr_pages.
            unsafe {
                std::ptr::copy_nonoverlapping(self.pages, new_page_array, self.nr_pages as usize);
                // mark new one-past-last page as taken to avoid boundary conditions
                (*new_page_array.add(new_pages)).free = false;
            }
            let old_pages = self.pages as *mut u8;
            let old_nr_pages = self.nr_pages;
            let mut old_pages_size = crate::engine::core::align::align_up_usize(
                size_of::<Page>() * (self.nr_pages as usize + 1),
                PAGE_SIZE,
            );
            old_pages_size = 1usize << log2ceil_usize(old_pages_size);
            self.pages = new_page_array;
            self.nr_pages = new_pages as u32;
            let mut old_pages_start = (old_pages as usize - self.memory as usize) / PAGE_SIZE;
            if old_pages_start == 0 {
                // keep page 0 allocated
                old_pages_start = 1;
                old_pages_size -= PAGE_SIZE;
            }
            if old_pages_size != 0 {
                self.free_span_unaligned(
                    old_pages_start as u32,
                    (old_pages_size / PAGE_SIZE) as u32,
                );
            }
            self.free_span_unaligned(old_nr_pages, new_pages as u32 - old_nr_pages);
            Ok(())
        }

        fn resize(
            &mut self,
            mut new_size: usize,
            alloc_memory: &AllocateSystemMemoryFn,
        ) -> Result<(), ExceptionPtr> {
            new_size = crate::engine::core::align::align_down_usize(new_size, HUGE_PAGE_SIZE);
            while (self.nr_pages as usize * PAGE_SIZE) < new_size {
                // don't reallocate all at once, since there might not
                // be enough free memory available to relocate the pages array
                let tmp_size = new_size.min(4 * self.nr_pages as usize * PAGE_SIZE);
                self.do_resize(tmp_size, alloc_memory)?;
            }
            Ok(())
        }

        /// Runs the reclaimers until `n_pages` more pages are free.
        ///
        /// # Safety
        /// `this` must be the thread's live cpu_pages. The reclaimers are user code that
        /// re-enters the allocator (freeing, registering reclaimers), so no reference into
        /// `*this` is held across their calls: every field is read through `this`.
        #[inline(never)]
        unsafe fn run_reclaimers(
            this: *mut Self,
            scope: ReclaimerScope,
            n_pages: usize,
        ) -> ReclaimingResult {
            // SAFETY: `this` is live by the caller; each access, here and every `(*this)`
            // below, is a short read of one field, never held across a reclaimer call.
            let nr_free_pages = || unsafe { (*this).nr_free_pages } as usize;
            // SAFETY: as above, a short read of one field through `this`.
            let target =
                (nr_free_pages() + n_pages).max(unsafe { (*this).min_free_pages } as usize);
            let mut result = ReclaimingResult::ReclaimedNothing;
            while nr_free_pages() < target {
                let mut made_progress = false;
                alloc_stats::increment_local(alloc_stats::Types::Reclaims, 1);
                // A reclaimer may register or unregister a reclaimer while it runs, so the
                // vector is re-read by index on every step (no iterator held across user code), so such
                // a change skips or repeats one reclaimer instead.
                let mut i = 0;
                // SAFETY: as above, a short read through `this`; no reclaimer runs during it.
                while i < unsafe { (*this).reclaimers.len() } {
                    // SAFETY: as above; `i` was checked against the length just read, and the shared
                    // borrow of the vector ends with the copy of the entry, before the reclaimer runs.
                    let r = unsafe { (&(*this).reclaimers)[i] };
                    // SAFETY: registered reclaimers unregister themselves when dropped, so a
                    // listed one is live; it is called with no reference into `*this` held.
                    let r = unsafe { &mut *r };
                    if r.scope() >= scope {
                        made_progress |= r.do_reclaim((target - nr_free_pages()) * PAGE_SIZE)
                            == ReclaimingResult::ReclaimedSomething;
                    }
                    i += 1;
                }
                if !made_progress {
                    return result;
                }
                result = ReclaimingResult::ReclaimedSomething;
            }
            result
        }

        fn schedule_reclaim(&mut self) {
            self.current_min_free_pages = 0;
            let this: *mut CpuPages = self;
            if let Some(hook) = &self.reclaim_hook {
                hook(Box::new(move || {
                    // SAFETY: the hook runs on this shard, whose cpu_pages lives as long as the
                    // thread; fields are read and written through the raw pointer.
                    let (nr_free, min_free) =
                        unsafe { ((*this).nr_free_pages, (*this).min_free_pages) };
                    if nr_free < min_free {
                        let n = (min_free - nr_free) as usize;
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            // SAFETY: as above; the reclaimers re-enter through the same object.
                            unsafe { CpuPages::run_reclaimers(this, ReclaimerScope::Async, n) };
                        }));
                        if let Err(e) = r {
                            // SAFETY: as above.
                            unsafe { (*this).current_min_free_pages = (*this).min_free_pages };
                            std::panic::resume_unwind(e);
                        }
                    }
                    // SAFETY: as above.
                    unsafe { (*this).current_min_free_pages = (*this).min_free_pages };
                }));
            }
        }

        fn memory_layout(&self) -> MemoryLayout {
            assert!(self.is_initialized());
            MemoryLayout {
                start: self.memory as usize,
                end: self.memory as usize + self.nr_pages as usize * PAGE_SIZE,
            }
        }

        fn set_reclaim_hook(&mut self, hook: ReclaimHook) {
            self.reclaim_hook = Some(hook);
            self.current_min_free_pages = self.min_free_pages;
        }

        fn set_min_free_pages(&mut self, pages: usize) -> Result<(), ExceptionPtr> {
            if pages > u32::MAX as usize {
                return Err(make_exception_ptr(crate::engine::core::future::RuntimeError::new(
                    "Number of pages too large".to_string(),
                )));
            }
            self.min_free_pages = pages as u32;
            self.maybe_reclaim();
            Ok(())
        }
    }

    /// The teardown of a thread's pages at thread exit. The per-thread `CpuPages` itself has no
    /// destructor: it is held in `ManuallyDrop` (its `reclaimers` and `reclaim_hook` would otherwise
    /// give it drop glue, and `thread_local!` would register a destructor on the first access,
    /// which allocates from inside the allocator and frees the vector while later thread-exit
    /// destructors still free into the pools). This guard is registered once the pages are
    /// initialized and runs the pages' teardown at thread exit.
    struct CpuPagesDestructor;

    impl Drop for CpuPagesDestructor {
        fn drop(&mut self) {
            let mem = get_cpu_mem_raw();
            // SAFETY: the thread's cpu_pages outlives this guard (it is never destroyed).
            let mem = unsafe { &*mem };
            if mem.is_initialized() {
                LIVE_CPUS[mem.cpu_id as usize].store(false, Ordering::Relaxed);
            }
        }
    }

    thread_local! {
        static CPU_MEM: UnsafeCell<ManuallyDrop<CpuPages>> = const { UnsafeCell::new(ManuallyDrop::new(CpuPages::new())) };
        static CPU_PAGES_DESTRUCTOR: CpuPagesDestructor = const { CpuPagesDestructor };
    }
    // The per-thread pool pointer lives in the `cpu_mem_ptr` field of AllocatorTls.

    #[inline(always)]
    fn get_cpu_mem_raw() -> *mut CpuPages {
        // ManuallyDrop<CpuPages> is repr(transparent).
        CPU_MEM.with(|c| c.get().cast::<CpuPages>())
    }

    /// Sets the heap profiler's sampling interval on the current shard, in bytes (on average one
    /// sample per `sample_rate` bytes allocated); 0 disables profiling. Turning it on or off
    /// logs an info message on the engine logger. While profiling is on, a new non-zero rate is
    /// ignored with a warning: turn it off first. The thread's pool must be set up.
    #[cfg(engine_heapprof)]
    pub fn set_heap_profiling_sampling_rate(sample_rate: usize) {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        let current_sample_rate =
            unsafe { (*get_cpu_mem()).heap_prof_sampler().sampling_interval() } != 0;
        if sample_rate != 0 {
            if !current_sample_rate {
                crate::log_info!(
                    engine_logger(),
                    "Enabling heap profiler - using {} bytes sampling rate",
                    sample_rate
                );
            } else {
                crate::log_warn!(
                    engine_logger(),
                    "Ignoring change to heap profiler sample rate as heap profiling is already turned on"
                );
                return;
            }
        } else if current_sample_rate {
            crate::log_info!(engine_logger(), "Disabling heap profiler");
        }
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).heap_prof_sampler().set_sampling_interval(sample_rate as u64) };
    }

    /// Returns the heap profiler's sampling interval on the current shard, in bytes (0 when
    /// profiling is off).
    #[cfg(engine_heapprof)]
    pub fn get_heap_profiling_sample_rate() -> usize {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).heap_prof_sampler().sampling_interval() as usize }
    }

    #[cfg(engine_heapprof)]
    thread_local! {
        static SCOPED_HEAP_PROFILING_EMBED_COUNT: Cell<i64> = const { Cell::new(0) };
    }

    #[cfg(engine_heapprof)]
    impl ScopedHeapProfiling {
        /// Counts a nested guard on this thread and calls
        /// [`set_heap_profiling_sampling_rate`]`(sample_rate)` (so a nested guard's rate is
        /// ignored with a warning while profiling is on). Dropping the last guard of the thread
        /// sets the rate to 0.
        pub fn new(sample_rate: usize) -> Self {
            SCOPED_HEAP_PROFILING_EMBED_COUNT.with(|c| c.set(c.get() + 1));
            set_heap_profiling_sampling_rate(sample_rate);
            Self { _private: () }
        }
    }

    #[cfg(engine_heapprof)]
    impl Drop for ScopedHeapProfiling {
        fn drop(&mut self) {
            let n = SCOPED_HEAP_PROFILING_EMBED_COUNT.with(|c| {
                c.set(c.get() - 1);
                c.get()
            });
            if n == 0 {
                set_heap_profiling_sampling_rate(0);
            }
        }
    }

    /// Heap profiling is not compiled in (no `engine_heapprof`): logs a warning on the engine
    /// logger, whatever the rate, and does nothing else.
    #[cfg(not(engine_heapprof))]
    pub fn set_heap_profiling_sampling_rate(_enable: usize) {
        crate::log_warn!(
            engine_logger(),
            "Engine compiled without heap profiling support, heap profiler not supported; build with `--cfg engine_heapprof` to add heap profiling support"
        );
    }

    /// Heap profiling is not compiled in: always 0, without logging.
    #[cfg(not(engine_heapprof))]
    pub fn get_heap_profiling_sample_rate() -> usize {
        // don't log here, called on all paths
        0
    }

    #[cfg(not(engine_heapprof))]
    impl ScopedHeapProfiling {
        /// Heap profiling is not compiled in: logs the warning of
        /// [`set_heap_profiling_sampling_rate`] and returns a guard whose drop does nothing.
        pub fn new(sample_rate: usize) -> Self {
            set_heap_profiling_sampling_rate(sample_rate); // let it print the warning
            Self { _private: () }
        }
    }

    #[cfg(not(engine_heapprof))]
    impl Drop for ScopedHeapProfiling {
        fn drop(&mut self) {}
    }

    // Smallest index i such that all spans stored in the index are >= pages.
    #[inline]
    fn index_of(pages: u32) -> u32 {
        if pages == 1 {
            return 0;
        }
        u32::BITS - (pages - 1).leading_zeros()
    }

    impl DisableBacktraceTemporarily {
        /// Pauses the heap profiler's sampler of this thread until the returned guard is
        /// dropped, so that allocations made meanwhile (such as those of taking a backtrace)
        /// are not sampled. Works whether or not the thread's pool is initialized.
        pub fn new() -> Self {
            Self {
                // SAFETY: the thread's cpu_mem object (constructed, maybe not initialized); its
                // sampler stays in place for the life of the thread and the guard drops first.
                _disable_sampling: unsafe {
                    Sampler::pause_sampling((*get_cpu_mem_raw()).heap_prof_sampler())
                },
            }
        }
    }

    // Called only on the engine_heapprof paths; compiled in every build.
    #[cfg_attr(not(engine_heapprof), allow(dead_code))]
    fn get_backtrace() -> SimpleBacktrace {
        let _dbt = DisableBacktraceTemporarily::new();
        current_backtrace_tasklocal()
    }

    // Called only on the engine_heapprof paths; compiled in every build.
    #[cfg_attr(not(engine_heapprof), allow(dead_code))]
    fn get_allocation_site() -> AllocationSitePtr {
        // SAFETY: the thread's cpu_mem object (constructed, maybe not initialized); this
        // heap-profiling path runs from the allocation path that owns it.
        let cpu_mem = unsafe { &mut *get_cpu_mem_raw() };
        if !cpu_mem.is_initialized() || cpu_mem.heap_prof_sampler().sampling_interval() == 0 {
            return std::ptr::null();
        }
        let _dbt = DisableBacktraceTemporarily::new();
        let new_alloc_site =
            AllocationSite { backtrace: get_backtrace(), ..AllocationSite::default() };
        if cpu_mem.alloc_sites().len() >= 1000 && !cpu_mem.alloc_sites().contains(&new_alloc_site) {
            // Drop sample for now. Could do something smarter like dropping a
            // current one at random but needs more work in remove_alloc_site as we
            // might then have allocations for which the allocsite is no longer
            // alive
            return std::ptr::null();
        }
        let (alloc_site, inserted) = match cpu_mem.alloc_sites().get(&new_alloc_site) {
            Some(existing) => (&**existing as *const AllocationSite, false),
            None => {
                let b = Box::new(new_alloc_site);
                let p = &*b as *const AllocationSite;
                cpu_mem.alloc_sites().insert(b);
                (p, true)
            }
        };
        if inserted {
            // SAFETY: `alloc_site` was just boxed into alloc_sites; the head is alive.
            unsafe {
                (*alloc_site).next.set(cpu_mem.alloc_site_list_head);
                if !cpu_mem.alloc_site_list_head.is_null() {
                    (*cpu_mem.alloc_site_list_head).prev.set(alloc_site);
                }
            }
            cpu_mem.alloc_site_list_head = alloc_site;
        }
        alloc_site
    }

    fn maybe_enable_transparent_hugepages(addr: *mut c_void, len: usize) {
        if USE_TRANSPARENT_HUGEPAGES.load(Ordering::Relaxed) {
            // SAFETY: advisory call on memory mapped by the allocator.
            unsafe { madvise(addr, len, MADV_HUGEPAGE) };
        }
    }

    fn maybe_disable_transparent_hugepages(addr: *mut c_void, len: usize) {
        if !USE_TRANSPARENT_HUGEPAGES.load(Ordering::Relaxed) {
            // SAFETY: advisory call on memory mapped by the allocator.
            unsafe { madvise(addr, len, MADV_NOHUGEPAGE) };
        }
    }

    /// # Safety
    /// `where_ .. where_ + how_much` is memory the allocator owns (the shard's window) and
    /// whose contents may be discarded: the mapping is MAP_FIXED.
    unsafe fn allocate_anonymous_memory(
        where_: *mut c_void,
        how_much: usize,
    ) -> Result<MmapArea, ExceptionPtr> {
        // SAFETY: by the caller.
        unsafe {
            crate::engine::core::posix::mmap_anonymous(
                where_,
                how_much,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_FIXED,
            )
        }
    }

    /// Grows the hugetlbfs file `fd` by `how_much` bytes and maps that new tail, shared,
    /// read-write and populated (`MAP_POPULATE`), at `where_`, or where the kernel chooses when
    /// `where_` is null. `how_much` should be a multiple of the file system's huge page size.
    ///
    /// # Errors
    /// Fails if the file's size cannot be read, the file cannot be extended (`ftruncate`), or
    /// the mapping fails.
    ///
    /// # Safety
    /// A non-null `where_ .. where_ + how_much` is memory the allocator owns and whose contents
    /// may be discarded: the mapping is then `MAP_FIXED` and replaces whatever was there.
    pub unsafe fn allocate_hugetlbfs_memory(
        fd: &mut FileDesc,
        where_: *mut c_void,
        how_much: usize,
    ) -> Result<MmapArea, ExceptionPtr> {
        let pos = fd.size()?;
        fd.truncate(pos + how_much)?;
        // SAFETY: by the caller.
        unsafe {
            fd.map(
                how_much,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE | if !where_.is_null() { MAP_FIXED } else { 0 },
                pos,
                where_,
            )
        }
    }

    /// Aborts the process with `abort(3)`, printing nothing, if `size` is above `isize::MAX`
    /// (its top bit is set), which only an unsigned underflow in the caller's size arithmetic
    /// produces. Otherwise returns; one compare and branch.
    pub fn abort_on_underflow(size: usize) {
        if (size as isize) < 0 {
            // probably a logic error, stop hard
            // SAFETY: abort has no preconditions.
            unsafe { abort() };
        }
    }

    /// Allocates `size` bytes as one span of the current shard: `size` rounded up to pages and
    /// then to a power of two pages. `should_sample` records it in the heap profiler (only with
    /// `engine_heapprof`). Logs a warning if the size reaches the large allocation threshold,
    /// and may run the synchronous reclaimers.
    ///
    /// Returns null when no span is free (even after reclaiming) or rounding `size` up
    /// overflows; it does not count or report the failure. Aborts the process if `size` is
    /// above `isize::MAX` (see [`abort_on_underflow`]). The calling thread's pool must be set
    /// up.
    #[inline(always)]
    pub fn allocate_large(size: usize, should_sample: bool) -> *mut c_void {
        let Some(size_in_pages) = size_to_page_count(size) else {
            return std::ptr::null_mut();
        };
        // SAFETY: the thread's cpu_pages is initialized on the allocation paths that get here.
        unsafe { CpuPages::allocate_large(cpu_mem_ptr(), size_in_pages, should_sample) }
    }

    /// The number of pages that hold `size` bytes, or `None` when it does not fit the page
    /// count of a span (`u32`). Aborts the process if `size` is above `isize::MAX` (see
    /// [`abort_on_underflow`]), so rounding it up cannot overflow.
    #[inline(always)]
    fn size_to_page_count(size: usize) -> Option<u32> {
        abort_on_underflow(size);
        u32::try_from((size + PAGE_SIZE - 1) >> PAGE_BITS).ok()
    }

    /// Like [`allocate_large`]; `align` is passed down but not used: the span is aligned to its
    /// own power-of-two size, so the result is aligned to `align` only when that size is at
    /// least `align` (a `size` smaller than an `align` above one page gets a smaller
    /// alignment; [`allocate_aligned`] raises `size` to `align` before calling it). Returns null
    /// when the page count of `size` does not fit a span, as [`allocate_large`] does.
    pub fn allocate_large_aligned(align: usize, size: usize, should_sample: bool) -> *mut c_void {
        let Some(size_in_pages) = size_to_page_count(size) else {
            return std::ptr::null_mut();
        };
        let align_in_pages = (align.max(PAGE_SIZE) >> PAGE_BITS) as u32;
        // SAFETY: as in allocate_large.
        unsafe {
            CpuPages::allocate_large_aligned(
                cpu_mem_ptr(),
                align_in_pages,
                size_in_pages,
                should_sample,
            )
        }
    }

    /// Returns the span of `ptr`, a large allocation of the current shard (the start of its
    /// span), to the buddy lists, merging it with free neighbours. Nothing is checked or
    /// counted: any other pointer corrupts the allocator.
    pub fn free_large(ptr: *mut c_void) {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).free_large(ptr) }
    }

    /// Returns the usable size of `ptr`, an allocation in the memory of any live shard: its size
    /// class for a small object (less the allocation-site slot of a sampled one), its span's
    /// size for a large one. `ptr` is not checked: it must be shard memory (see
    /// [`is_engine_memory`]), or the lookup reads through a null or foreign pool.
    pub fn object_size(ptr: *mut c_void) -> usize {
        // SAFETY: a engine pointer's cpu is registered in all_cpus.
        unsafe { (*ALL_CPUS[object_cpu_id(ptr) as usize].load(Ordering::Relaxed)).object_size(ptr) }
    }

    // Cold, so that the compiler moves it out of the hot text.
    #[cold]
    fn init_cpu_mem() {
        let p = get_cpu_mem_raw();
        with_allocator_tls!(cpu_mem_ptr, |c: &Cell<*mut CpuPages>| c.set(p));
        // SAFETY: the thread's cpu_pages is only accessed from this thread.
        let initialized = unsafe { (*p).initialize() };
        if initialized {
            // Register the teardown of the pages for this thread (see
            // CpuPagesDestructor). The registration may allocate (the C runtime's list of
            // thread-exit destructors), re-entering malloc: it is done once the allocator is
            // fully set up and no reference into it is live, so the nested allocation is an
            // ordinary one.
            CPU_PAGES_DESTRUCTOR.with(|_| {});
            // Touching the other thread-local with a destructor here registers it now, which
            // keeps its registration out of the first diagnostics report, which must not
            // allocate (alloc_test's test_diagnostics_allocation).
            ADDITIONAL_DIAGNOSTICS_PRODUCER.with(|_| {});
        }
    }

    /// The thread's initialized `cpu_mem`, as a pointer, for the paths that
    /// re-enter the allocator.
    #[inline(always)]
    fn cpu_mem_ptr() -> *mut CpuPages {
        with_allocator_tls!(cpu_mem_ptr, |c: &Cell<*mut CpuPages>| c.get())
    }

    /// The thread's initialized `cpu_mem`. It is a raw pointer because the allocator re-enters
    /// itself (reclaimers, pools refilling): each use borrows it for one call or one field
    /// access, and no reference to it outlives that.
    #[inline(always)]
    fn get_cpu_mem() -> *mut CpuPages {
        // cpu_pages has a non-trivial constructor which means that the compiler
        // must make sure the instance local to the current thread has been
        // constructed before each access. So instead we access cpu_mem_ptr
        // which has been initialized by calls to init_cpu_mem() before it is
        // accessed.
        // init_cpu_mem() ran on this thread before any allocator path reaches here, and the
        // thread's cpu_pages lives as long as the thread.
        with_allocator_tls!(cpu_mem_ptr, |c: &Cell<*mut CpuPages>| c.get())
    }

    #[cfg(engine_debug_allocations)]
    const DEBUG_ALLOCATION_PATTERN: u8 = 0xab;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum AlignmentT {
        Aligned,
        Unaligned,
    }

    #[cfg(engine_heapprof)]
    fn allocate_from_sampled_small_pool(alignment: AlignmentT, mut size: usize) -> *mut c_void {
        size = object_size_with_alloc_site(size);
        if alignment == AlignmentT::Aligned {
            size = 1 << log2ceil_usize(size);
        }
        let idx = SmallPool::size_to_idx(size as u32);
        // SAFETY: the thread's cpu_mem and one of its pools, each borrowed for one call; `idx`
        // is in range since size <= max_small_allocation.
        unsafe {
            let pool = SmallPoolArray::get(&raw mut (*get_cpu_mem()).sampled_small_pools, idx);
            dassert!(size <= (*pool).object_size() as usize);
            let ptr = SmallPool::allocate(pool);
            if likely(!ptr.is_null()) {
                // we failed to allocate, so we won't sample either
                let alloc_site = (*get_cpu_mem()).add_alloc_site((*pool).object_size() as usize);
                *(*pool).alloc_site_holder(ptr) = alloc_site;
            }
            ptr
        }
    }

    #[inline(always)]
    fn allocate_from_small_pool(alignment: AlignmentT, mut size: usize) -> *mut c_void {
        if alignment == AlignmentT::Aligned {
            size = 1 << log2ceil_usize(size);
        }
        let idx = SmallPool::size_to_idx(size as u32);
        // SAFETY: the thread's cpu_mem; `idx` is in range since size <= max_small_allocation
        // (the callers check it), and the pool is used through its pointer.
        unsafe {
            let pool = SmallPoolArray::get(&raw mut (*get_cpu_mem()).small_pools, idx);
            dassert!(size <= (*pool).object_size() as usize);
            SmallPool::allocate(pool)
        }
    }

    /// The small-pool fast path of `allocate` (unaligned request, then the allocation's
    /// bookkeeping), with the refill of an empty pool (and the rest of that
    /// allocation) in a function of its own that it tail-calls: no call on the fast path
    /// returns to it, so it needs no frame.
    #[inline(always)]
    fn allocate_from_small_pool_and_finish(size: usize) -> *mut c_void {
        let idx = SmallPool::size_to_idx(size as u32);
        // SAFETY: the thread's cpu_mem; `idx` is in range since size <= max_small_allocation
        // (the caller checks it), and the pool is used through its pointer.
        unsafe {
            let pool = SmallPoolArray::get(&raw mut (*get_cpu_mem()).small_pools, idx);
            dassert!(size <= (*pool).object_size() as usize);
            if likely(!(*pool)._free.is_null()) {
                finish_allocation(SmallPool::pop_free(pool), size)
            } else {
                refill_small_pool_and_finish(pool, size)
            }
        }
    }

    /// The empty-pool leg of [`allocate_from_small_pool_and_finish`].
    ///
    /// # Safety
    /// `pool` is a pool of the calling thread's cpu_pages whose size class holds `size`.
    ///
    /// `extern "C"` (it cannot unwind), as `allocate_slowpath`.
    #[cold]
    #[inline(never)]
    unsafe extern "C" fn refill_small_pool_and_finish(
        pool: *mut SmallPool,
        size: usize,
    ) -> *mut c_void {
        // SAFETY: by the caller.
        finish_allocation(unsafe { SmallPool::add_more_objects(pool) }, size)
    }

    /**
     * Common handling code when a pointer (possibly null) has
     * been returned by any allocation path.
     */
    #[inline(always)]
    fn finish_allocation(ptr: *mut c_void, size: usize) -> *mut c_void {
        alloc_stats::increment_local(alloc_stats::Types::Allocs, 1);
        if unlikely(ptr.is_null()) {
            on_allocation_failure(size);
        } else {
            #[cfg(engine_debug_allocations)]
            // SAFETY: `ptr` points to at least `size` writable bytes.
            unsafe {
                std::ptr::write_bytes(ptr as *mut u8, DEBUG_ALLOCATION_PATTERN, size)
            };
        }
        ptr
    }

    static DOUBLE_CHECKED: AtomicBool = AtomicBool::new(false);

    /// The out-of-line part of [`allocate`], for every request its inlined fast path does not
    /// serve. On a thread that is not a shard it forwards to libc's `malloc` (counted as a
    /// foreign allocation) once that has been looked up; otherwise (or before the lookup) it
    /// serves `size` from the thread's pool, small pool or span, sampling it for the heap
    /// profiler when due. It counts the allocation, and on failure runs
    /// [`on_allocation_failure`] (which may abort the process) and returns null. `size` must
    /// be at least 8, as [`allocate`] makes it.
    ///
    /// `extern "C"`, so that it cannot unwind (a panic in it, or in a reclaimer it runs,
    /// aborts): it is reached from `malloc`, which cannot unwind, as well as from `operator new`,
    /// which raises its error only after it returns, and an unwinding `GlobalAlloc` is
    /// undefined behaviour. Without an unwind edge, `malloc` tail-calls it and needs no frame.
    #[inline(never)]
    pub extern "C" fn allocate_slowpath(size: usize) -> *mut c_void {
        if !is_reactor_thread()
            && let Some(ptr) = allocate_slowpath_foreign(size)
        {
            return ptr;
        }
        // On the fast path we've already called maybe_sample, except in the case
        // of !is_reactor_thread (we don't sample such alloctions).
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        let should_sample = unsafe { (*get_cpu_mem()).definitely_sample(size) };
        let ptr;
        if size <= MAX_SMALL_ALLOCATION {
            #[cfg(engine_heapprof)]
            {
                ptr = if should_sample {
                    allocate_from_sampled_small_pool(AlignmentT::Unaligned, size)
                } else {
                    allocate_from_small_pool(AlignmentT::Unaligned, size)
                };
            }
            #[cfg(not(engine_heapprof))]
            {
                ptr = allocate_from_small_pool(AlignmentT::Unaligned, size);
            }
        } else {
            ptr = allocate_large(size, should_sample);
        }
        finish_allocation(ptr, size)
    }

    /// The `!is_reactor_thread()` branch of `allocate_slowpath`, out of line: `Some` is the
    /// result of the call, `None` falls through to this shard's pools. Kept apart so that the
    /// registers it needs are not saved on every large allocation of a reactor thread.
    #[cold]
    #[inline(never)]
    fn allocate_slowpath_foreign(size: usize) -> Option<*mut c_void> {
        if let Some(f) = libc!(LIBC_MALLOC_FUNC, MallocFuncType) {
            alloc_stats::increment(alloc_stats::Types::ForeignMallocs, 1);
            // SAFETY: calling libc's malloc, found with dlsym(RTLD_NEXT).
            return Some(unsafe { f(size) });
        }

        // libc's malloc may not be looked up yet, for allocations made by initializers that run
        // before ours. Much code relies on cpu_mem_ptr being set up by this point, so the pool is
        // initialized here first.
        init_cpu_mem();

        // Initialization order of the fallback functions: if dependent libraries call malloc
        // before our initializers run,
        // we end up here with nowhere to go. Add a second check and attempt the full init
        // already. If we find the functions, all is good. Otherwise, trudge along and probably
        // crash terribly later.
        // Note: this is only relevant if we're in dlinit phase, in which case we are single
        // threaded. Or there is no external func to find. No need for atomics or fences.
        if !DOUBLE_CHECKED.load(Ordering::Relaxed) {
            DOUBLE_CHECKED.store(true, Ordering::Relaxed);

            load_libc_functions();

            if !LIBC_MALLOC_FUNC.load(Ordering::Relaxed).is_null() {
                return Some(allocate_slowpath(size));
            }
        }
        None
    }

    /// Allocates `size` bytes (at least 8 are reserved); returns null on failure, after
    /// [`on_allocation_failure`] (which may abort the process). A block of more than 16 bytes
    /// is 16-byte aligned; a smaller one only to its size class's stride (8, 10, 12, 14 or 16
    /// bytes), so a 10-byte class gives 2-byte alignment.
    ///
    /// Fast path, inlined into `malloc`, the `new` operators and `__rust_alloc`: on a shard,
    /// for a request of at most [`MAX_SMALL_ALLOCATION`] bytes that the heap profiler does not
    /// sample, it pops the head of the size class's free list and counts the allocation,
    /// without a call or a stack frame. Everything else (an empty pool, a large or sampled
    /// request, a thread that is not a shard, which gets libc's `malloc`) goes to the shared
    /// out-of-line [`allocate_slowpath`].
    #[inline(always)]
    pub fn allocate(mut size: usize) -> *mut c_void {
        size = size.max(size_of::<FreeObject>());
        if likely(
            is_reactor_thread()
            // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
            && !unsafe { (*get_cpu_mem()).maybe_sample(size) } && size <= MAX_SMALL_ALLOCATION,
        ) {
            return allocate_from_small_pool_and_finish(size);
        }

        allocate_slowpath(size)
    }

    /// Allocates `size` bytes aligned to `align` (a power of two, not checked); returns null on
    /// failure, after [`on_allocation_failure`] (which may abort the process).
    ///
    /// On a thread that is not a shard it forwards to libc's `aligned_alloc` once that has been
    /// looked up. Otherwise `size` is first raised to at least `align` and 8 bytes (the same
    /// class size the sized aligned free computes): the small pools and the buddy spans align a
    /// block only to its own power-of-two size, so a block smaller than `align` would be under-aligned
    /// (`allocate_aligned(64, 12)` would give a 16-byte-aligned block). A request of at most
    /// [`MAX_SMALL_ALLOCATION`] bytes with `align` up to one page then takes the small pool of
    /// that size rounded up to a power of two; anything else takes a span (see
    /// [`allocate_large_aligned`]). The block is thus aligned to `align` for every `size`.
    /// Not inlined into a fast path; the allocation is counted.
    pub fn allocate_aligned(align: usize, mut size: usize) -> *mut c_void {
        if !is_reactor_thread() {
            if let Some(f) = libc!(LIBC_ALIGNED_ALLOC_FUNC, AlignedAllocType) {
                alloc_stats::increment(alloc_stats::Types::ForeignMallocs, 1);
                // SAFETY: calling libc's aligned_alloc, found with dlsym(RTLD_NEXT).
                return unsafe { f(align, size) };
            }
            // libc's functions may not be looked up yet, for allocations made by initializers
            // that run before ours.
            init_cpu_mem();
        }
        size = aligned_class_size(align, size);
        #[cfg(engine_heapprof)]
        let should_sample = {
            // SAFETY: the thread's cpu_mem, borrowed for these two calls.
            unsafe {
                (*get_cpu_mem()).maybe_sample(size) && (*get_cpu_mem()).definitely_sample(size)
            }
        };
        #[cfg(not(engine_heapprof))]
        let should_sample = false;
        let ptr;
        if size <= MAX_SMALL_ALLOCATION && align <= PAGE_SIZE {
            // Our small allocator only guarantees alignment for power-of-two
            // allocations which are not larger than a page.
            #[cfg(engine_heapprof)]
            {
                ptr = if should_sample {
                    allocate_from_sampled_small_pool(AlignmentT::Aligned, size)
                } else {
                    allocate_from_small_pool(AlignmentT::Aligned, size)
                };
            }
            #[cfg(not(engine_heapprof))]
            {
                ptr = allocate_from_small_pool(AlignmentT::Aligned, size);
            }
        } else {
            ptr = allocate_large_aligned(align, size, should_sample);
        }
        if ptr.is_null() {
            on_allocation_failure(size);
        } else {
            #[cfg(engine_debug_allocations)]
            // SAFETY: `ptr` points to at least `size` writable bytes.
            unsafe {
                std::ptr::write_bytes(ptr as *mut u8, DEBUG_ALLOCATION_PATTERN, size)
            };
        }
        alloc_stats::increment_local(alloc_stats::Types::Allocs, 1);
        ptr
    }

    /// The "size unknown" argument of [`free_with`]: the free looks the size up in the page
    /// structure. The other [`FreeSize`] is `usize`, the allocation's size, which picks the
    /// small pool without that lookup. Zero-sized; resolved at compile time.
    #[derive(Clone, Copy, Default)]
    pub struct NoSize;

    /// The size argument of the free paths: [`NoSize`] or the allocation's size.
    pub trait FreeSize: Copy {
        /// Frees `obj`, an allocation of the calling thread's shard, into its pool or span
        /// (without counting the free).
        ///
        /// # Safety
        /// `mem` is the thread's cpu_mem and `obj` a live allocation of this shard (of this
        /// size, when one is given).
        unsafe fn free_local(self, mem: *mut CpuPages, obj: *mut c_void);

        /// Calls the out-of-line, non-unwinding free slow path for this size type: a local
        /// pointer is counted and freed with [`FreeSize::free_local`]; anything else (null,
        /// libc memory, another shard's memory) goes to the foreign free.
        fn free_slowpath(self, obj: *mut c_void);
    }

    impl FreeSize for NoSize {
        #[inline(always)]
        unsafe fn free_local(self, mem: *mut CpuPages, obj: *mut c_void) {
            // SAFETY: by the caller; `mem` is borrowed for this one call.
            unsafe { (*mem).free(obj) };
        }

        #[inline(always)]
        fn free_slowpath(self, obj: *mut c_void) {
            free_slowpath_no_size(obj);
        }
    }

    impl FreeSize for usize {
        #[inline(always)]
        unsafe fn free_local(self, mem: *mut CpuPages, obj: *mut c_void) {
            // SAFETY: by the caller; `mem` is borrowed for this one call.
            unsafe { (*mem).free_sized(obj, self) };
        }

        #[inline(always)]
        fn free_slowpath(self, obj: *mut c_void) {
            free_slowpath_sized(obj, self);
        }
    }

    // The two never-inlined instances of `free_slowpath::<S>`. They are `extern "C"`, so they
    // cannot unwind (a panic in them aborts): nothing on the free path runs user code, `free` and
    // `operator delete` cannot fail, and a `GlobalAlloc::dealloc` that unwinds is undefined
    // behaviour. Without an unwind edge the inlined fast paths need no landing pad and reach the
    // slow path with a tail call.
    #[inline(never)]
    extern "C" fn free_slowpath_no_size(obj: *mut c_void) {
        free_slowpath_impl(obj, NoSize);
    }

    #[inline(never)]
    extern "C" fn free_slowpath_sized(obj: *mut c_void, size: usize) {
        free_slowpath_impl(obj, size);
    }

    #[inline(always)]
    fn free_slowpath_impl<S: FreeSize>(obj: *mut c_void, size: S) {
        if CpuPages::is_local_pointer(obj) {
            alloc_stats::increment_local(alloc_stats::Types::Frees, 1);
            // SAFETY: `obj` is a local pointer, so it was allocated by this shard.
            unsafe { size.free_local(get_cpu_mem(), obj) };
        } else {
            CpuPages::do_foreign_free(obj);
        }
    }

    /// Frees `obj`, of any origin; `size` is [`NoSize`] or the allocation's size.
    ///
    /// Fast path, inlined into `free`, the `delete` operators and `__rust_dealloc`: for a
    /// pointer of the calling shard's own memory it frees an unsampled small object onto its
    /// pool's free list, or hands a large one to an out-of-line span free, and counts the
    /// free; it reads the page structure in both cases, never the size. Anything else goes to
    /// the shared out-of-line slow path ([`FreeSize::free_slowpath`]): null is ignored, libc
    /// memory goes to libc's `free`, and another shard's memory is queued on that shard's
    /// lock-free cross-shard list (leaked if that shard has exited). Never fails or unwinds.
    #[inline(always)]
    pub fn free_with<S: FreeSize>(obj: *mut c_void, size: S) {
        if !likely(CpuPages::try_free_fastpath(obj)) {
            size.free_slowpath(obj);
        }
    }

    /// Frees `obj`, of any origin, looking its size up in its page structure; null is ignored.
    /// See [`free_with`].
    #[inline(always)]
    pub fn free(obj: *mut c_void) {
        free_with(obj, NoSize);
    }

    /// Frees `obj`, allocated with `size` bytes; null is ignored. The size is used only on the
    /// slow path, to pick the small pool without reading the page structure (and not at all
    /// with `engine_heapprof`); it must be the size the block was allocated with, or one of the
    /// same size class. See [`free_with`].
    #[inline(always)]
    pub fn free_sized(obj: *mut c_void, size: usize) {
        free_with(obj, size);
    }

    /// Frees `obj`, made by [`allocate_aligned`]`(align, size)`; null is ignored. `align` and
    /// `size` only rebuild the size class `allocate_aligned` picked (the power of two for a
    /// small request), passed to [`free_sized`].
    pub fn free_aligned(obj: *mut c_void, align: usize, size: usize) {
        free_sized(obj, free_aligned_size(align, size));
    }

    /// The size an aligned request of `size` bytes is served with: at least `align`, so that the
    /// block, aligned to its own power-of-two size, is aligned to `align` too, and at least a
    /// free-list link (8 bytes). [`allocate_aligned`] and [`free_aligned_size`] both start from
    /// it, so a sized aligned free finds the pool the block came from.
    #[inline(always)]
    fn aligned_class_size(align: usize, size: usize) -> usize {
        // Written as a test rather than a second `max`: for a size already raised to `align`
        // (the Rust allocator's aligned layouts) the first `max` folds away, and the sized
        // free path compiles to the same code as without it.
        let size = size.max(align);
        if size <= size_of::<FreeObject>() { size_of::<FreeObject>() } else { size }
    }

    /// The size `free_aligned(obj, align, size)` frees with: the class `allocate_aligned`
    /// picked for the block.
    #[inline(always)]
    fn free_aligned_size(align: usize, size: usize) -> usize {
        let mut size = aligned_class_size(align, size);
        #[cfg(not(engine_heapprof))]
        {
            // See comment in the sized-free implementation below. With heap profiling we
            // can skip this as sized-free defers to non-sized-free
            if size <= MAX_SMALL_ALLOCATION && align <= PAGE_SIZE {
                // Same adjustment as allocate_aligned()
                size = 1 << log2ceil_usize(size);
            }
        }
        let _ = align;
        size
    }

    /// Shrinks the allocation at `obj` in place to hold at least `new_size` bytes. Only a large
    /// allocation shrinks: its span is halved while the half still holds `new_size`, and the
    /// pages cut off go back to the buddy lists; a small object is left as it is. Counts one
    /// free and one allocation either way.
    ///
    /// # Panics
    /// Panics if the shard number encoded in `obj`'s address is not the calling thread's (the
    /// thread's pool must be set up, and `obj` must be shard memory).
    pub fn shrink(obj: *mut c_void, new_size: usize) {
        alloc_stats::increment_local(alloc_stats::Types::Frees, 1);
        alloc_stats::increment_local(alloc_stats::Types::Allocs, 1); // keep them balanced
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).shrink(obj, new_size) };
    }

    /// Sets how the allocator schedules reclaim on the current shard (see [`ReclaimHook`]) and
    /// arms the low-memory threshold at [`min_free_memory`]: from now on, a span allocation
    /// that leaves less free memory than that drains the cross-shard free list, runs the
    /// synchronous reclaimers and, if still short, schedules the asynchronous reclaim through
    /// `hook` (disarming the threshold until that task has run). Before a hook is set only a
    /// span allocation that finds no free span runs the reclaimers. Ignored, and `hook`
    /// dropped, on a thread whose pool is not set up.
    pub fn set_reclaim_hook(hook: ReclaimHook) {
        // in general, we are using Engine allocator here. but the Engine application
        // can still configure smp_opts.memory_allocator with memory_allocator::standard.
        // in that case, the memory::configure() is not called, hence cpu_mem_ptr is not
        // set.
        let p = with_allocator_tls!(cpu_mem_ptr, |c: &Cell<*mut CpuPages>| c.get());
        if !p.is_null() {
            // SAFETY: cpu_mem_ptr points to this thread's cpu_pages.
            unsafe { (*p).set_reclaim_hook(hook) };
        }
    }

    impl Reclaimer {
        /// Boxes a reclaimer that receives the [`ReclaimerRequest`] and appends it to the
        /// current shard's list of reclaimers; `scope` says when it may run. Allocates (the box
        /// and possibly the list).
        ///
        /// The calling thread must have its pool set up ([`configure`] or
        /// [`configure_minimal`] ran on it): the list is reached through the thread's pool
        /// pointer without a null check.
        pub fn with_request(
            reclaim: Box<dyn FnMut(ReclaimerRequest) -> ReclaimingResult>,
            scope: ReclaimerScope,
        ) -> Box<Reclaimer> {
            let mut r = Box::new(Reclaimer { _reclaim: reclaim, _scope: scope });
            let p: *mut Reclaimer = &mut *r;
            // SAFETY: the thread's cpu_pages is live; only its `reclaimers` field is borrowed,
            // for the push (a reclaimer may be created from inside another one).
            unsafe { (*cpu_mem_ptr()).reclaimers.push(p) };
            r
        }
    }

    impl Drop for Reclaimer {
        fn drop(&mut self) {
            let me: *mut Reclaimer = self;
            // SAFETY: as in `with_request`; only the `reclaimers` field is borrowed.
            let r = unsafe { &mut (*cpu_mem_ptr()).reclaimers };
            if let Some(pos) = r.iter().position(|&x| x == me) {
                r.remove(pos);
            }
        }
    }

    /// Sets, on the current shard, the size from which a span allocation logs a warning with
    /// its backtrace on the `engine_memory` logger and counts as a large allocation; each
    /// warning raises the threshold by a factor of 1.618 to limit the noise. Compared with the
    /// size rounded up to pages. The thread's pool must be set up.
    pub fn set_large_allocation_warning_threshold(threshold: usize) {
        // SAFETY: the thread's cpu_mem, one field written (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).large_allocation_warning_threshold = threshold };
    }

    /// Returns the current shard's large allocation warning threshold, including the raises
    /// made by earlier warnings (`usize::MAX`, the initial value, when disabled). The thread's
    /// pool must be set up.
    pub fn get_large_allocation_warning_threshold() -> usize {
        // SAFETY: the thread's cpu_mem, one field read (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).large_allocation_warning_threshold }
    }

    /// Disables large allocation warnings on the current shard (sets the threshold to
    /// `usize::MAX`). The thread's pool must be set up.
    pub fn disable_large_allocation_warning() {
        // SAFETY: the thread's cpu_mem, one field written (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).large_allocation_warning_threshold = usize::MAX };
    }

    /// Sets up the calling thread's pool without the memory of [`configure`]: the next shard
    /// window gets its 32 MiB bootstrap mapping (aborting the process if it cannot be mapped)
    /// and the thread pointer to its pool is set. The thread is not marked as a shard, so its
    /// `malloc` keeps going to libc's allocator once that has been looked up. Used by reactors
    /// that run with the system allocator option. Calling it again maps nothing.
    pub fn configure_minimal() {
        init_cpu_mem();
    }

    fn mbind(
        addr: *mut c_void,
        len: c_ulong,
        mode: c_int,
        nodemask: *const c_ulong,
        maxnode: c_ulong,
        flags: u32,
    ) -> c_long {
        // SAFETY: SYS_mbind takes exactly these arguments; the kernel validates the range.
        unsafe { syscall(SYS_MBIND, addr, len, mode, nodemask, maxnode, flags) }
    }

    /// Makes the calling thread a shard and gives its pool the memory `m`: one entry per NUMA
    /// node, whose sizes add up to the pool size (rounded down to a multiple of the huge page
    /// size; the pool only grows).
    ///
    /// It stores `transparent_hugepages` in [`USE_TRANSPARENT_HUGEPAGES`], sets up the pool if
    /// needed (as [`configure_minimal`]) and marks the thread as a shard, so that its
    /// allocations use the pool from now on. With `hugetlbfs_path`, the pool is backed by a
    /// temporary file created in that directory: the memory mapped so far is copied aside,
    /// remapped from the file and copied back, and the pool grows from the same file. The pool
    /// is then grown to the total size, at most four times its size per step. With `mbind_`,
    /// each entry's range is bound to its node (`MPOL_PREFERRED`, moving pages already there);
    /// a failed `mbind` prints a warning on stderr and is otherwise ignored.
    ///
    /// Returns the ranges bound with `mbind`, one per entry, so the layout is empty when
    /// `mbind_` is false.
    ///
    /// # Errors
    /// Fails if the hugetlbfs file cannot be created, extended or mapped, if a mapping that
    /// grows the pool fails, or if the pool has no room for its grown page array.
    pub fn configure(
        m: Vec<resource::Memory>,
        mbind_: bool,
        transparent_hugepages: bool,
        hugetlbfs_path: Option<String>,
    ) -> Result<internal::NumaLayout, ExceptionPtr> {
        // we need to make sure cpu_mem is initialize since configure calls cpu_mem.resize
        // and we might reach configure without ever allocating, hence without ever calling
        // cpu_pages::initialize.
        // The correct solution is to add a condition inside cpu_mem.resize, but since all
        // other paths to cpu_pages::resize are already verifying initialize was called, we
        // verify that here.
        USE_TRANSPARENT_HUGEPAGES.store(transparent_hugepages, Ordering::Relaxed);
        init_cpu_mem();
        // init_cpu_mem() could have been called very early, see call site in allocate().
        // In that case we don't know about the transparent_hugepages parameter and conservatively
        // assume it's true. Undo that here if it turned out to be false.
        {
            // SAFETY: the thread's cpu_mem, two fields read (see get_cpu_mem).
            let (memory, nr_pages) =
                unsafe { ((*get_cpu_mem()).memory, (*get_cpu_mem()).nr_pages) };
            maybe_disable_transparent_hugepages(
                memory as *mut c_void,
                nr_pages as usize * PAGE_SIZE,
            );
        }
        with_allocator_tls!(is_reactor_thread, |c: &Cell<bool>| c.set(true));
        let mut ret_layout = internal::NumaLayout::default();
        let mut total: usize = 0;
        for x in &m {
            total += x.bytes;
        }
        // `sys_alloc` goes only to this thread's cpu_mem (replace_memory_backing and resize
        // below), which maps with it its own window: the current memory, already copied
        // aside, and the pages past its end.
        let mut sys_alloc: AllocateSystemMemoryFn =
            std::rc::Rc::new(|where_: *mut c_void, how_much: usize| {
                // SAFETY: a range of the shard's window (see above).
                unsafe { allocate_anonymous_memory(where_, how_much) }
            });
        if let Some(path) = &hugetlbfs_path {
            // The file descriptor cannot be cloned, so it is shared, to let the `sys_alloc`
            // closure be cloned.
            let fdp =
                make_lw_shared(RefCell::new(FileDesc::temporary(Sstring::from(path.as_str()))?));
            sys_alloc = std::rc::Rc::new(move |where_: *mut c_void, how_much: usize| {
                // SAFETY: a range of the shard's window (see above).
                unsafe { allocate_hugetlbfs_memory(&mut fdp.borrow_mut(), where_, how_much) }
            });
            // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
            unsafe { (*get_cpu_mem()).replace_memory_backing(&sys_alloc) }?;
        }
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).resize(total, &sys_alloc) }?;
        let mut pos: usize = 0;
        for x in &m {
            let nodemask: c_ulong = 1 << x.nodeid;
            if mbind_ {
                // SAFETY: `pos` stays within the resized shard memory of the thread's cpu_mem.
                let start = unsafe { (*get_cpu_mem()).mem().add(pos) };
                let r = mbind(
                    start as *mut c_void,
                    x.bytes as c_ulong,
                    MPOL_PREFERRED,
                    &nodemask,
                    c_ulong::BITS as c_ulong,
                    MPOL_MF_MOVE,
                );

                if r == -1 {
                    let msg = std::io::Error::last_os_error();
                    eprintln!(
                        "WARNING: unable to mbind shard memory; performance may suffer: {}",
                        msg
                    );
                }
                ret_layout.ranges.push(internal::MemoryRange {
                    start,
                    // SAFETY: as above.
                    end: unsafe { start.add(x.bytes) },
                    numa_node_id: x.nodeid,
                });
            }
            pos += x.bytes;
        }
        Ok(ret_layout)
    }

    /// Returns the calling thread's allocation statistics (see [`Statistics`] for what each
    /// counter covers). Works on any thread; the memory sizes are 0 while its pool is not set
    /// up. Reads eleven counters.
    pub fn stats() -> Statistics {
        use alloc_stats::{Types, get};
        // SAFETY: the thread's cpu_mem object (constructed, maybe not initialized), two
        // fields read.
        let (nr_pages, nr_free_pages) =
            unsafe { ((*get_cpu_mem_raw()).nr_pages, (*get_cpu_mem_raw()).nr_free_pages) };
        Statistics::new(
            get(Types::Allocs),
            get(Types::Frees),
            get(Types::CrossCpuFrees),
            (nr_pages as usize * PAGE_SIZE) as u64,
            (nr_free_pages as usize * PAGE_SIZE) as u64,
            get(Types::Reclaims),
            get(Types::LargeAllocs),
            get(Types::FailedAllocs),
            get(Types::ForeignMallocs),
            get(Types::ForeignFrees),
            get(Types::ForeignCrossFrees),
        )
    }

    /// Returns the free memory of the current shard's pool, in bytes (free pages only; objects
    /// on the small pools' free lists count as used). The thread's pool must be set up.
    pub fn free_memory() -> usize {
        // SAFETY: the thread's cpu_mem, one field read (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).nr_free_pages as usize * PAGE_SIZE }
    }

    /// Frees into this shard's pools the objects other threads freed into its memory (its
    /// lock-free cross-shard list, taken with one atomic swap), counting each as a free;
    /// returns whether there were any. One relaxed load when the list is empty. Called by the
    /// reactor's poller and before a low-memory reclaim. The thread's pool must be set up.
    pub fn drain_cross_cpu_freelist() -> bool {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).drain_cross_cpu_freelist() }
    }

    /// Returns the address range of the current shard's pool, page array included. Never
    /// returns an error with the engine allocator (the `Result` is for builds without it).
    ///
    /// The thread's pool must be set up ([`configure`] or [`configure_minimal`]); the pointer
    /// to it is not checked for null.
    ///
    /// # Panics
    /// Panics if the pool has no pages, which cannot happen once it is set up.
    pub fn get_memory_layout() -> Result<MemoryLayout, ExceptionPtr> {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        Ok(unsafe { (*get_cpu_mem()).memory_layout() })
    }

    /// Returns the current shard's low-memory threshold, in bytes (initially 20 MB rounded
    /// down to pages): the free memory below which a span allocation reclaims once a reclaim
    /// hook is set, and the floor each reclaim round aims for. The thread's pool must be set up.
    pub fn min_free_memory() -> usize {
        // SAFETY: the thread's cpu_mem, one field read (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).min_free_pages as usize * PAGE_SIZE }
    }

    /// Sets the current shard's low-memory threshold to `pages` pages (see
    /// [`min_free_memory`]). The armed threshold only follows on the next
    /// [`set_reclaim_hook`] or at the end of the next asynchronous reclaim; the reclaim
    /// target follows at once. It then checks the armed threshold and, if free memory is below
    /// it, reclaims synchronously as an allocation would. The thread's pool must be set up.
    ///
    /// # Errors
    /// Fails, changing nothing, if `pages` does not fit in a `u32`. It does not compare
    /// `pages` with the pool size.
    pub fn set_min_free_pages(pages: usize) -> Result<(), ExceptionPtr> {
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).set_min_free_pages(pages) }
    }

    thread_local! {
        static REPORT_ON_ALLOC_FAILURE_SUPPRESSED: Cell<i32> = const { Cell::new(0) };
    }

    struct DisableReportOnAllocFailureTemporarily;

    impl DisableReportOnAllocFailureTemporarily {
        fn new() -> Self {
            REPORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.set(c.get() + 1));
            Self
        }
    }

    impl Drop for DisableReportOnAllocFailureTemporarily {
        fn drop(&mut self) {
            REPORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.set(c.get() - 1));
        }
    }

    /// Sets, for the whole process, whether a failed allocation aborts it (for a core dump at
    /// the point of failure): [`on_allocation_failure`] then logs the diagnostics report at
    /// error level and calls `abort(3)`, unless a [`DisableAbortOnAllocFailureTemporarily`]
    /// guard is alive on the failing thread.
    pub fn set_abort_on_allocation_failure(enabled: bool) {
        ABORT_ON_ALLOCATION_FAILURE.store(enabled, Ordering::SeqCst);
    }

    /// Returns the process-wide setting of [`set_abort_on_allocation_failure`] (not whether a
    /// suppression guard is alive on this thread).
    pub fn is_abort_on_allocation_failure() -> bool {
        ABORT_ON_ALLOCATION_FAILURE.load(Ordering::SeqCst)
    }

    /// Sets, for the whole process, which allocation failures log the diagnostics report at
    /// error level: none, only those inside a critical allocation section (the default), or
    /// all. The others log it at debug level, and a failure that aborts always logs at error
    /// level.
    pub fn set_dump_memory_diagnostics_on_alloc_failure_kind(kind: AllocFailureKind) {
        DUMP_DIAGNOSTICS_ON_ALLOC_FAILURE_KIND
            .store(alloc_failure_kind_to_u8(kind), Ordering::SeqCst);
    }

    /// Like [`set_dump_memory_diagnostics_on_alloc_failure_kind`], with the kind by name:
    /// exactly `none`, `critical` or `all` (case-sensitive). Any other string logs an error on
    /// the engine logger and leaves the setting unchanged.
    pub fn set_dump_memory_diagnostics_on_alloc_failure_kind_str(str_: &str) {
        if str_ == "none" {
            set_dump_memory_diagnostics_on_alloc_failure_kind(AllocFailureKind::None);
        } else if str_ == "critical" {
            set_dump_memory_diagnostics_on_alloc_failure_kind(AllocFailureKind::Critical);
        } else if str_ == "all" {
            set_dump_memory_diagnostics_on_alloc_failure_kind(AllocFailureKind::All);
        } else {
            crate::log_error!(
                engine_logger(),
                "Ignoring invalid option '{}' for the allocation failure kind to dump engine memory diagnostics for, valid options are: none, critical and all",
                str_
            );
        }
    }

    thread_local! {
        static ADDITIONAL_DIAGNOSTICS_PRODUCER: RefCell<Option<MemoryDiagnosticsProducer>> = const { RefCell::new(None) };
    }

    /// Sets, for the calling thread only, the function that adds the application's own lines
    /// to its diagnostics report (after the memory totals), replacing any earlier one. The
    /// producer runs while an allocation failure is reported, so it should not allocate.
    pub fn set_additional_diagnostics_producer(producer: MemoryDiagnosticsProducer) {
        ADDITIONAL_DIAGNOSTICS_PRODUCER.with(|p| *p.borrow_mut() = Some(producer));
    }

    #[derive(Debug, Clone, Copy)]
    struct HumanReadableValue {
        value: u16, // [0, 1024)
        suffix: u8, // 0 -> no suffix
    }

    impl fmt::Display for HumanReadableValue {
        fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Rendered on the stack (no allocation while reporting an allocation failure):
            // at most five digits and one suffix.
            let mut buf = [0u8; 6];
            let mut len = 0;
            let mut v = self.value;
            let mut digits = [0u8; 5];
            let mut nd = 0;
            loop {
                digits[nd] = b'0' + (v % 10) as u8;
                nd += 1;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            while nd > 0 {
                nd -= 1;
                buf[len] = digits[nd];
                len += 1;
            }
            if self.suffix != 0 {
                buf[len] = self.suffix;
                len += 1;
            }
            // SAFETY: the buffer holds ASCII digits and an ASCII suffix only.
            os.pad(unsafe { std::str::from_utf8_unchecked(&buf[..len]) })
        }
    }

    fn to_human_readable_value(
        value: u64,
        step: u64,
        precision: u64,
        suffixes: &[u8; 5],
    ) -> HumanReadableValue {
        if value == 0 {
            return HumanReadableValue { value: 0, suffix: suffixes[0] };
        }

        let mut result = value;
        let mut remainder: u64 = 0;
        let mut i: usize = 0;
        // If there is no remainder we go below precision because we don't loose any.
        while (remainder == 0 && result >= step) || result >= precision {
            // The last suffix is the largest unit: stop before dividing past it.
            if i + 1 == suffixes.len() {
                break;
            }
            remainder = result % step;
            result /= step;
            i += 1;
        }
        HumanReadableValue {
            value: (if remainder < (step / 2) { result } else { result + 1 }) as u16,
            suffix: suffixes[i],
        }
    }

    fn to_hr_size(size: u64) -> HumanReadableValue {
        const SUFFIXES: [u8; 5] = *b"BKMGT";
        to_human_readable_value(size, 1024, 8192, &SUFFIXES)
    }

    fn to_hr_number(number: u64) -> HumanReadableValue {
        const SUFFIXES: [u8; 5] = [0, b'k', b'm', b'b', b't'];
        to_human_readable_value(number, 1000, 10000, &SUFFIXES)
    }

    /// Writes the current shard's diagnostics report to `it`: used, free and total memory, the
    /// failed allocation count, the lines of the thread's additional producer, a table of the
    /// small pools (object size, preferred span size, objects in use, memory, unused bytes,
    /// waste percentage) and a table of the span sizes (free and used memory, span count). It
    /// walks every span list and the whole page array, so it is O(pages); it formats numbers
    /// without allocating. The span table is read in full before its first row is written, so
    /// allocations made by `it` cannot make it inconsistent. The thread's pool must be set up.
    ///
    /// # Errors
    /// Returns the first error of `it`; the producer's writes ignore errors.
    pub fn do_dump_memory_diagnostics(it: &mut dyn fmt::Write) -> fmt::Result {
        // The writer may allocate, re-entering the allocator: the thread's cpu_mem and its
        // pools are read through raw places, one field at a time, never held as references.
        let cpu_mem = get_cpu_mem();
        // SAFETY: `cpu_mem` is the thread's live cpu_mem, and every `(*cpu_mem)`, `(*sp)` and
        // `(*span)` below is a raw place read: `sp` one of its pools (index < NR_SMALL_POOLS)
        // and `span` a page of its array reached through a span list or the page walk.
        let (free_pages, nr_pages, pages) =
            unsafe { ((*cpu_mem).nr_free_pages, (*cpu_mem).nr_pages, (*cpu_mem).pages) };
        let free_mem = free_pages as u64 * PAGE_SIZE as u64;
        let total_mem = nr_pages as u64 * PAGE_SIZE as u64;
        writeln!(it, "Dumping engine memory diagnostics")?;

        writeln!(it, "Used memory:   {}", to_hr_size(total_mem - free_mem))?;
        writeln!(it, "Free memory:   {}", to_hr_size(free_mem))?;
        writeln!(it, "Total memory:  {}", to_hr_size(total_mem))?;
        write!(it, "Hard failures: {}\n\n", alloc_stats::get(alloc_stats::Types::FailedAllocs))?;

        ADDITIONAL_DIAGNOSTICS_PRODUCER.with(|p| {
            if let Some(producer) = p.borrow_mut().as_mut() {
                producer(&mut |v: &str| {
                    let _ = it.write_str(v);
                });
            }
        });

        writeln!(it, "Small pools:")?;
        writeln!(it, "objsz spansz usedobj memory unused wst%")?;
        for i in 0..NR_SMALL_POOLS as u32 {
            // SAFETY: as at the top of this function; `i < NR_SMALL_POOLS`, and the pool array is
            // reached through a raw place.
            let sp = unsafe { SmallPoolArray::get(&raw mut (*cpu_mem).small_pools, i) };
            // SAFETY: `sp` is one of the thread's live pools; a raw place read.
            let object_size = unsafe { (*sp)._object_size };
            // We don't use pools too small to fit a free_object, so skip these, they
            // are always empty.
            if (object_size as usize) < size_of::<FreeObject>() {
                continue;
            }

            // For the small pools, there are two types of free objects:
            // Pool freelist objects are poitned to by sp._free and their count is sp._free_count
            // Span freelist objects are those removed from the pool freelist when that list
            // becomes too large: they are instead attached to the spans allocated to this
            // pool. To count this second category, we iterate over the spans below.
            let mut span_freelist_objs: u32 = 0;
            // SAFETY: `sp` is one of the thread's live pools; a raw place read.
            let mut front = unsafe { (*sp)._span_list._front };
            while front != 0 {
                // SAFETY: `front` is a nonzero page index taken from the pool's span list, so a page of
                // the live page array; raw place reads.
                let (span_size, nr_small_alloc, next) = unsafe {
                    let span = pg(pages, front as usize);
                    ((*span).span_size, (*span).nr_small_alloc, (*span).link._next)
                };
                let capacity_in_objects =
                    (span_size as usize * PAGE_SIZE / object_size as usize) as u32;
                span_freelist_objs += capacity_in_objects - nr_small_alloc as u32;
                front = next;
            }
            // SAFETY: `sp` is one of the thread's live pools; raw place reads.
            let (free_count, pages_in_use, preferred) =
                unsafe { ((*sp)._free_count, (*sp)._pages_in_use, (*sp)._span_sizes.preferred) };
            let free_objs = free_count + span_freelist_objs; // pool + span free objects
            let use_count = (pages_in_use as u64 * PAGE_SIZE as u64 / object_size as u64)
                .wrapping_sub(free_objs as u64);
            let memory = pages_in_use as u64 * PAGE_SIZE as u64;
            let unused = free_objs as u64 * object_size as u64;
            let wasted_percent = (unused * 100).checked_div(memory).unwrap_or(0);
            writeln!(
                it,
                "{:>5}  {:>5}   {:>5}  {:>5}  {:>5} {:>4}",
                object_size,
                to_hr_size(preferred as u64 * PAGE_SIZE as u64),
                to_hr_number(use_count),
                to_hr_size(memory),
                to_hr_size(unused),
                wasted_percent as u32
            )?;
        }
        write!(it, "\nPage spans:\n")?;
        writeln!(it, "index  size  free  used spans")?;

        // The span table is a snapshot: the page walk and the free lists are both read here,
        // before any row is written. Writing a row may allocate (the writer grows its buffer),
        // and an allocation that splits a large span links its upper halves into the smaller
        // lists; reading a list after such a write would count free spans that the walk never
        // saw, and its free pages could exceed the pages of all spans of that size.
        let mut span_size_histogram = [0u32; NR_SPAN_LISTS];
        let mut span_free_pages = [0u32; NR_SPAN_LISTS];

        let mut i: u32 = 0;
        while i < nr_pages {
            // SAFETY: `i < nr_pages`, so a page of the live page array; a raw place read.
            let span_size = unsafe { (*pg(pages, i as usize)).span_size };
            if span_size == 0 {
                i += 1;
                continue;
            }
            span_size_histogram[log2ceil_u32(span_size) as usize] += 1;
            i += span_size;
        }

        for (list, free_pages) in span_free_pages.iter_mut().enumerate() {
            // SAFETY: as at the top of this function, a raw place read of the thread's live
            // cpu_mem, no reference held across the writer.
            let mut front = unsafe { (*cpu_mem).free_spans[list]._front };
            while front != 0 {
                // SAFETY: `front` is a nonzero page index taken from a free span list, so a
                // page of the live page array; raw place reads.
                let (span_size, next) = unsafe {
                    let span = pg(pages, front as usize);
                    ((*span).span_size, (*span).link._next)
                };
                *free_pages += span_size;
                front = next;
            }
        }

        // An index loop: `i` indexes both tables and gives the span size `1 << i`.
        #[allow(clippy::needless_range_loop)]
        for i in 0..NR_SPAN_LISTS {
            // Every span of free list `i` heads a span of `1 << i` pages that the walk
            // counted, so `free_pages <= total_pages`.
            let free_pages = span_free_pages[i];
            let total_spans = span_size_histogram[i];
            let total_pages = total_spans * (1 << i);
            writeln!(
                it,
                "{:>5} {:>5} {:>5} {:>5} {:>5}",
                i,
                to_hr_size((1u64 << i) * PAGE_SIZE as u64),
                to_hr_size(free_pages as u64 * PAGE_SIZE as u64),
                to_hr_size((total_pages - free_pages) as u64 * PAGE_SIZE as u64),
                to_hr_number(total_spans as u64)
            )?;
        }

        Ok(())
    }

    /// Logs the current shard's diagnostics report as one message on the `engine_memory`
    /// logger at level `lvl`, if that level is enabled and `rate_limit` allows it.
    pub fn dump_memory_diagnostics(lvl: LogLevel, rate_limit: &mut RateLimit) {
        let mut writer = LambdaLogWriter::new(|mut it: InserterIterator| {
            // Writing into the log buffer cannot fail (it grows as needed).
            let _ = do_dump_memory_diagnostics(&mut it);
            it
        });
        ENGINE_MEMORY_LOGGER.log_writer(lvl, rate_limit, &mut writer, FormatInfo::default());
    }

    /// Logs the current shard's diagnostics report at level `lvl` on the `engine_memory`
    /// logger, with no rate limit (an explicit request).
    pub fn log_memory_diagnostics_report_impl(lvl: LogLevel) {
        let mut rl = RateLimit::new(std::time::Duration::from_secs(0)); // never limit for explicit dump requests
        dump_memory_diagnostics(lvl, &mut rl);
    }

    thread_local! {
        static RATE_LIMIT: RefCell<RateLimit> = RefCell::new(RateLimit::new(std::time::Duration::from_secs(10)));
    }

    /// Reports a failed allocation of `size` bytes: logs it with its backtrace at debug level,
    /// then the diagnostics report at error level when the failure kind set with
    /// [`set_dump_memory_diagnostics_on_alloc_failure_kind`] covers it or `is_aborting`, at
    /// debug level otherwise; at most one report per 10 seconds per thread. Does nothing when
    /// called again from inside a report on the same thread (an allocation that fails while
    /// reporting).
    pub fn maybe_dump_memory_diagnostics(size: usize, is_aborting: bool) {
        if REPORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.get()) != 0 {
            return;
        }

        let _guard = DisableReportOnAllocFailureTemporarily::new();
        if ENGINE_MEMORY_LOGGER.is_enabled(LogLevel::Debug) {
            crate::log_debug!(
                ENGINE_MEMORY_LOGGER,
                "Failed to allocate {} bytes at {}",
                size,
                current_backtrace()
            );
        }

        let mut lvl = match alloc_failure_kind_from_u8(
            DUMP_DIAGNOSTICS_ON_ALLOC_FAILURE_KIND.load(Ordering::Relaxed),
        ) {
            AllocFailureKind::None => LogLevel::Debug,
            AllocFailureKind::Critical => {
                if is_critical_alloc_section() {
                    LogLevel::Error
                } else {
                    LogLevel::Debug
                }
            }
            AllocFailureKind::All => LogLevel::Error,
        };

        if is_aborting {
            // if we are about to abort, always report the memory diagnositics at error level
            lvl = LogLevel::Error;
        }

        RATE_LIMIT.with(|rl| dump_memory_diagnostics(lvl, &mut rl.borrow_mut()));
    }

    /// Handles a failed allocation of `size` bytes: counts it, reports it (see
    /// [`maybe_dump_memory_diagnostics`]) and, if [`set_abort_on_allocation_failure`] is on and
    /// no [`DisableAbortOnAllocFailureTemporarily`] guard is alive on this thread, logs an
    /// error and aborts the process. Otherwise returns, and the caller returns null. Out of
    /// line, off the fast paths.
    #[inline(never)]
    pub fn on_allocation_failure(size: usize) {
        alloc_stats::increment(alloc_stats::Types::FailedAllocs, 1);

        let will_abort = ABORT_ON_ALLOC_FAILURE_SUPPRESSED.with(|c| c.get()) == 0
            && ABORT_ON_ALLOCATION_FAILURE.load(Ordering::Relaxed);

        maybe_dump_memory_diagnostics(size, will_abort);

        if will_abort {
            crate::log_error!(engine_logger(), "Failed to allocate {} bytes", size);
            // SAFETY: abort has no preconditions.
            unsafe { abort() };
        }
    }

    /// Returns the current shard's diagnostics report (see [`do_dump_memory_diagnostics`]) as a
    /// string. Allocates the string. The thread's pool must be set up.
    pub fn generate_memory_diagnostics_report() -> Sstring {
        let mut buf = String::new();
        let _ = do_dump_memory_diagnostics(&mut buf);
        Sstring::from(buf.as_str())
    }

    /// Throws (returns the error of) the allocation failure injector: its failure action
    /// throws, which here is a panic, caught and returned as the exception.
    #[inline(always)]
    fn trigger_error_injector() -> Result<(), ExceptionPtr> {
        std::panic::catch_unwind(on_alloc_point).map_err(ExceptionPtr::from_panic)
    }

    /// Returns whether the failure injector fires at this allocation point (always false
    /// without the injector compiled in).
    #[inline(always)]
    fn try_trigger_error_injector() -> bool {
        std::panic::catch_unwind(on_alloc_point).is_err()
    }

    /// Returns a copy of every live allocation site the current shard's heap profiler holds
    /// (empty when profiling never sampled anything), in no particular order. Sampling is
    /// paused while it copies. The thread's pool must be set up.
    pub fn sampled_memory_profile() -> Vec<AllocationSite> {
        let _dbt = DisableBacktraceTemporarily::new();
        // SAFETY: the thread's cpu_mem, borrowed for this one call (see get_cpu_mem).
        unsafe { (*get_cpu_mem()).alloc_sites() }.iter().map(|s| (**s).clone()).collect()
    }

    /// Copies up to `output.len()` allocation sites of the current shard's heap profiler into
    /// the front of `output` (in no particular order, with `clone_from`, so a backtrace may
    /// allocate) and returns how many were copied. Sampling is not paused. The thread's pool
    /// must be set up.
    pub fn sampled_memory_profile_into(output: &mut [AllocationSite]) -> usize {
        let size = output.len();
        // SAFETY: the thread's cpu_mem, borrowed while the sites are read (see get_cpu_mem).
        // The set's keys hash and compare by backtrace only (see `alloc_sites`).
        #[allow(clippy::mutable_key_type)]
        let sites = unsafe { (*get_cpu_mem()).alloc_sites() };
        let to_copy = size.min(sites.len());
        for (dst, src) in output.iter_mut().zip(sites.iter()).take(to_copy) {
            dst.clone_from(src);
        }
        to_copy
    }

    // ---- The C allocation interface (malloc and friends) -------------------------------------

    /// The C `malloc`, exported under its C name so that it replaces libc's in the process:
    /// [`allocate`]`(n)` (the current shard's pools on a shard, libc's `malloc` on another
    /// thread once it has been looked up). Returns null when the allocation failure injector
    /// fires (a `catch_unwind` around the injection point, a no-op without
    /// the `alloc_failure_injection` feature) or the allocation fails; `malloc(0)` returns a
    /// unique block of at least 8 bytes.
    #[unsafe(no_mangle)]
    pub extern "C" fn malloc(n: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        allocate(n)
    }

    /// Alias of [`malloc`] under glibc's internal name.
    #[unsafe(no_mangle)]
    pub extern "C" fn __libc_malloc(n: usize) -> *mut c_void {
        malloc(n)
    }

    // `free` itself: the Rust function named `free` above is `memory::free`; the C symbol is
    // exported under its C name here.
    /// The C `free`, exported under its C name: [`free`]`(ptr)`, for memory of any origin; null
    /// is ignored.
    #[unsafe(export_name = "free")]
    pub extern "C" fn c_free(ptr: *mut c_void) {
        free(ptr);
    }

    /// Alias of `free` under glibc's internal name.
    #[unsafe(no_mangle)]
    pub extern "C" fn __libc_free(obj: *mut c_void) {
        c_free(obj);
    }

    /// The C `calloc`: [`malloc`]`(nmemb * size)`, zeroed; null when the failure injector fires
    /// or the allocation fails, and when `nmemb * size` overflows. The block is always zeroed,
    /// even when fresh pages already are.
    #[unsafe(no_mangle)]
    pub extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        // A product that overflows is an allocation that cannot be made: null, not an abort.
        let Some(s) = nmemb.checked_mul(size) else {
            return std::ptr::null_mut();
        };
        let p = malloc(s);
        if !p.is_null() {
            // SAFETY: `p` points to `s` writable bytes.
            unsafe { std::ptr::write_bytes(p as *mut u8, 0, s) };
        }
        p
    }

    /// Alias of [`calloc`] under glibc's internal name.
    #[unsafe(no_mangle)]
    pub extern "C" fn __libc_calloc(n: usize, m: usize) -> *mut c_void {
        calloc(n, m)
    }

    /// The C `realloc`. Null `ptr` is [`malloc`]`(size)`. A block of libc's goes to libc's
    /// `realloc` (the process aborts if that has not been looked up yet). For shard memory:
    /// the same usable size returns `ptr`; `size == 0` frees it and returns null; a smaller
    /// size for a block of the calling shard is shrunk in place ([`shrink`]: large blocks
    /// give pages back, small ones stay as they are); anything else (growing, or a block of
    /// another shard) allocates a new block on this thread, copies, and frees the old one,
    /// returning null and keeping `ptr` if the allocation fails. Returns null without touching
    /// `ptr` when the failure injector fires.
    ///
    /// # Safety
    /// The C `realloc` contract: `ptr` is null or a live block from this allocator or libc's,
    /// not used after the call unless returned.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        if ptr.is_null() {
            // A null pointer behaves as malloc(size), as the C standard specifies.
            return malloc(size);
        } else if !is_engine_memory(ptr) {
            // libc's realloc may not be looked up yet when an earlier initializer allocates.
            if let Some(f) = libc!(LIBC_REALLOC_FUNC, ReallocFuncType) {
                // SAFETY: `ptr` belongs to libc's allocator.
                return unsafe { f(ptr, size) };
            }
            // Memory not owned by a shard cannot be reallocated without libc's realloc.
            // SAFETY: abort has no preconditions.
            unsafe { abort() };
        }
        // if we're here, it's a non-null engine memory ptr
        let old_size = if !ptr.is_null() { object_size(ptr) } else { 0 };
        if size == old_size {
            return ptr;
        }
        if size == 0 {
            c_free(ptr);
            return std::ptr::null_mut();
        }
        if size < old_size && CpuPages::is_local_pointer(ptr) {
            // local pointers can sometimes be shrunk by returning freed
            // pages to the buddy allocator
            shrink(ptr, size);
            return ptr;
        }

        // either a request to realloc larger than the existing allocation size
        // or a cross-shard pointer: in either case we allocate a new local
        // pointer and copy the contents
        let nptr = malloc(size);
        if nptr.is_null() {
            return nptr;
        }
        if !ptr.is_null() {
            // SAFETY: both blocks hold at least min(size, old_size) bytes and are distinct.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr as *const u8, nptr as *mut u8, size.min(old_size))
            };
            c_free(ptr);
        }
        nptr
    }

    /// # Safety
    /// As [`realloc`].
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __libc_realloc(obj: *mut c_void, size: usize) -> *mut c_void {
        // SAFETY: forwarded from the caller.
        unsafe { realloc(obj, size) }
    }

    /// The C `posix_memalign`, exported under its C name: stores
    /// [`allocate_aligned`]`(align, size)` in `*ptr` and returns 0, or `ENOMEM` when it is
    /// null or the failure injector fires (then `*ptr` is not written).
    /// `align` is not validated, so it never returns `EINVAL`; the block is aligned to `align`
    /// whatever `size` (see [`allocate_aligned`]).
    ///
    /// # Safety
    /// The C `posix_memalign` contract: `ptr` is valid for a write of one pointer.
    #[unsafe(export_name = "posix_memalign")]
    pub unsafe extern "C" fn c_posix_memalign(
        ptr: *mut *mut c_void,
        align: usize,
        size: usize,
    ) -> c_int {
        if try_trigger_error_injector() {
            return ENOMEM;
        }
        let p = allocate_aligned(align, size);
        // POSIX leaves `*ptr` unchanged when the allocation fails.
        if p.is_null() {
            return ENOMEM;
        }
        // SAFETY: `ptr` is non-null (nonnull(1)) and writable (by the caller).
        unsafe { *ptr = p };
        0
    }

    /// # Safety
    /// As [`c_posix_memalign`].
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __libc_posix_memalign(
        ptr: *mut *mut c_void,
        align: usize,
        size: usize,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { c_posix_memalign(ptr, align, size) }
    }

    /// The obsolete `memalign(3)`: `size` rounded up to a multiple of `align`, then
    /// [`allocate_aligned`], so the block is aligned to `align` whatever `size`. Null when the
    /// failure injector fires or the allocation fails.
    #[unsafe(no_mangle)]
    pub extern "C" fn memalign(align: usize, mut size: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        size = crate::engine::core::align::align_up(size, align);
        allocate_aligned(align, size)
    }

    /// The C `aligned_alloc`: [`allocate_aligned`]`(align, size)`, null when the failure
    /// injector fires or the allocation fails. `align` is not validated, and `size` need not
    /// be a multiple of it: the block is aligned to `align` whatever `size` (see
    /// [`allocate_aligned`]).
    #[unsafe(no_mangle)]
    pub extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        allocate_aligned(align, size)
    }

    /// Alias of [`memalign`] under glibc's internal name.
    #[unsafe(no_mangle)]
    pub extern "C" fn __libc_memalign(align: usize, size: usize) -> *mut c_void {
        memalign(align, size)
    }

    /// The obsolete `cfree(3)`, the same as `free`.
    #[unsafe(no_mangle)]
    pub extern "C" fn cfree(obj: *mut c_void) {
        c_free(obj)
    }

    /// Alias of [`cfree`] under glibc's internal name.
    #[unsafe(no_mangle)]
    pub extern "C" fn __libc_cfree(obj: *mut c_void) {
        cfree(obj)
    }

    /// The glibc `malloc_usable_size`: for shard memory, [`object_size`] (the size class or the
    /// span size); for anything else (null included), libc's `malloc_usable_size`, or 0 before
    /// it has been looked up.
    ///
    /// # Safety
    /// The C `malloc_usable_size` contract: `obj` is null or a live block from this
    /// allocator or libc's.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc_usable_size(obj: *mut c_void) -> usize {
        if !is_engine_memory(obj) {
            return match libc!(LIBC_MALLOC_USABLE_SIZE_FUNC, MallocUsableSizeType) {
                // SAFETY: `obj` belongs to libc's allocator (by the caller).
                Some(f) => unsafe { f(obj) },
                None => 0,
            };
        }
        object_size(obj)
    }

    /// The glibc `malloc_trim`: on a thread that is not a shard, forwarded to libc's (0 before
    /// it has been looked up); on a shard, a no-op that returns 0 (no memory released).
    #[unsafe(no_mangle)]
    pub extern "C" fn malloc_trim(pad: usize) -> c_int {
        if !is_reactor_thread() {
            return match libc!(LIBC_MALLOC_TRIM_FUNC, MallocTrimType) {
                // SAFETY: calling libc's malloc_trim, found with dlsym(RTLD_NEXT).
                Some(f) => unsafe { f(pad) },
                None => 0,
            };
        }
        0
    }

    // ---- The native `operator new` / `delete` interface, by their Itanium ABI names ----------
    // These serve native libraries linked into the process. A failed throwing `new` raises the
    // allocation error through the native runtime those libraries link (its `__throw_bad_alloc`,
    // found with dlsym so that a binary without that runtime still links); the throwing
    // operators are `extern "C-unwind"` so the error can cross them. Without that runtime nobody
    // can call them, and they abort.

    /// The `RTLD_DEFAULT` pseudo-handle of `dlsym(3)`.
    const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();

    /// Raises the native runtime's allocation error; aborts if that runtime is not linked.
    #[cold]
    fn throw_bad_alloc() -> ! {
        // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name has no other precondition.
        let f = unsafe { dlsym(RTLD_DEFAULT, c"_ZSt17__throw_bad_allocv".as_ptr()) };
        if !f.is_null() {
            // SAFETY: the symbol is the native runtime's `__throw_bad_alloc`, which takes no
            // arguments, unwinds and never returns.
            let throw: extern "C-unwind" fn() -> ! = unsafe { std::mem::transmute(f) };
            throw();
        }
        // SAFETY: abort has no preconditions.
        unsafe { abort() }
    }

    /// Returns `ptr`, or raises the allocation error if it is null.
    #[inline]
    fn throw_if_null(ptr: *mut c_void) -> *mut c_void {
        if ptr.is_null() {
            throw_bad_alloc();
        }
        ptr
    }

    /// Raises the allocation error if the failure injector fires at this point.
    fn trigger_or_throw() {
        if trigger_error_injector().is_err() {
            throw_bad_alloc();
        }
    }

    /// Allocates `size` bytes for `new`. On failure, or when the allocation failure injector fires, raises the allocation error of the
    /// native runtime (or aborts if none is linked).
    ///
    /// Exported as `_Znwm`.
    #[unsafe(export_name = "_Znwm")]
    pub extern "C-unwind" fn operator_new(size: usize) -> *mut c_void {
        trigger_or_throw();
        throw_if_null(allocate(size))
    }

    /// Allocates `size` bytes for array `new`. On failure, or when the allocation failure injector fires, raises the allocation error of the
    /// native runtime (or aborts if none is linked).
    ///
    /// Exported as `_Znam`.
    #[unsafe(export_name = "_Znam")]
    pub extern "C-unwind" fn operator_new_array(size: usize) -> *mut c_void {
        trigger_or_throw();
        throw_if_null(allocate(size))
    }

    /// Frees `ptr` for `delete`.
    ///
    /// Exported as `_ZdlPv`.
    #[unsafe(export_name = "_ZdlPv")]
    pub extern "C" fn operator_delete(ptr: *mut c_void) {
        free(ptr);
    }

    /// Frees `ptr` for array `delete`.
    ///
    /// Exported as `_ZdaPv`.
    #[unsafe(export_name = "_ZdaPv")]
    pub extern "C" fn operator_delete_array(ptr: *mut c_void) {
        free(ptr);
    }

    /// Frees `ptr` for `delete`, with its allocation size.
    ///
    /// Exported as `_ZdlPvm`.
    #[unsafe(export_name = "_ZdlPvm")]
    pub extern "C" fn operator_delete_sized(ptr: *mut c_void, size: usize) {
        free_sized(ptr, size);
    }

    /// Frees `ptr` for array `delete`, with its allocation size.
    ///
    /// Exported as `_ZdaPvm`.
    #[unsafe(export_name = "_ZdaPvm")]
    pub extern "C" fn operator_delete_array_sized(ptr: *mut c_void, size: usize) {
        free_sized(ptr, size);
    }

    /// Allocates `size` bytes for `new`. Returns null on failure.
    ///
    /// Exported as `_ZnwmSt9nothrow_t`.
    #[unsafe(export_name = "_ZnwmSt9nothrow_t")]
    pub extern "C" fn operator_new_nothrow(size: usize) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        allocate(size)
    }

    /// Allocates `size` bytes for array `new`. Returns null on failure. Unlike the other
    /// allocation entry points it does not consult the allocation failure injector.
    ///
    /// Exported as `_ZnamSt9nothrow_t`.
    #[unsafe(export_name = "_ZnamSt9nothrow_t")]
    pub extern "C" fn operator_new_array_nothrow(size: usize) -> *mut c_void {
        allocate(size)
    }

    /// Frees `ptr` for `delete`.
    ///
    /// Exported as `_ZdlPvSt9nothrow_t`.
    #[unsafe(export_name = "_ZdlPvSt9nothrow_t")]
    pub extern "C" fn operator_delete_nothrow(ptr: *mut c_void) {
        free(ptr);
    }

    /// Frees `ptr` for array `delete`.
    ///
    /// Exported as `_ZdaPvSt9nothrow_t`.
    #[unsafe(export_name = "_ZdaPvSt9nothrow_t")]
    pub extern "C" fn operator_delete_array_nothrow(ptr: *mut c_void) {
        free(ptr);
    }

    /// Frees `ptr` for `delete`, with its allocation size.
    ///
    /// Exported as `_ZdlPvmSt9nothrow_t`.
    #[unsafe(export_name = "_ZdlPvmSt9nothrow_t")]
    pub extern "C" fn operator_delete_sized_nothrow(ptr: *mut c_void, size: usize) {
        free_sized(ptr, size);
    }

    /// Frees `ptr` for array `delete`, with its allocation size.
    ///
    /// Exported as `_ZdaPvmSt9nothrow_t`.
    #[unsafe(export_name = "_ZdaPvmSt9nothrow_t")]
    pub extern "C" fn operator_delete_array_sized_nothrow(ptr: *mut c_void, size: usize) {
        free_sized(ptr, size);
    }

    /// Allocates `size` bytes for `new`, aligned to `a`. On failure, or when the allocation failure injector fires, raises the allocation error of the
    /// native runtime (or aborts if none is linked).
    ///
    /// Exported as `_ZnwmSt11align_val_t`.
    #[unsafe(export_name = "_ZnwmSt11align_val_t")]
    pub extern "C-unwind" fn operator_new_aligned(size: usize, a: usize) -> *mut c_void {
        trigger_or_throw();
        let ptr = allocate_aligned(a, size);
        throw_if_null(ptr)
    }

    /// Allocates `size` bytes for array `new`, aligned to `a`. On failure, or when the allocation failure injector fires, raises the allocation error of the
    /// native runtime (or aborts if none is linked).
    ///
    /// Exported as `_ZnamSt11align_val_t`.
    #[unsafe(export_name = "_ZnamSt11align_val_t")]
    pub extern "C-unwind" fn operator_new_array_aligned(size: usize, a: usize) -> *mut c_void {
        trigger_or_throw();
        let ptr = allocate_aligned(a, size);
        throw_if_null(ptr)
    }

    /// Allocates `size` bytes for `new`, aligned to `a`. Returns null on failure.
    ///
    /// Exported as `_ZnwmSt11align_val_tRKSt9nothrow_t`.
    #[unsafe(export_name = "_ZnwmSt11align_val_tRKSt9nothrow_t")]
    pub extern "C" fn operator_new_aligned_nothrow(
        size: usize,
        a: usize,
        _nt: *const c_void,
    ) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        allocate_aligned(a, size)
    }

    /// Allocates `size` bytes for array `new`, aligned to `a`. Returns null on failure.
    ///
    /// Exported as `_ZnamSt11align_val_tRKSt9nothrow_t`.
    #[unsafe(export_name = "_ZnamSt11align_val_tRKSt9nothrow_t")]
    pub extern "C" fn operator_new_array_aligned_nothrow(
        size: usize,
        a: usize,
        _nt: *const c_void,
    ) -> *mut c_void {
        if try_trigger_error_injector() {
            return std::ptr::null_mut();
        }
        allocate_aligned(a, size)
    }

    /// Frees `ptr` for `delete`.
    ///
    /// Exported as `_ZdlPvSt11align_val_t`.
    #[unsafe(export_name = "_ZdlPvSt11align_val_t")]
    pub extern "C" fn operator_delete_aligned(ptr: *mut c_void, _a: usize) {
        free(ptr);
    }

    /// Frees `ptr` for array `delete`.
    ///
    /// Exported as `_ZdaPvSt11align_val_t`.
    #[unsafe(export_name = "_ZdaPvSt11align_val_t")]
    pub extern "C" fn operator_delete_array_aligned(ptr: *mut c_void, _a: usize) {
        free(ptr);
    }

    /// Frees `ptr` for `delete`, with its allocation size;
    /// the alignment is only used to find the size class.
    ///
    /// Exported as `_ZdlPvmSt11align_val_t`.
    #[unsafe(export_name = "_ZdlPvmSt11align_val_t")]
    pub extern "C" fn operator_delete_sized_aligned(ptr: *mut c_void, size: usize, a: usize) {
        if !ptr.is_null() {
            free_aligned(ptr, a, size);
        }
    }

    /// Frees `ptr` for array `delete`, with its allocation size;
    /// the alignment is only used to find the size class.
    ///
    /// Exported as `_ZdaPvmSt11align_val_t`.
    #[unsafe(export_name = "_ZdaPvmSt11align_val_t")]
    pub extern "C" fn operator_delete_array_sized_aligned(ptr: *mut c_void, size: usize, a: usize) {
        if !ptr.is_null() {
            free_aligned(ptr, a, size);
        }
    }

    /// Frees `ptr` for `delete`.
    ///
    /// Exported as `_ZdlPvSt11align_val_tRKSt9nothrow_t`.
    #[unsafe(export_name = "_ZdlPvSt11align_val_tRKSt9nothrow_t")]
    pub extern "C" fn operator_delete_aligned_nothrow(
        ptr: *mut c_void,
        _a: usize,
        _nt: *const c_void,
    ) {
        free(ptr);
    }

    /// Frees `ptr` for array `delete`.
    ///
    /// Exported as `_ZdaPvSt11align_val_tRKSt9nothrow_t`.
    #[unsafe(export_name = "_ZdaPvSt11align_val_tRKSt9nothrow_t")]
    pub extern "C" fn operator_delete_array_aligned_nothrow(
        ptr: *mut c_void,
        _a: usize,
        _nt: *const c_void,
    ) {
        free(ptr);
    }

    // ---- The Rust allocation interface ------------------------------------------------------

    /// The engine allocator as the Rust global allocator. Install it with `#[global_allocator] static A: EngineAllocator = EngineAllocator;`.
    ///
    /// Layouts whose alignment the small pools already guarantee (`align` at most that of
    /// `max_align_t`, and `size` a multiple of `align`) take the plain allocation and sized free
    /// paths; the others take the aligned paths, with `size.max(align)` as the size (the
    /// aligned paths raise a smaller size to `align` themselves, so this only computes the
    /// same class size before the call).
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EngineAllocator;

    /// Whether `layout` can take the plain (unaligned) paths.
    #[inline(always)]
    fn plain_layout(layout: std::alloc::Layout) -> bool {
        // Size classes up to 16 bytes are not multiples of every smaller alignment (12-byte
        // objects sit at 12·k), so the size must also be a multiple of the alignment (which,
        // as GlobalAlloc sizes are never zero, also gives `align <= size`).
        // `Layout` guarantees a power-of-two alignment, so the multiple check is a mask
        // (the usual way for allocators): `__rust_alloc` is not inlined
        // into its callers, so the alignment is not a constant there and `%` would be a
        // division.
        // Both tests in one: with `mask = align - 1`, `align <= 16` is `mask & !15 == 0` and
        // "size is a multiple of align" is `mask & size == 0`, so together they are
        // `mask & (size | !15) == 0`, one test and one branch where `&&` makes two.
        const _: () = assert!(MAX_ALIGN_T_ALIGN.is_power_of_two());
        (layout.align() - 1) & (layout.size() | !(MAX_ALIGN_T_ALIGN - 1)) == 0
    }

    /// The size an aligned (non-plain) layout is allocated and freed with: the same value in
    /// `alloc`, `dealloc` and `realloc`, so a block is always returned to the pool or span
    /// size it came from.
    #[inline(always)]
    fn aligned_layout_size(layout: std::alloc::Layout) -> usize {
        layout.size().max(layout.align())
    }

    // SAFETY: allocate/allocate_aligned return blocks of at least the requested size and
    // alignment (or null), and free/free_aligned release blocks obtained from them.
    unsafe impl std::alloc::GlobalAlloc for EngineAllocator {
        #[inline(always)]
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if plain_layout(layout) {
                allocate(layout.size()) as *mut u8
            } else {
                allocate_aligned(layout.align(), aligned_layout_size(layout)) as *mut u8
            }
        }

        #[inline(always)]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // free(obj, size) and free_aligned(obj, align, size) both start with
            // try_free_fastpath(obj), which does not read the size: try it first, so that the
            // layout is only looked at on the slow path (otherwise LLVM computes the adjusted
            // size, and duplicates the fast path, before it).
            let obj = ptr as *mut c_void;
            if likely(CpuPages::try_free_fastpath(obj)) {
                return;
            }
            let size = if plain_layout(layout) {
                layout.size()
            } else {
                free_aligned_size(layout.align(), aligned_layout_size(layout))
            };
            size.free_slowpath(obj);
        }

        #[inline]
        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            // SAFETY: GlobalAlloc::realloc guarantees new_size, rounded up to the alignment, does
            // not overflow, so the alignment stays a valid one.
            let new_layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(new_size, layout.align()) };
            // The shortcuts hold only for a block of the engine allocator: a block that a
            // thread without cpu_mem got from libc's malloc is exactly its size and must
            // go through the copy below (or libc's realloc), never be handed back grown.
            if plain_layout(layout)
                && plain_layout(new_layout)
                && is_engine_memory(ptr as *const c_void)
            {
                // `dealloc` frees by the layout's size, which picks the small pool: the block
                // must stay in the pool the new size maps to, as the sized free expects.
                let pool_of =
                    |size: usize| SmallPool::size_to_idx(size.max(size_of::<FreeObject>()) as u32);
                let (old_small, new_small) =
                    (layout.size() <= MAX_SMALL_ALLOCATION, new_size <= MAX_SMALL_ALLOCATION);
                if old_small && new_small && pool_of(layout.size()) == pool_of(new_size) {
                    // Same pool: the block already holds `new_size` bytes.
                    return ptr;
                }
                if !old_small && !new_small {
                    // Both large: freed by their span whatever the size; realloc grows or
                    // shrinks in place, page-aligned.
                    // SAFETY: `ptr` is a live block of this allocator (the caller's contract).
                    return unsafe { realloc(ptr as *mut c_void, new_size) } as *mut u8;
                }
            }
            // SAFETY: the caller upholds GlobalAlloc::realloc's contract.
            unsafe {
                let nptr = self.alloc(new_layout);
                if !nptr.is_null() {
                    std::ptr::copy_nonoverlapping(ptr, nptr, layout.size().min(new_size));
                    self.dealloc(ptr, layout);
                }
                nptr
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn hr(value: u64, step: u64, precision: u64, suffixes: &[u8; 5]) -> (u16, u8) {
            let v = to_human_readable_value(value, step, precision, suffixes);
            (v.value, v.suffix)
        }

        // Values past the largest unit stay in that unit instead of being divided once or
        // twice more and shown with its suffix.
        #[test]
        fn human_readable_value_stops_at_the_largest_suffix() {
            const T: u64 = 1 << 40;
            assert_eq!(hr(8 * T, 1024, 8192, b"BKMGT"), (8, b'T'));
            assert_eq!(hr(8191 * T, 1024, 8192, b"BKMGT"), (8191, b'T'));
            assert_eq!(hr(9000 * T, 1024, 8192, b"BKMGT"), (9000, b'T'));
            assert_eq!(hr(9000 * 1024 * T, 1024, 8192, b"BKMGT").1, b'T');
            assert_eq!(hr(5 * 1024, 1024, 8192, b"BKMGT"), (5, b'K'));
            assert_eq!(hr(9 * 1024, 1024, 8192, b"BKMGT"), (9, b'K'));
        }
    }
}

#[cfg(not(all(target_os = "linux", not(engine_default_allocator))))]
mod default_allocator {
    use super::*;
    use crate::engine::core::reactor::engine_logger;

    impl DisableBacktraceTemporarily {
        /// There is no heap profiler without the engine allocator: returns a guard that pauses
        /// nothing.
        pub fn new() -> Self {
            Self { _disable_sampling: DisableSamplingTemporarily::default() }
        }
    }

    /// There is no heap profiler without the engine allocator: logs a warning on the engine
    /// logger, whatever the rate, and does nothing else.
    pub fn set_heap_profiling_sampling_rate(_: usize) {
        crate::log_warn!(
            engine_logger(),
            "Engine compiled with default allocator, heap profiler not supported"
        );
    }

    /// There is no heap profiler without the engine allocator: always 0.
    pub fn get_heap_profiling_sample_rate() -> usize {
        0
    }

    /// There is no heap profiler without the engine allocator: always empty.
    pub fn sampled_memory_profile() -> Vec<AllocationSite> {
        Vec::new()
    }

    /// There is no heap profiler without the engine allocator: copies nothing and returns 0.
    pub fn sampled_memory_profile_into(_output: &mut [AllocationSite]) -> usize {
        0
    }

    impl ScopedHeapProfiling {
        /// There is no heap profiler without the engine allocator: logs the warning of
        /// [`set_heap_profiling_sampling_rate`] and returns a guard whose drop does nothing.
        pub fn new(sample_rate: usize) -> Self {
            set_heap_profiling_sampling_rate(sample_rate); // let it print the warning
            Self { _private: () }
        }
    }

    impl Drop for ScopedHeapProfiling {
        fn drop(&mut self) {}
    }

    /// Without the engine allocator failures are not intercepted: `true` only logs a warning
    /// that the process will not abort, `false` does nothing.
    pub fn set_abort_on_allocation_failure(enabled: bool) {
        if enabled {
            crate::log_warn!(
                engine_logger(),
                "Engine compiled with default allocator, will not abort on bad_alloc"
            );
        }
    }

    /// Without the engine allocator: always false.
    pub fn is_abort_on_allocation_failure() -> bool {
        false
    }

    impl Reclaimer {
        /// Boxes a reclaimer that receives the [`ReclaimerRequest`]; without the engine
        /// allocator it is not registered anywhere and is never called.
        pub fn with_request(
            reclaim: Box<dyn FnMut(ReclaimerRequest) -> ReclaimingResult>,
            scope: ReclaimerScope,
        ) -> Box<Reclaimer> {
            Box::new(Reclaimer { _reclaim: reclaim, _scope: scope })
        }
    }

    impl Drop for Reclaimer {
        fn drop(&mut self) {}
    }

    /// Without the engine allocator there is no reclaim: drops `hook`.
    pub fn set_reclaim_hook(_hook: ReclaimHook) {}

    /// Without the engine allocator: ignores its arguments and returns an empty layout; never
    /// fails.
    pub fn configure(
        _m: Vec<resource::Memory>,
        _mbind: bool,
        _transparent_hugepages: bool,
        _hugepages_path: Option<String>,
    ) -> Result<internal::NumaLayout, ExceptionPtr> {
        Ok(internal::NumaLayout::default())
    }

    /// Without the engine allocator: does nothing.
    pub fn configure_minimal() {}

    /// Without the engine allocator: every counter 0, and 1 GiB of total and of free memory.
    pub fn stats() -> Statistics {
        Statistics::new(0, 0, 0, 1 << 30, 1 << 30, 0, 0, 0, 0, 0, 0)
    }

    /// Without the engine allocator: always 1 GiB (see [`stats`]).
    pub fn free_memory() -> usize {
        stats().free_memory()
    }

    /// Without the engine allocator there is no cross-shard list: always false.
    pub fn drain_cross_cpu_freelist() -> bool {
        false
    }

    /// Without the engine allocator there is no shard pool.
    ///
    /// # Errors
    /// Always fails, with a runtime error.
    pub fn get_memory_layout() -> Result<MemoryLayout, ExceptionPtr> {
        Err(make_exception_ptr(crate::engine::core::future::RuntimeError::new(
            "get_memory_layout() not supported".to_string(),
        )))
    }

    /// Without the engine allocator there is no reclaim: always 0.
    pub fn min_free_memory() -> usize {
        0
    }

    /// Without the engine allocator there is no reclaim: ignores `pages`.
    ///
    /// # Errors
    /// Never fails.
    pub fn set_min_free_pages(_pages: usize) -> Result<(), ExceptionPtr> {
        // Ignore, reclaiming not supported for default allocator.
        Ok(())
    }

    /// Without the engine allocator there are no large allocation warnings: does nothing.
    pub fn set_large_allocation_warning_threshold(_: usize) {
        // Ignore, not supported for default allocator.
    }

    /// Without the engine allocator there are no large allocation warnings: always
    /// `usize::MAX` (disabled).
    pub fn get_large_allocation_warning_threshold() -> usize {
        // Ignore, not supported for default allocator.
        usize::MAX
    }

    /// Without the engine allocator: does nothing.
    pub fn disable_large_allocation_warning() {
        // Ignore, not supported for default allocator.
    }

    /// Without the engine allocator there is no diagnostics report: does nothing.
    pub fn set_dump_memory_diagnostics_on_alloc_failure_kind(_: AllocFailureKind) {
        // Ignore, not supported for default allocator.
    }

    /// Without the engine allocator there is no diagnostics report: does nothing, and does not
    /// validate the name.
    pub fn set_dump_memory_diagnostics_on_alloc_failure_kind_str(_: &str) {
        // Ignore, not supported for default allocator.
    }

    /// Without the engine allocator there is no diagnostics report: drops the producer.
    pub fn set_additional_diagnostics_producer(_: MemoryDiagnosticsProducer) {
        // Ignore, not supported for default allocator.
    }

    /// Without the engine allocator there is no diagnostics report: always an empty string.
    pub fn generate_memory_diagnostics_report() -> Sstring {
        // Ignore, not supported for default allocator.
        Sstring::default()
    }

    /// Without the engine allocator there is no diagnostics report: logs nothing.
    pub fn log_memory_diagnostics_report_impl(_lvl: LogLevel) {}
}
