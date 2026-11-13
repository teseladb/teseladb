//! The memory a core's block caches keep their blocks in (012 FR-032): a pool of 2 MiB pages
//! the size of the cache's budget, when the system has that many huge pages reserved and free.
//! A block that does not fit in the pool, or every block without one, stays on the heap where
//! it was read.

use std::sync::Arc;

use teseladb_lsm::BlockMemory;
use teseladb_sys::huge_pool::HugePool;
use teseladb_wire::connection::RequestArena;
use teseladb_types::Bytes;

/// Blocks copied into a pool of huge pages.
#[derive(Debug)]
pub struct HugeBlocks {
    pool: HugePool,
}

impl HugeBlocks {
    /// The memory for a cache of `budget` bytes, or `None` when the system has no huge pages
    /// for it.
    pub fn for_cache(budget: u64) -> Option<Arc<dyn BlockMemory>> {
        let bytes = usize::try_from(budget).ok()?;
        HugeBlocks::over(HugePool::new(bytes))
    }

    /// The memory over `pool`, if it has any.
    pub fn over(pool: HugePool) -> Option<Arc<dyn BlockMemory>> {
        (pool.capacity() > 0).then(|| Arc::new(HugeBlocks { pool }) as Arc<dyn BlockMemory>)
    }
}

/// Request buffers on huge pages (012 FR-027, FR-032): each request a connection reads is copied
/// into a block of the pool, which goes back when the last document of the request does.
#[derive(Debug)]
pub struct HugeRequests {
    pool: HugePool,
}

/// Bytes of the pool of request buffers of a client port.
pub const REQUEST_POOL_BYTES: usize = 64 << 20;

impl HugeRequests {
    /// The request buffers of a port, or `None` when the system has no huge pages for them.
    pub fn for_port() -> Option<Arc<dyn RequestArena>> {
        HugeRequests::over(HugePool::new(REQUEST_POOL_BYTES))
    }

    /// The request buffers over `pool`, if it has any.
    pub fn over(pool: HugePool) -> Option<Arc<dyn RequestArena>> {
        (pool.capacity() > 0).then(|| Arc::new(HugeRequests { pool }) as Arc<dyn RequestArena>)
    }
}

impl RequestArena for HugeRequests {
    fn hold(&self, message: &[u8]) -> Option<Bytes> {
        self.pool.copy(message).map(Bytes::from_owner)
    }
}

impl BlockMemory for HugeBlocks {
    fn place(&self, block: Bytes) -> Bytes {
        match self.pool.copy(&block) {
            Some(b) => Bytes::from_owner(b),
            None => block,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_cache_gets_huge_pages_only_when_the_system_has_them_free() {
        let huge = HugePool::new(4 << 20).is_huge();
        assert_eq!(HugeBlocks::for_cache(4 << 20).is_some(), huge);
        assert!(HugeBlocks::for_cache(0).is_none());
    }

    #[test]
    fn blocks_are_copied_into_the_pool_until_it_is_full() {
        let memory = HugeBlocks::over(HugePool::on_normal_pages(8 << 10)).expect("a pool");
        let block = Bytes::from_vec(vec![3; 5000]);
        let placed = memory.place(block.clone());
        assert_eq!(placed, block);
        assert_ne!(placed.as_ptr(), block.as_ptr(), "a copy in the pool");
        let full = memory.place(block.clone());
        assert_eq!(full.as_ptr(), block.as_ptr(), "the pool is full: the block stays");
        drop(placed);
        assert_ne!(memory.place(block.clone()).as_ptr(), block.as_ptr(), "its room came back");
    }

    #[test]
    fn requests_are_held_in_the_pool_until_it_is_full() {
        assert_eq!(HugeRequests::for_port().is_some(), HugePool::new(REQUEST_POOL_BYTES).is_huge());
        let arena = HugeRequests::over(HugePool::on_normal_pages(4 << 10)).expect("a pool");
        let held = arena.hold(b"a request").expect("room");
        assert_eq!(&held[..], b"a request");
        assert!(arena.hold(b"another").is_none(), "full");
        drop(held);
        assert!(arena.hold(b"another").is_some());
        assert!(format!("{arena:?}").contains("HugeRequests"));
    }
}
