//! A linear arena per request (012 FR-027): while a thread runs a request inside [`request`], the
//! small allocations it makes (decoding to owned values, evaluation, intermediate results) are
//! carved one after the other from chunks of [`CHUNK`] bytes instead of going to the system's
//! allocator, and the chunks are emptied at once when the request ends.
//!
//! [`ArenaAlloc`] is the process's global allocator. Outside a request, and for allocations of more
//! than [`LARGEST`] bytes or past [`REQUEST_CAP`] bytes in one request, it is the system's. Freeing
//! a block of the arena only counts it down in its chunk. When the request ends, each of its chunks
//! whose blocks were all freed goes back to the node's pool of free chunks at once; a chunk that
//! still holds a block (one that outlived the request: a cursor, a cached plan, an answer handed to
//! another thread) is retired, and the thread that frees its last block, whichever it is, gives it
//! back. So nothing that outlives its request is ever overwritten, and a request whose memory all
//! went with it costs no call to the system's allocator once the pool has a chunk.
//!
//! The pool is a stack without a lock (a counter in each pointer to it tells a stale one apart),
//! and no thread keeps a chunk between two requests: with a thread per connection, a chunk kept by
//! each thread stayed with it after it ended. The last [`KEEP_WARM`] chunks given back keep their
//! pages; the ones past them give their memory to the system.
//!
//! The chunks lie in one region of address space reserved at the first request, so that telling
//! a block of the arena from one of the system's is a comparison of addresses.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// Bytes of a chunk, aligned to its size.
pub const CHUNK: usize = 64 << 10;
/// Largest allocation the arena takes; larger ones go to the system's allocator.
pub const LARGEST: usize = 16 << 10;
/// Bytes one request takes from the arena at most: an aggregation that allocates and frees for
/// long goes on with the system's allocator past them, instead of growing its arena.
pub const REQUEST_CAP: usize = 32 << 20;
/// Bytes of address space of the region.
const REGION: usize = 64 << 30;
/// Bytes at the start of a chunk for its header: the count of its live blocks and the next chunk
/// of its request or of the free list.
const HEADER: usize = 64;
/// The bit of a chunk's count that says its request ended.
const RETIRED: u64 = 1 << 63;

/// The region: 0 until reserved.
static BASE: AtomicUsize = AtomicUsize::new(0);
/// 0: not reserved, 1: being reserved, 2: reserved, 3: refused (the arena stays off).
static REGION_STATE: AtomicU8 = AtomicU8::new(0);
/// Chunks of the region handed out at least once.
static FRESH: AtomicUsize = AtomicUsize::new(0);
/// Free chunks kept with their pages; the ones past them are given to the system.
pub const KEEP_WARM: usize = 64;
/// Free chunks, linked through their headers.
struct Pool {
    /// A counter in the high half and the index plus one of the first chunk in the low half (0:
    /// empty).
    top: AtomicU64,
    /// Chunks in it.
    count: AtomicUsize,
}

/// The node's pool of free chunks.
static POOL: Pool = Pool { top: AtomicU64::new(0), count: AtomicUsize::new(0) };

static GENERAL: AtomicU64 = AtomicU64::new(0);
static ARENA: AtomicU64 = AtomicU64::new(0);
static CHUNKS_OUT: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Nested [`request`] scopes of the thread.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// The chunk the request carves from (0: none) and the offset of its next free byte.
    static CUR: Cell<usize> = const { Cell::new(0) };
    static POS: Cell<usize> = const { Cell::new(0) };
    /// Bytes the request took from the arena.
    static USED: Cell<usize> = const { Cell::new(0) };
    /// The request's chunks, linked through their headers.
    static LIST: Cell<usize> = const { Cell::new(0) };
    /// Allocations of this thread that went to the system's allocator.
    static GENERAL_HERE: Cell<u64> = const { Cell::new(0) };
}

/// Allocations and reallocations of this thread that went to the system's allocator: what a test
/// counts around one request without the other threads' noise.
pub fn thread_general() -> u64 {
    GENERAL_HERE.get()
}

fn count_general() {
    GENERAL.fetch_add(1, Ordering::Relaxed);
    GENERAL_HERE.set(GENERAL_HERE.get() + 1);
}

/// What the allocator did since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaStats {
    /// Allocations and reallocations that went to the system's allocator.
    pub general: u64,
    /// Allocations carved from a request's arena.
    pub arena: u64,
    /// Chunks handed out and not given back: in use, kept by a thread, or holding a block that
    /// outlived its request.
    pub chunks_out: u64,
}

/// The allocator's counts.
pub fn stats() -> ArenaStats {
    ArenaStats {
        general: GENERAL.load(Ordering::Relaxed),
        arena: ARENA.load(Ordering::Relaxed),
        chunks_out: CHUNKS_OUT.load(Ordering::Relaxed),
    }
}

/// Runs `f` as a request of this thread: its small allocations come from the thread's arena,
/// emptied when `f` returns or unwinds. A request inside another one is part of it.
pub fn request<R>(f: impl FnOnce() -> R) -> R {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            let depth = DEPTH.get() - 1;
            DEPTH.set(depth);
            if depth == 0 {
                end_request();
            }
        }
    }
    DEPTH.set(DEPTH.get() + 1);
    let _scope = Scope;
    f()
}

/// Whether this thread runs a request.
pub fn in_request() -> bool {
    DEPTH.get() > 0
}

/// The process's global allocator: the arena of the thread's request, or the system's.
#[derive(Debug, Default)]
pub struct ArenaAlloc;

fn region() -> Option<usize> {
    match REGION_STATE.load(Ordering::Acquire) {
        2 => return Some(BASE.load(Ordering::Relaxed)),
        3 => return None,
        _ => {}
    }
    if REGION_STATE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        match crate::os::reserve(REGION) {
            Some(p) => {
                BASE.store(p.as_ptr() as usize, Ordering::Relaxed);
                REGION_STATE.store(2, Ordering::Release);
            }
            None => REGION_STATE.store(3, Ordering::Release),
        }
    }
    // Another thread reserves it: its request uses the system's allocator meanwhile.
    (REGION_STATE.load(Ordering::Acquire) == 2).then(|| BASE.load(Ordering::Relaxed))
}

/// Whether `p` lies in the region.
fn in_region(p: usize) -> bool {
    let base = BASE.load(Ordering::Relaxed);
    base != 0 && p >= base && p < base + REGION
}

/// The count of live blocks of chunk `c`.
fn live(c: usize) -> &'static AtomicU64 {
    // SAFETY: `c` is the start of a chunk of the region, which stays mapped for the life of the
    // process; its first 8 bytes, aligned to 64, are only ever used as this atomic.
    unsafe { &*(c as *const AtomicU64) }
}

/// The link of chunk `c` to the next chunk of its request (an address) or of the pool (an index
/// plus one).
fn link(c: usize) -> &'static AtomicU64 {
    // SAFETY: `c` is the start of a chunk of the region, which stays mapped for the life of the
    // process; bytes 8..16 of its header, aligned to 8, are only ever used as this atomic.
    unsafe { &*((c + 8) as *const AtomicU64) }
}

fn next_of(c: usize) -> usize {
    link(c).load(Ordering::Relaxed) as usize
}

fn set_next(c: usize, next: usize) {
    link(c).store(next as u64, Ordering::Relaxed);
}

const LOW: u64 = 0xFFFF_FFFF;

impl Pool {
    /// Puts chunk `c` of the region at `base` on top.
    fn push(&self, base: usize, c: usize) {
        let index = ((c - base) / CHUNK) as u64 + 1;
        let mut top = self.top.load(Ordering::Acquire);
        loop {
            link(c).store(top & LOW, Ordering::Relaxed);
            let new = (((top >> 32) + 1) << 32 & !LOW) | index;
            match self.top.compare_exchange_weak(top, new, Ordering::Release, Ordering::Acquire) {
                Ok(_) => break,
                Err(t) => top = t,
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Takes the chunk on top. The counter of the top changes with every push and pop, so a pop
    /// that read a top another thread took and put back meanwhile fails and tries again.
    fn pop(&self, base: usize) -> Option<usize> {
        let mut top = self.top.load(Ordering::Acquire);
        loop {
            let index = top & LOW;
            if index == 0 {
                return None;
            }
            let c = base + (index as usize - 1) * CHUNK;
            let next = link(c).load(Ordering::Relaxed) & LOW;
            let new = (((top >> 32) + 1) << 32 & !LOW) | next;
            match self.top.compare_exchange_weak(top, new, Ordering::Acquire, Ordering::Acquire) {
                Ok(_) => {
                    self.count.fetch_sub(1, Ordering::Relaxed);
                    return Some(c);
                }
                Err(t) => top = t,
            }
        }
    }
}

/// An empty chunk: one from the pool or a fresh one of the region.
fn take_chunk() -> Option<usize> {
    let base = region()?;
    let c = match POOL.pop(base) {
        Some(c) => c,
        None => {
            let i = FRESH.fetch_add(1, Ordering::Relaxed);
            if i >= REGION / CHUNK {
                FRESH.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
            base + i * CHUNK
        }
    };
    live(c).store(0, Ordering::Relaxed);
    set_next(c, 0);
    CHUNKS_OUT.fetch_add(1, Ordering::Relaxed);
    Some(c)
}

/// Gives chunk `c`, with no live block, back to the pool; past [`KEEP_WARM`] chunks there, its
/// memory goes to the system.
fn give_back(c: usize) {
    if POOL.count.load(Ordering::Relaxed) >= KEEP_WARM {
        crate::os::discard((c + HEADER) as *mut u8, CHUNK - HEADER);
    }
    POOL.push(BASE.load(Ordering::Relaxed), c);
    CHUNKS_OUT.fetch_sub(1, Ordering::Relaxed);
}

/// Empties the request's arena: each chunk whose blocks were all freed goes back to the pool; the
/// others are retired for their last free to give back.
fn end_request() {
    let mut c = LIST.replace(0);
    CUR.set(0);
    POS.set(0);
    USED.set(0);
    while c != 0 {
        // The link is read before the chunk is retired: from then on another thread may give it
        // back.
        let next = next_of(c);
        if live(c).fetch_or(RETIRED, Ordering::AcqRel) == 0 {
            // No block left: it is this thread's alone.
            give_back(c);
        }
        c = next;
    }
}

/// A block of `l` from the request's arena, or `None` for the system's allocator.
fn carve(l: Layout) -> Option<*mut u8> {
    if DEPTH.get() == 0 || l.size() > LARGEST || l.align() > HEADER {
        return None;
    }
    let size = l.size().max(1);
    if USED.get() + size > REQUEST_CAP {
        return None;
    }
    let mut c = CUR.get();
    let mut at = POS.get().next_multiple_of(l.align());
    if c == 0 || at + size > CHUNK {
        c = take_chunk()?;
        set_next(c, LIST.get());
        LIST.set(c);
        CUR.set(c);
        at = HEADER;
    }
    live(c).fetch_add(1, Ordering::Relaxed);
    POS.set(at + size);
    USED.set(USED.get() + size);
    ARENA.fetch_add(1, Ordering::Relaxed);
    Some((c + at) as *mut u8)
}

/// Frees block `p` of the arena: the last block of a retired chunk gives the chunk back, and
/// says so.
fn free_block(p: usize) -> bool {
    let c = p & !(CHUNK - 1);
    let last = live(c).fetch_sub(1, Ordering::AcqRel) == RETIRED | 1;
    if last {
        give_back(c);
    }
    last
}

// SAFETY: a block of the arena is `l.size()` bytes inside a chunk, aligned to `l.align()`, handed
// out once until it is freed: no two live blocks overlap, and a chunk is only reused once every
// block of it was freed. Everything else is the system's allocator with the caller's arguments.
unsafe impl GlobalAlloc for ArenaAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if let Some(p) = carve(l) {
            return p;
        }
        count_general();
        // SAFETY: the caller's contract for `alloc` is `System::alloc`'s.
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if let Some(p) = carve(l) {
            // SAFETY: `p` is a fresh block of `l.size()` bytes.
            unsafe { p.write_bytes(0, l.size()) };
            return p;
        }
        count_general();
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if in_region(p as usize) {
            free_block(p as usize);
            return;
        }
        // SAFETY: `p` is outside the region, so the system's allocator gave it with `l`.
        unsafe { System.dealloc(p, l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if !in_region(p as usize) {
            count_general();
            // SAFETY: `p` came from the system's allocator with `l`.
            return unsafe { System.realloc(p, l, new_size) };
        }
        // SAFETY: the caller guarantees `new_size` with `l.align()` is a valid layout.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, l.align()) };
        // SAFETY: `new` has a non-zero size, as `realloc`'s contract requires.
        let q = unsafe { self.alloc(new) };
        if !q.is_null() {
            // SAFETY: both blocks are live, distinct and at least this long.
            unsafe { std::ptr::copy_nonoverlapping(p, q, l.size().min(new_size)) };
            free_block(p as usize);
        }
        q
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// The region reserved: a test that runs while another thread reserves it would get the
    /// system's allocator.
    fn ready() {
        while region().is_none() {
            std::hint::spin_loop();
        }
    }

    /// Held by the tests that count the region's chunks, so that they do not count each other's.
    struct Counting;

    static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn counting() -> Counting {
        while COUNTING.swap(true, Ordering::Acquire) {
            std::thread::yield_now();
        }
        Counting
    }

    impl Drop for Counting {
        fn drop(&mut self) {
            COUNTING.store(false, Ordering::Release);
        }
    }

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    #[test]
    fn a_request_carves_its_blocks_one_after_the_other_and_gives_its_chunk_back() {
        ready();
        let a = ArenaAlloc;
        let first = request(|| {
            // SAFETY: the blocks are freed with their layouts below.
            let p = unsafe { a.alloc(layout(100)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            let q = unsafe { a.alloc(layout(200)) };
            assert!(in_region(p as usize) && in_region(q as usize));
            assert_eq!(q as usize, (p as usize + 100).next_multiple_of(8), "one after the other");
            // SAFETY: as above.
            unsafe { a.dealloc(p, layout(100)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(q, layout(200)) };
            p as usize
        });
        assert!(!in_request());
        let c = first & !(CHUNK - 1);
        assert_eq!(live(c).load(Ordering::Relaxed), RETIRED, "emptied and in the pool");
    }

    #[test]
    fn the_pool_gives_back_the_last_chunk_put_and_tells_a_stale_top_apart() {
        ready();
        // A pool of its own over the region, on chunks nobody else uses: the last three.
        let base = region().unwrap();
        let pool = Pool { top: AtomicU64::new(0), count: AtomicUsize::new(0) };
        let [x, y, z] = [3, 2, 1].map(|k| base + REGION - k * CHUNK);
        assert_eq!(pool.pop(base), None);
        pool.push(base, x);
        pool.push(base, y);
        let before = pool.top.load(Ordering::Acquire);
        assert_eq!(pool.pop(base), Some(y), "the last one put");
        // Another thread takes the top and puts back another chunk and the same one: the top
        // read before them is stale, though it names the same chunk.
        pool.push(base, z);
        pool.push(base, y);
        let after = pool.top.load(Ordering::Acquire);
        assert_eq!(after & LOW, before & LOW, "the same chunk on top");
        assert_ne!(after, before, "with another counter");
        assert!(pool.top.compare_exchange(before, 0, Ordering::AcqRel, Ordering::Acquire).is_err());
        assert_eq!((pool.pop(base), pool.pop(base), pool.pop(base)), (Some(y), Some(z), Some(x)));
        assert_eq!((pool.pop(base), pool.count.load(Ordering::Relaxed)), (None, 0));
    }

    #[test]
    fn threads_that_end_keep_no_chunk() {
        ready();
        let _alone = counting();
        // With a thread per connection, each thread that ends must leave its chunks in the pool.
        let fresh = FRESH.load(Ordering::Relaxed);
        for _ in 0..400 {
            std::thread::spawn(|| {
                let a = ArenaAlloc;
                request(|| {
                    // SAFETY: freed at once.
                    let p = unsafe { a.alloc(layout(1000)) };
                    // SAFETY: a block of this test, used and freed with its own layout.
                    unsafe { a.dealloc(p, layout(1000)) };
                });
            })
            .join()
            .unwrap();
        }
        let taken = FRESH.load(Ordering::Relaxed) - fresh;
        assert!(taken < 16, "{taken} fresh chunks for 400 threads, one after the other");
    }

    #[test]
    fn outside_a_request_and_for_large_blocks_the_system_allocates() {
        ready();
        let a = ArenaAlloc;
        // SAFETY: freed below with its layout.
        let p = unsafe { a.alloc(layout(64)) };
        assert!(!in_region(p as usize));
        // SAFETY: a block of this test, used and freed with its own layout.
        unsafe { a.dealloc(p, layout(64)) };
        request(|| {
            // SAFETY: as above.
            let big = unsafe { a.alloc(layout(LARGEST + 1)) };
            assert!(!in_region(big as usize));
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(big, layout(LARGEST + 1)) };
            let wide = Layout::from_size_align(64, 4096).unwrap();
            // SAFETY: a block of this test, used and freed with its own layout.
            let w = unsafe { a.alloc(wide) };
            assert!(!in_region(w as usize) && (w as usize).is_multiple_of(4096));
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(w, wide) };
        });
    }

    #[test]
    fn a_block_that_outlives_its_request_keeps_its_chunk_until_another_thread_frees_it() {
        ready();
        let a = ArenaAlloc;
        let kept = request(|| {
            // SAFETY: freed by the other thread below, with its layout.
            let p = unsafe { a.alloc(layout(32)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { p.write_bytes(9, 32) };
            p as usize
        });
        let c = kept & !(CHUNK - 1);
        assert_eq!(live(c).load(Ordering::Relaxed), RETIRED | 1, "retired with one block");
        // A new request carves elsewhere and leaves the block as it was.
        request(|| {
            // SAFETY: freed at once.
            let q = unsafe { a.alloc(layout(32)) };
            assert_ne!(q as usize & !(CHUNK - 1), c);
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(q, layout(32)) };
        });
        // SAFETY: the block is live and 32 bytes long.
        let bytes = unsafe { std::slice::from_raw_parts(kept as *const u8, 32) };
        assert!(bytes.iter().all(|&b| b == 9));
        let gave = std::thread::spawn(move || free_block(kept)).join().unwrap();
        assert!(gave, "its last free, on another thread, gave the chunk back");
    }

    #[test]
    fn a_reallocation_moves_the_bytes_and_a_zeroed_block_is_zero() {
        ready();
        let a = ArenaAlloc;
        request(|| {
            // SAFETY: each block is freed once with its current layout.
            let p = unsafe { a.alloc(layout(16)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { p.copy_from_nonoverlapping(b"0123456789abcdef".as_ptr(), 16) };
            // SAFETY: a block of this test, used and freed with its own layout.
            let q = unsafe { a.realloc(p, layout(16), 4000) };
            assert!(in_region(q as usize));
            // SAFETY: a block of this test, used and freed with its own layout.
            assert_eq!(unsafe { std::slice::from_raw_parts(q, 16) }, b"0123456789abcdef");
            // Past the largest block it goes to the system's allocator, bytes and all.
            // SAFETY: a block of this test, used and freed with its own layout.
            let r = unsafe { a.realloc(q, layout(4000), LARGEST * 2) };
            assert!(!in_region(r as usize));
            // SAFETY: a block of this test, used and freed with its own layout.
            assert_eq!(unsafe { std::slice::from_raw_parts(r, 16) }, b"0123456789abcdef");
            // SAFETY: a block of this test, used and freed with its own layout.
            let s = unsafe { a.realloc(r, layout(LARGEST * 2), LARGEST * 3) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(s, layout(LARGEST * 3)) };
            // A chunk taken from the free list may hold old bytes: a zeroed block is cleared.
            // SAFETY: a block of this test, used and freed with its own layout.
            let z = unsafe { a.alloc(layout(64)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { z.write_bytes(0xAB, 64) };
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(z, layout(64)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            let z = unsafe { a.alloc_zeroed(layout(64)) };
            // SAFETY: a block of this test, used and freed with its own layout.
            assert!(unsafe { std::slice::from_raw_parts(z, 64) }.iter().all(|&b| b == 0));
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(z, layout(64)) };
        });
    }

    #[test]
    fn a_request_past_its_cap_goes_on_with_the_system_allocator() {
        ready();
        let _alone = counting();
        let a = ArenaAlloc;
        request(|| {
            let mut n = 0;
            let mut blocks = Vec::new();
            while n < REQUEST_CAP {
                // SAFETY: freed below.
                let p = unsafe { a.alloc(layout(LARGEST)) };
                assert!(in_region(p as usize));
                blocks.push(p as usize);
                n += LARGEST;
            }
            // SAFETY: as above.
            let past = unsafe { a.alloc(layout(8)) };
            assert!(!in_region(past as usize));
            // SAFETY: a block of this test, used and freed with its own layout.
            unsafe { a.dealloc(past, layout(8)) };
            for p in blocks {
                // SAFETY: each block once.
                unsafe { a.dealloc(p as *mut u8, layout(LARGEST)) };
            }
        });
    }
}
