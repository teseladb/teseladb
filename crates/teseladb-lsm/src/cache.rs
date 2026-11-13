//! Block cache of a core (FR-013): `teseladb-raftlog`'s CLOCK [`ContentCache`] with a budget,
//! keyed by file and block.
//!
//! Each open file's meta bytes (filter, properties and index) go in pinned, so they count against
//! the budget and are never evicted while the file is open. A block that a reader holds stays
//! valid when the cache evicts it: readers keep a reference to its bytes, which are freed when
//! the last reader drops them.

use std::collections::HashMap;
use std::sync::Arc;

use teseladb_raftlog::cache::ContentCache;
use teseladb_types::Bytes;

/// Where the cache keeps the bytes of the blocks it takes (012 FR-032): the node places them on
/// pages of 2 MiB when the system has them reserved. Without one, a block stays where it was read.
pub trait BlockMemory: Send + Sync {
    /// `block` in this memory, or `block` itself when there is no room.
    fn place(&self, block: Bytes) -> Bytes;
}

/// Block number of a file's meta bytes.
const META: u32 = u32::MAX;

/// The cache.
pub(crate) struct BlockCache {
    cache: ContentCache<(u64, u32)>,
    slots: HashMap<(u64, u32), u32>,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    memory: Option<Arc<dyn BlockMemory>>,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockCache")
            .field("used", &self.cache.used())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .field("placed", &self.memory.is_some())
            .finish()
    }
}

impl BlockCache {
    /// An empty cache of `budget` bytes.
    pub(crate) fn new(budget: u64) -> BlockCache {
        let cache = ContentCache::new(budget);
        BlockCache { cache, slots: HashMap::new(), hits: 0, misses: 0, memory: None }
    }

    /// Keeps the blocks it takes from now on in `memory`.
    pub(crate) fn set_memory(&mut self, memory: Arc<dyn BlockMemory>) {
        self.memory = Some(memory);
    }

    /// Bytes in use.
    pub(crate) fn used(&self) -> u64 {
        self.cache.used()
    }

    /// The payload of block `block` of file `file`, if cached.
    pub(crate) fn get(&mut self, file: u64, block: u32) -> Option<Bytes> {
        let found = self.slots.get(&(file, block)).and_then(|s| self.cache.get(*s));
        if found.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        found
    }

    fn put(&mut self, key: (u64, u32), data: Bytes, pinned: bool) {
        let data = match &self.memory {
            Some(m) => m.place(data),
            None => data,
        };
        if let Some(old) = self.slots.remove(&key) {
            self.cache.remove(old);
        }
        let (slot, evicted) = self.cache.insert(key, data, pinned);
        for gone in evicted {
            self.slots.remove(&gone);
        }
        self.slots.insert(key, slot);
    }

    /// Caches the payload of a block.
    pub(crate) fn insert(&mut self, file: u64, block: u32, payload: Bytes) {
        self.put((file, block), payload, false);
    }

    /// Pins the meta bytes of an open file.
    pub(crate) fn pin_meta(&mut self, file: u64, meta: Bytes) {
        self.put((file, META), meta, true);
    }

    /// Forgets every block of a file that is going away (`blocks` data blocks).
    pub(crate) fn forget(&mut self, file: u64, blocks: usize) {
        for b in (0..blocks as u32).chain([META]) {
            if let Some(slot) = self.slots.remove(&(file, b)) {
                self.cache.remove(slot);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn evicts_blocks_but_not_meta_and_counts_hits() {
        let mut c = BlockCache::new(10_000);
        c.pin_meta(1, Bytes::from_vec(vec![0; 6000]));
        c.insert(1, 0, Bytes::from_vec(vec![1; 3000]));
        assert!(c.slots.contains_key(&(1, 0)));
        assert_eq!(c.get(1, 0).unwrap().len(), 3000);
        assert!(c.get(1, 1).is_none());
        assert_eq!((c.hits, c.misses), (1, 1));
        // Two more blocks do not fit with the pinned meta: older blocks go, the meta stays.
        c.insert(1, 1, Bytes::from_vec(vec![2; 3000]));
        c.insert(1, 2, Bytes::from_vec(vec![3; 3000]));
        assert!(c.slots.contains_key(&(1, META)));
        assert!(c.used() <= 10_000 + 3000);
        let held = c.get(1, 2).unwrap();
        c.insert(1, 3, Bytes::from_vec(vec![4; 3000]));
        assert_eq!(held[0], 3, "a block in use stays valid after eviction");
        // Replacing a block keeps one slot for it.
        c.insert(1, 3, Bytes::from_vec(vec![5; 100]));
        assert_eq!(c.get(1, 3).unwrap()[0], 5);
        c.forget(1, 4);
        assert!(!c.slots.contains_key(&(1, META)));
        assert!(!c.slots.contains_key(&(1, 3)));
        assert_eq!(c.used(), 0);
    }

    /// A memory that copies each block into a buffer of its own, and counts them.
    struct Copying(std::sync::atomic::AtomicUsize);

    impl BlockMemory for Copying {
        fn place(&self, block: Bytes) -> Bytes {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Bytes::from_owner(block.to_vec())
        }
    }

    #[test]
    fn blocks_and_meta_go_to_the_memory_the_cache_was_given() {
        let memory = Arc::new(Copying(std::sync::atomic::AtomicUsize::new(0)));
        let mut c = BlockCache::new(10_000);
        c.set_memory(memory.clone());
        let block = Bytes::from_vec(vec![7; 100]);
        c.insert(1, 0, block.clone());
        c.pin_meta(1, Bytes::from_vec(vec![1; 10]));
        let cached = c.get(1, 0).unwrap();
        assert_eq!(cached, block);
        assert_ne!(cached.as_ptr(), block.as_ptr(), "the cache holds the placed copy");
        assert_eq!(memory.0.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(format!("{c:?}").contains("placed: true"));
    }
}
