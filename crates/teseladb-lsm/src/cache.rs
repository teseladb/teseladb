//! Block cache of a core (FR-013): `teseladb-raftlog`'s CLOCK [`ContentCache`] with a budget,
//! keyed by file and block.
//!
//! Each open file's meta bytes (filter, properties and index) go in pinned, so they count against
//! the budget and are never evicted while the file is open. A block that a reader holds stays
//! valid when the cache evicts it: readers keep a reference to its bytes, which are freed when
//! the last reader drops them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use teseladb_raftlog::cache::ContentCache;
use teseladb_types::Bytes;

use crate::sst::reader::COLUMN_SLOT;

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
    /// How many blocks of each file are cached (its meta bytes included): a file with none is
    /// passed over when it goes.
    files: HashMap<u64, u32>,
    /// Files that left the current version ([`BlockCache::retire`]) whose blocks this cache no
    /// longer takes.
    retired: HashSet<u64>,
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

/// The slots a file's blocks may take: its data blocks, from 0, and their column blocks, from
/// [`COLUMN_SLOT`].
pub(crate) fn file_slots(blocks: usize) -> impl Iterator<Item = u32> {
    let n = u32::try_from(blocks).unwrap_or(u32::MAX);
    (0..n).chain((0..n).map(|b| COLUMN_SLOT as u32 + b))
}

impl BlockCache {
    /// An empty cache of `budget` bytes.
    pub(crate) fn new(budget: u64) -> BlockCache {
        let cache = ContentCache::new(budget);
        BlockCache {
            cache,
            slots: HashMap::new(),
            files: HashMap::new(),
            retired: HashSet::new(),
            hits: 0,
            misses: 0,
            memory: None,
        }
    }

    fn unlist(&mut self, key: (u64, u32)) -> Option<u32> {
        let slot = self.slots.remove(&key)?;
        if let Some(n) = self.files.get_mut(&key.0) {
            *n -= 1;
            if *n == 0 {
                self.files.remove(&key.0);
            }
        }
        Some(slot)
    }

    /// Keeps the blocks it takes from now on in `memory`.
    pub(crate) fn set_memory(&mut self, memory: Arc<dyn BlockMemory>) {
        self.memory = Some(memory);
    }

    /// Bytes in use.
    pub(crate) fn used(&self) -> u64 {
        self.cache.used()
    }

    /// The budget.
    pub(crate) fn budget(&self) -> u64 {
        self.cache.budget()
    }

    /// The payload of block `block` of file `file`, if cached (a hit). A miss is counted by the
    /// reader when it asks the disk for the block ([`BlockCache::note_miss`], FR-013).
    pub(crate) fn get(&mut self, file: u64, block: u32) -> Option<Bytes> {
        let found = self.slots.get(&(file, block)).and_then(|s| self.cache.get(*s));
        if found.is_some() {
            self.hits += 1;
        }
        found
    }

    /// Whether block `block` of file `file` is cached, without counting a hit: what a read ahead
    /// leaves out (FR-044).
    pub(crate) fn contains(&self, file: u64, block: u32) -> bool {
        self.slots.contains_key(&(file, block))
    }

    /// Counts one block read from the disk.
    pub(crate) fn note_miss(&mut self) {
        self.misses += 1;
    }

    fn put(&mut self, key: (u64, u32), data: Bytes, pinned: bool) {
        let data = match &self.memory {
            Some(m) => m.place(data),
            None => data,
        };
        if let Some(old) = self.unlist(key) {
            self.cache.remove(old);
        }
        let (slot, evicted) = self.cache.insert(key, data, pinned);
        for gone in evicted {
            self.unlist(gone);
        }
        self.slots.insert(key, slot);
        *self.files.entry(key.0).or_insert(0) += 1;
    }

    /// Caches the payload of a block, unless its file is retired ([`BlockCache::retire`]).
    pub(crate) fn insert(&mut self, file: u64, block: u32, payload: Bytes) {
        if !self.retired.contains(&file) {
            self.put((file, block), payload, false);
        }
    }

    /// Pins the meta bytes of an open file.
    pub(crate) fn pin_meta(&mut self, file: u64, meta: Bytes) {
        self.retired.remove(&file);
        self.put((file, META), meta, true);
    }

    /// Drops the cached blocks among `slots` of file `file`.
    pub(crate) fn drop_blocks(&mut self, file: u64, slots: impl IntoIterator<Item = u32>) {
        if !self.files.contains_key(&file) {
            return;
        }
        for b in slots {
            if let Some(slot) = self.unlist((file, b)) {
                self.cache.remove(slot);
            }
        }
    }

    /// Drops the blocks of a file of `blocks` data blocks that left the current version, while
    /// views that still hold it keep it open; `refuse` keeps this cache from taking its blocks
    /// again, which a reader of such a view then reads from the disk. Its meta bytes, which the
    /// file holds as long as it is open, stay counted until [`BlockCache::forget`].
    pub(crate) fn retire(&mut self, file: u64, blocks: usize, refuse: bool) {
        self.drop_blocks(file, file_slots(blocks));
        if refuse {
            self.retired.insert(file);
        }
    }

    /// Whether [`BlockCache::retire`] refused file `file`'s blocks.
    pub(crate) fn is_retired(&self, file: u64) -> bool {
        self.retired.contains(&file)
    }

    /// Forgets every block of a file of `blocks` data blocks that is going away: its column
    /// blocks and its meta bytes too.
    pub(crate) fn forget(&mut self, file: u64, blocks: usize) {
        self.retired.remove(&file);
        self.drop_blocks(file, file_slots(blocks).chain([META]));
    }

    /// Bytes of the cached blocks (meta bytes left out) of the files `live` says no.
    pub(crate) fn bytes_outside(&self, live: impl Fn(u64) -> bool) -> u64 {
        self.slots
            .iter()
            .filter(|((file, block), _)| *block != META && !live(*file))
            .map(|(_, s)| self.cache.len_of(*s))
            .sum()
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
        assert!(c.contains(1, 0));
        assert_eq!(c.get(1, 0).unwrap().len(), 3000);
        // Not cached: the lookup counts nothing; the reader counts the disk read it asks for.
        assert!(c.get(1, 1).is_none());
        assert_eq!((c.hits, c.misses), (1, 0));
        c.note_miss();
        assert_eq!((c.hits, c.misses), (1, 1));
        // Two more blocks do not fit with the pinned meta: older blocks go, the meta stays.
        c.insert(1, 1, Bytes::from_vec(vec![2; 3000]));
        c.insert(1, 2, Bytes::from_vec(vec![3; 3000]));
        assert!(c.contains(1, META));
        assert!(c.used() <= 10_000 + 3000);
        let held = c.get(1, 2).unwrap();
        c.insert(1, 3, Bytes::from_vec(vec![4; 3000]));
        assert_eq!(held[0], 3, "a block in use stays valid after eviction");
        // Replacing a block keeps one slot for it.
        c.insert(1, 3, Bytes::from_vec(vec![5; 100]));
        assert_eq!(c.get(1, 3).unwrap()[0], 5);
        c.forget(1, 4);
        assert!(!c.contains(1, META));
        assert!(!c.contains(1, 3));
        assert_eq!(c.used(), 0);
    }

    #[test]
    fn a_forgotten_file_loses_every_block_column_ones_too() {
        // Its column blocks, of slots from 2^31, stayed: forgetting asked for the data blocks
        // and the meta bytes only.
        let column = 1u32 << 31;
        let mut c = BlockCache::new(1 << 20);
        for file in [1, 2] {
            c.pin_meta(file, Bytes::from_vec(vec![0; 10]));
            c.insert(file, 0, Bytes::from_vec(vec![1; 100]));
            c.insert(file, column + 3, Bytes::from_vec(vec![2; 50]));
        }
        assert_eq!(c.bytes_outside(|f| f == 2), 150, "file 1's blocks, without its meta");
        c.forget(1, 4);
        assert!(!c.contains(1, 0) && !c.contains(1, column + 3) && !c.contains(1, META));
        assert_eq!((c.used(), c.files.len()), (160, 1));
        c.forget(2, 4);
        assert_eq!((c.used(), c.files.len()), (0, 0));
    }

    #[test]
    fn a_retired_file_loses_its_blocks_column_ones_too_and_keeps_its_meta() {
        let column = 1u32 << 31;
        let mut c = BlockCache::new(1 << 20);
        for file in [1, 2] {
            c.pin_meta(file, Bytes::from_vec(vec![0; 10]));
            c.insert(file, 0, Bytes::from_vec(vec![1; 100]));
            c.insert(file, column + 3, Bytes::from_vec(vec![2; 50]));
        }
        assert_eq!(c.bytes_outside(|f| f == 2), 150, "file 1's blocks, without its meta");
        c.retire(1, 4, true);
        assert!(!c.contains(1, 0) && !c.contains(1, column + 3) && c.contains(1, META));
        assert_eq!(c.used(), 10 + 10 + 150);
        // A reader of an old view reads the block again: the cache does not take it back.
        c.insert(1, 0, Bytes::from_vec(vec![1; 100]));
        assert!(!c.contains(1, 0));
        // A cache that takes them (the shared one) keeps it until the file goes.
        c.retire(2, 4, false);
        c.insert(2, 0, Bytes::from_vec(vec![1; 100]));
        assert!(c.contains(2, 0));
        c.forget(1, 4);
        c.forget(2, 4);
        assert_eq!((c.used(), c.files.len(), c.retired.len()), (0, 0, 0));
        // A file retired before it had a block in the cache is refused all the same.
        c.retire(5, 1, true);
        c.insert(5, 0, Bytes::from_vec(vec![1; 100]));
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
