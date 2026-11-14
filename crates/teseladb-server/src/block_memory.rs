//! The memory a core's block caches keep their blocks in (012 FR-032): the whole 1 GiB pages
//! that fit in the cache's budget, when the system has that many reserved and free, and a pool
//! of 2 MiB pages for the rest of the budget, or for all of it below 1 GiB. A block that fits in
//! neither pool, or every block without them, stays on the heap where it was read.

use std::sync::Arc;

use teseladb_lsm::BlockMemory;
use teseladb_sys::huge_pool::HugePool;
use teseladb_types::Bytes;
use teseladb_wire::connection::RequestArena;

/// Blocks copied into pools of huge pages, the first with room taking each.
#[derive(Debug)]
pub struct HugeBlocks {
    pools: Vec<HugePool>,
}

impl HugeBlocks {
    /// The memory for a cache of `budget` bytes, or `None` when the system has no huge pages
    /// for it.
    pub fn for_cache(budget: u64) -> Option<Arc<dyn BlockMemory>> {
        let bytes = usize::try_from(budget).ok()?;
        HugeBlocks::over(HugeBlocks::pools_for(bytes))
    }

    /// The pools for a cache of `bytes`: its whole 1 GiB pages, if the system has them, and
    /// 2 MiB pages for what is left.
    pub fn pools_for(bytes: usize) -> Vec<HugePool> {
        let gigantic = HugePool::on_gigantic_pages(bytes);
        let rest = bytes - gigantic.capacity();
        vec![gigantic, HugePool::new(rest)]
    }

    /// The memory over `pools`, if any of them has memory.
    pub fn over(pools: Vec<HugePool>) -> Option<Arc<dyn BlockMemory>> {
        let pools: Vec<HugePool> = pools.into_iter().filter(|p| p.capacity() > 0).collect();
        (!pools.is_empty()).then(|| Arc::new(HugeBlocks { pools }) as Arc<dyn BlockMemory>)
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
        match self.pools.iter().find_map(|p| p.copy(&block)) {
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
    fn a_cache_of_1_gib_or_more_takes_its_whole_pages_of_1_gib_and_2_mib_pages_for_the_rest() {
        use teseladb_sys::aligned::{GIGANTIC_PAGE, HUGE_PAGE};
        // 012 FR-032. What each kind of page gets depends on what the system has reserved:
        // scripts/test-linux.sh --huge-pages reserves 2 MiB pages, and 1 GiB ones where the
        // kernel can make them after boot.
        let has_gigantic = HugePool::on_gigantic_pages(GIGANTIC_PAGE).capacity() > 0;
        let bytes = GIGANTIC_PAGE + 4 * HUGE_PAGE;
        let pools = HugeBlocks::pools_for(bytes);
        let (gigantic, small) = (&pools[0], &pools[1]);
        if has_gigantic {
            assert_eq!(
                (gigantic.capacity(), gigantic.page_bytes()),
                (GIGANTIC_PAGE, GIGANTIC_PAGE)
            );
            assert_eq!(small.capacity() % HUGE_PAGE, 0);
            assert!(small.capacity() == 4 * HUGE_PAGE || small.capacity() == 0);
        } else {
            assert_eq!(gigantic.capacity(), 0);
            assert!(small.capacity() == bytes || small.capacity() == 0);
        }
        // Below 1 GiB there are only 2 MiB pages.
        assert_eq!(HugeBlocks::pools_for(4 * HUGE_PAGE)[0].capacity(), 0);
    }

    #[test]
    fn a_block_goes_to_the_first_pool_with_room() {
        let memory = HugeBlocks::over(vec![
            HugePool::new(0),
            HugePool::on_normal_pages(4 << 10),
            HugePool::on_normal_pages(4 << 10),
        ])
        .expect("pools with memory");
        let block = Bytes::from_vec(vec![5; 4096]);
        let (a, b) = (memory.place(block.clone()), memory.place(block.clone()));
        assert_eq!((&a, &b), (&block, &block));
        assert!(
            a.as_ptr() != block.as_ptr()
                && b.as_ptr() != block.as_ptr()
                && a.as_ptr() != b.as_ptr()
        );
        assert_eq!(memory.place(block.clone()).as_ptr(), block.as_ptr(), "both full");
        assert!(HugeBlocks::over(vec![HugePool::new(0)]).is_none());
    }

    #[test]
    fn blocks_are_copied_into_the_pool_until_it_is_full() {
        let memory = HugeBlocks::over(vec![HugePool::on_normal_pages(8 << 10)]).expect("a pool");
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
