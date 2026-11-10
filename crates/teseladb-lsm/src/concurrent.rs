//! Reads from threads other than the core (004 FR-026, H-013): a connection's thread takes the
//! version the core published and reads it with the same walk the core uses, without the core and
//! without a lock. A block it misses is not queued for the disk thread: the read answers
//! [`SharedRead::Need`] with the file, offset and length, the caller reads those bytes itself (the
//! I/O stays outside this crate, constitution principle II) and gives them back with
//! [`SharedLsm::supply`], which decodes them into the shared block cache.
//!
//! The cache is split in shards, each a [`TryCell`]: a read that finds its shard busy takes it
//! as a miss and never waits.

use std::path::PathBuf;
use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_types::Bytes;

use crate::cache::BlockCache;
use crate::error::LsmError;
use crate::read::{self, Blocks, Fetch, Found, Item, Lookup, ReadToken, Scan, Step, corrupt};
use crate::read_back::BackScan;
use crate::shared::{Published, TryCell};
use crate::sst::block::Block;
use crate::sst::reader::Table;
use crate::sst::{align_up, file_name};
use crate::version::{Version, View};

/// Shards of the shared block cache.
const SHARDS: usize = 64;

/// The handle readers on other threads keep: cheap to clone, `Send` and `Sync`.
#[derive(Clone)]
pub struct SharedLsm {
    inner: Arc<Inner>,
}

struct Inner {
    current: Published<Version>,
    cache: Box<[TryCell<BlockCache>]>,
    dir: PathBuf,
}

impl std::fmt::Debug for SharedLsm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedLsm").field("dir", &self.inner.dir).finish()
    }
}

/// A block a read misses: the bytes of `len` at `offset` of the file at `path`.
#[derive(Clone)]
pub struct BlockNeed {
    /// The file's path, as the core's disk names it.
    pub path: String,
    /// Where the block starts.
    pub offset: u64,
    /// Its length on disk.
    pub len: u64,
    table: Arc<Table>,
    block: usize,
}

impl std::fmt::Debug for BlockNeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockNeed")
            .field("path", &self.path)
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish()
    }
}

/// What a point read from another thread gives.
#[derive(Clone, Debug)]
pub enum SharedRead {
    /// The key's visible version.
    Hit(Found),
    /// No visible version.
    NotFound,
    /// A block to read and [`SharedLsm::supply`]; then the read runs again.
    Need(BlockNeed),
    /// The read failed.
    Failed(LsmError),
}

/// What a step of a scan from another thread gives.
#[derive(Clone, Debug)]
pub enum SharedStep {
    /// The next key's visible version.
    Item(Item),
    /// No more keys.
    Done,
    /// A block to read and [`SharedLsm::supply`]; then the step runs again.
    Need(BlockNeed),
    /// The scan failed.
    Failed(LsmError),
}

/// The view one reader thread keeps between its reads ([`SharedLsm::view_cached`]), with the LSM
/// it came from: another LSM (one opened again) starts its versions again.
#[derive(Default)]
pub struct ViewCache {
    held: Option<(Arc<Inner>, u64, SharedView)>,
}

impl std::fmt::Debug for ViewCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewCache").field("version", &self.held.as_ref().map(|h| h.1)).finish()
    }
}

/// A version the core published, read from any thread.
#[derive(Clone)]
pub struct SharedView {
    version: Arc<Version>,
}

impl std::fmt::Debug for SharedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedView").finish()
    }
}

impl SharedLsm {
    pub(crate) fn new(version: Arc<Version>, cache_bytes: u64, dir: PathBuf) -> SharedLsm {
        let per_shard = (cache_bytes / SHARDS as u64).max(1);
        let cache = (0..SHARDS).map(|_| TryCell::new(BlockCache::new(per_shard))).collect();
        SharedLsm { inner: Arc::new(Inner { current: Published::new(version), cache, dir }) }
    }

    /// The version the core published last.
    pub fn view(&self) -> SharedView {
        SharedView { version: self.inner.current.load() }
    }

    /// The version the core published last, kept in `cache` between reads of one thread: it is
    /// taken again only when the core published another one since (AnotherProduct's thread-local
    /// `SuperVersion`), so a read touches no counter that other readers write. A version published
    /// before the core answered a write is seen by any read that starts after that answer.
    pub fn view_cached<'a>(&self, cache: &'a mut ViewCache) -> &'a SharedView {
        let now = self.inner.current.version();
        let stale = cache
            .held
            .as_ref()
            .is_none_or(|(lsm, v, _)| *v != now || !Arc::ptr_eq(lsm, &self.inner));
        if stale {
            let (v, version) = self.inner.current.load_versioned();
            cache.held = Some((self.inner.clone(), v, SharedView { version }));
        }
        &cache.held.as_ref().expect("just taken").2
    }

    pub(crate) fn publish(&self, version: Arc<Version>) {
        self.inner.current.store(version);
    }

    /// Gives the cache the block `need` names, read from its file as `bytes`.
    pub fn supply(&self, need: &BlockNeed, bytes: &[u8]) -> Result<(), LsmError> {
        let raw = Bytes::copy_from_slice(bytes);
        let payload =
            need.table.payload(need.block, &raw).map_err(|_| corrupt(&need.table, need.block))?;
        let (file, block) = (need.table.number, need.block);
        let shard = self.inner.shard(file, block);
        // A miss already cost a read of the disk; a moment's retry keeps the block for the next
        // reads instead of dropping it when another thread holds the shard.
        loop {
            if shard.try_with(|c| c.insert(file, block as u32, payload.clone())).is_some() {
                return Ok(());
            }
            std::hint::spin_loop();
        }
    }
}

impl Inner {
    fn shard(&self, file: u64, block: usize) -> &TryCell<BlockCache> {
        let h = file.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (block as u64);
        &self.cache[(h as usize) % self.cache.len()]
    }
}

/// The blocks of a read from another thread: the shared cache, or the one block it needs.
struct Source<'a> {
    inner: &'a Inner,
    need: Option<BlockNeed>,
}

impl Blocks for Source<'_> {
    fn block(&mut self, table: &Arc<Table>, i: usize) -> Result<Fetch, LsmError> {
        let cached = self.inner.shard(table.number, i).try_with(|c| c.get(table.number, i as u32));
        if let Some(Some(payload)) = cached {
            return Block::parse(payload).map(Fetch::Ready).map_err(|_| corrupt(table, i));
        }
        if self.need.is_none() {
            let h = table.handle(i);
            self.need = Some(BlockNeed {
                path: self.inner.dir.join(file_name(table.number)).display().to_string(),
                offset: h.offset,
                len: align_up(h.len),
                table: table.clone(),
                block: i,
            });
        }
        Ok(Fetch::Wait(ReadToken(0)))
    }

    fn failure(&self) -> Option<&LsmError> {
        None
    }
}

impl SharedView {
    /// The visible version of `key` at `ts` (FR-005).
    pub fn get(&self, shared: &SharedLsm, key: &[u8], ts: Hlc) -> SharedRead {
        let mut s = Source { inner: &shared.inner, need: None };
        match read::get(&mut s, &self.version, key, ts) {
            Lookup::Hit(f) => SharedRead::Hit(f),
            Lookup::NotFound => SharedRead::NotFound,
            Lookup::Miss(_) => SharedRead::Need(s.need.expect("a miss names its block")),
            Lookup::Failed(e) => SharedRead::Failed(e),
        }
    }

    /// Whether `key` may have a version, without reading a block: `false` is certain (H-042).
    pub fn may_exist(&self, key: &[u8]) -> bool {
        read::may_exist(&self.version, key)
    }

    /// A scan of `[start, end)` at `ts`: one visible version per key, in key order.
    pub fn scan(&self, start: &[u8], end: &[u8], ts: Hlc) -> SharedScan {
        SharedScan { scan: View { version: self.version.clone() }.scan(start, end, ts) }
    }

    /// [`SharedView::scan`] from the last key down (H-003 of the native tests).
    pub fn scan_back(&self, start: &[u8], end: &[u8], ts: Hlc) -> SharedBackScan {
        SharedBackScan { scan: View { version: self.version.clone() }.scan_back(start, end, ts) }
    }
}

/// A backward scan from another thread: call [`SharedBackScan::next`] until
/// [`SharedStep::Done`].
#[derive(Debug)]
pub struct SharedBackScan {
    scan: BackScan,
}

impl SharedBackScan {
    /// The previous key's visible version.
    pub fn next(&mut self, shared: &SharedLsm) -> SharedStep {
        let mut s = Source { inner: &shared.inner, need: None };
        match self.scan.next_with(&mut s) {
            Step::Item(i) => SharedStep::Item(i),
            Step::Done => SharedStep::Done,
            Step::Miss(_) => SharedStep::Need(s.need.expect("a miss names its block")),
            Step::Failed(e) => SharedStep::Failed(e),
        }
    }
}

/// A scan from another thread: call [`SharedScan::next`] until [`SharedStep::Done`].
#[derive(Debug)]
pub struct SharedScan {
    scan: Scan,
}

impl SharedScan {
    /// The next key's visible version.
    pub fn next(&mut self, shared: &SharedLsm) -> SharedStep {
        let mut s = Source { inner: &shared.inner, need: None };
        match self.scan.next_with(&mut s) {
            Step::Item(i) => SharedStep::Item(i),
            Step::Done => SharedStep::Done,
            Step::Miss(_) => SharedStep::Need(s.need.expect("a miss names its block")),
            Step::Failed(e) => SharedStep::Failed(e),
        }
    }
}
