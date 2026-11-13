//! Memory on 2 MiB pages carved into blocks (012 FR-032): the block cache of a core keeps its
//! blocks here when the system has huge pages reserved, so the TLB covers the cache with one
//! entry per 2 MiB. Without them, or once the pool is full, the caller keeps its heap memory.
//!
//! The pool is one `MAP_HUGETLB` mapping. A block takes the smallest power of two from 4 KiB that
//! holds it, carved at an offset that is a multiple of its size; a freed block goes to the list of
//! its size and is the first taken for that size again; blocks of one size are not joined into a
//! larger one, so a pool carved into small blocks gives none of a larger size, and the caller
//! keeps that block on the heap. The mapping goes with the pool and its last block.

use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::aligned::{AlignedBuf, HUGE_PAGE};

/// Smallest block: 4 KiB.
const MIN_SHIFT: u32 = 12;
/// Largest block: one huge page.
const MAX_SHIFT: u32 = 21;
/// Sizes of block.
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;

/// A pool of blocks on huge pages; it holds nothing when the system has none reserved.
#[derive(Clone, Debug)]
pub struct HugePool {
    pool: Option<Arc<Pool>>,
}

/// The memory of a pool.
struct Pool {
    base: NonNull<u8>,
    len: usize,
    /// What the memory is: a huge mapping, or a heap buffer in the tests.
    region: Region,
    state: Mutex<State>,
}

enum Region {
    Huge,
    /// Kept for its memory, which the pool carves.
    Heap(#[expect(dead_code, reason = "held only to keep the memory the pool carves")] AlignedBuf),
}

/// The blocks not handed out.
struct State {
    /// The first byte never handed out.
    bump: usize,
    /// Freed blocks of each size, by offset.
    free: [Vec<usize>; CLASSES],
}

// SAFETY: the pool owns its memory; each byte of it is reached only through the one live
// `HugeBlock` that covers it, and the state that hands blocks out is behind a mutex.
unsafe impl Send for Pool {}
// SAFETY: as above.
unsafe impl Sync for Pool {}

impl Drop for Pool {
    fn drop(&mut self) {
        if matches!(self.region, Region::Huge) {
            crate::os::unmap(self.base, self.len);
        }
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool").field("len", &self.len).finish_non_exhaustive()
    }
}

/// A block of a pool: `len` bytes it hands out, freed for the pool when it is dropped.
pub struct HugeBlock {
    pool: Arc<Pool>,
    at: usize,
    class: usize,
    len: usize,
}

// SAFETY: the block owns its bytes like a `Vec<u8>`; nothing else points to them.
unsafe impl Send for HugeBlock {}
// SAFETY: shared references only read them.
unsafe impl Sync for HugeBlock {}

/// The size class of `len` bytes, if a block holds them.
fn class_of(len: usize) -> Option<usize> {
    let size = len.max(1).next_power_of_two().max(1 << MIN_SHIFT);
    let shift = size.trailing_zeros();
    (shift <= MAX_SHIFT).then(|| (shift - MIN_SHIFT) as usize)
}

fn size_of_class(class: usize) -> usize {
    1 << (MIN_SHIFT as usize + class)
}

impl HugePool {
    /// A pool of `bytes` (rounded up to whole huge pages) on huge pages, when the system has that
    /// many reserved and free; an empty pool otherwise.
    pub fn new(bytes: usize) -> HugePool {
        let len = bytes.next_multiple_of(HUGE_PAGE);
        let enough = len > 0 && crate::os::huge_pages_free() * HUGE_PAGE >= len;
        let pool = enough
            .then(|| crate::os::map_huge(len))
            .flatten()
            .map(|base| Arc::new(Pool::new(base, len, Region::Huge)));
        HugePool { pool }
    }

    /// A pool of `bytes` on normal pages, carved the same way: what a caller that wants the
    /// pool's behaviour without huge pages uses, the tests of the carving among them.
    pub fn on_normal_pages(bytes: usize) -> HugePool {
        let mut buf = AlignedBuf::new(bytes);
        let len = buf.len();
        let base = NonNull::new(buf.as_mut_ptr()).expect("a buffer has an address");
        HugePool { pool: Some(Arc::new(Pool::new(base, len, Region::Heap(buf)))) }
    }

    /// Whether the pool has memory on huge pages.
    pub fn is_huge(&self) -> bool {
        self.pool.as_ref().is_some_and(|p| matches!(p.region, Region::Huge))
    }

    /// Bytes of the pool.
    pub fn capacity(&self) -> usize {
        self.pool.as_ref().map_or(0, |p| p.len)
    }

    /// A block of `len` zeroed or reused bytes, if the pool has room for one.
    pub fn block(&self, len: usize) -> Option<HugeBlock> {
        let pool = self.pool.as_ref()?;
        let class = class_of(len)?;
        let at = pool.take(class)?;
        Some(HugeBlock { pool: Arc::clone(pool), at, class, len })
    }

    /// `data` copied into a block of the pool, if it has room.
    pub fn copy(&self, data: &[u8]) -> Option<HugeBlock> {
        let mut b = self.block(data.len())?;
        b.copy_from_slice(data);
        Some(b)
    }
}

impl Pool {
    fn new(base: NonNull<u8>, len: usize, region: Region) -> Pool {
        let state = State { bump: 0, free: std::array::from_fn(|_| Vec::new()) };
        Pool { base, len, region, state: Mutex::new(state) }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves the lists whole: each change is one push or pop.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The offset of a free block of `class`.
    fn take(&self, class: usize) -> Option<usize> {
        let mut s = self.state();
        if let Some(at) = s.free[class].pop() {
            return Some(at);
        }
        let size = size_of_class(class);
        let at = s.bump.next_multiple_of(size);
        let end = at.checked_add(size).filter(|&e| e <= self.len)?;
        s.bump = end;
        Some(at)
    }

    fn give_back(&self, class: usize, at: usize) {
        self.state().free[class].push(at);
    }
}

impl HugeBlock {
    /// Whether the block is on huge pages.
    pub fn is_huge(&self) -> bool {
        matches!(self.pool.region, Region::Huge)
    }
}

impl Deref for HugeBlock {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `at + size_of_class(class) <= pool.len` (checked in `take`) and `len` fits in
        // the class, so the bytes lie in the pool's memory, which lives while `self.pool` does;
        // no other live block covers them (a block's range goes back to the pool only when its
        // block drops). The bytes were zeroed by the mapping or written by an earlier block.
        unsafe { std::slice::from_raw_parts(self.pool.base.as_ptr().add(self.at), self.len) }
    }
}

impl DerefMut for HugeBlock {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref`, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.pool.base.as_ptr().add(self.at), self.len) }
    }
}

impl AsRef<[u8]> for HugeBlock {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Drop for HugeBlock {
    fn drop(&mut self) {
        self.pool.give_back(self.class, self.at);
    }
}

impl std::fmt::Debug for HugeBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HugeBlock").field("at", &self.at).field("len", &self.len).finish()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn sizes_take_the_smallest_power_of_two_from_4_kib_up_to_a_huge_page() {
        assert_eq!(class_of(0), Some(0));
        assert_eq!(class_of(4096), Some(0));
        assert_eq!(class_of(4097), Some(1));
        assert_eq!(class_of(HUGE_PAGE), Some(CLASSES - 1));
        assert_eq!(class_of(HUGE_PAGE + 1), None);
        assert_eq!(size_of_class(CLASSES - 1), HUGE_PAGE);
    }

    #[test]
    fn blocks_hold_their_bytes_apart_and_a_freed_one_is_taken_again() {
        let pool = HugePool::on_normal_pages(64 << 10);
        assert!(!pool.is_huge());
        assert_eq!(pool.capacity(), 64 << 10);
        let a = pool.copy(&[1; 5000]).expect("room");
        let b = pool.copy(&[2; 100]).expect("room");
        assert!(!a.is_huge());
        assert_eq!((a.len(), b.len()), (5000, 100));
        assert!(a.iter().all(|&x| x == 1) && b.iter().all(|&x| x == 2));
        let (a_at, b_at) = (a.at, b.at);
        assert_eq!(a_at % 8192, 0, "a block sits at a multiple of its size");
        assert!(b_at + 4096 <= a_at || a_at + 8192 <= b_at, "blocks never overlap");
        drop(a);
        let c = pool.block(6000).expect("the freed block");
        assert_eq!(c.at, a_at);
        assert!(format!("{c:?} {pool:?}").contains("HugeBlock"));
    }

    #[test]
    fn a_full_pool_or_a_block_past_a_huge_page_gives_none() {
        let pool = HugePool::on_normal_pages(16 << 10);
        let held: Vec<HugeBlock> = (0..4).map(|_| pool.block(4096).expect("room")).collect();
        assert!(pool.block(1).is_none());
        assert!(pool.block(HUGE_PAGE + 1).is_none());
        drop(held);
        // Freed blocks serve their own size only: four of 4 KiB do not make one of 16 KiB.
        assert!(pool.block(16 << 10).is_none());
        assert!(pool.block(4096).is_some());
    }

    #[test]
    fn a_block_outlives_its_pool_handle_and_crosses_threads() {
        let pool = HugePool::on_normal_pages(8 << 10);
        let b = pool.copy(b"block").expect("room");
        drop(pool);
        let back = std::thread::spawn(move || b.to_vec()).join().expect("the thread");
        assert_eq!(back, b"block");
    }

    #[test]
    fn a_pool_has_huge_pages_or_nothing() {
        let pool = HugePool::new(4 << 20);
        if pool.is_huge() {
            assert_eq!(pool.capacity(), 4 << 20);
            let b = pool.copy(b"on a huge page").expect("room");
            assert!(b.is_huge());
            assert_eq!(&b[..], b"on a huge page");
        } else {
            // Without reserved huge pages the caller keeps its heap memory.
            assert_eq!(pool.capacity(), 0);
            assert!(pool.block(10).is_none());
        }
        assert!(HugePool::new(0).block(1).is_none());
    }
}
