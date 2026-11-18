//! Reads over a view (FR-005, FR-006, FR-008, FR-013, FR-017).
//!
//! A read walks the sources of its version in order of precedence. Blocks come from the cache; a
//! block that is not there queues its read and the call returns `Miss`: the caller runs the I/O
//! and calls again. A point read starts over (cheap, everything is cached by then); a scan keeps
//! its cursors and the state of the key it was on, and goes on from there.
//!
//! The newest version of a key up to the read timestamp wins, except:
//! - a `Retract` hides the version with its own timestamp in the sources after it (a resolved
//!   intent), and the read goes on to older versions;
//! - a deletion, or a range tombstone with timestamp between the version and the read, means the
//!   key is not there.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_keys::Hlc;
use teseladb_keys::zone::Zones;
use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::cache::BlockCache;
use crate::columnar::{self, ColumnStep};
use crate::cursor::{Choice, Cursor, MemCursor, Pos, Readahead};
use crate::entry::{self, Kind, RangeTombstone};
use crate::error::LsmError;
use crate::lsm::Lsm;
use crate::memtable::Memtable;
use crate::sst::block::{Block, BlockCursor};
use crate::sst::reader::Table;
use crate::sst::value::{self, ValueRef};
use crate::sst::{file_id, filter};
use crate::version::{Version, View};

/// Identifies a read waiting for the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadToken(pub u64);

/// A version found by a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Its timestamp.
    pub ts: Hlc,
    /// [`Kind::Put`] or [`Kind::Intent`].
    pub kind: Kind,
    /// Its value.
    pub value: Bytes,
}

/// The version a read holds for a key so far: its value, or where it is while the read has not
/// read it from its value file (FR-034).
pub(crate) struct Cand {
    ts: Hlc,
    kind: Kind,
    value: Bytes,
    deferred: Option<Box<Deferred>>,
}

/// A value a read finds where it is once the version is decided.
#[derive(Clone)]
pub(crate) enum Deferred {
    /// In a value file (FR-034): the file, the version's internal key and the reference.
    Value { file: Arc<Table>, ikey: Vec<u8>, at: ValueRef },
    /// An old version whose value is a delta against a version elsewhere (FR-035): the history
    /// file, its data file, the block of the version and its internal key.
    Based { history: Arc<Table>, data: Arc<Table>, block: usize, ikey: Vec<u8> },
}

impl Deferred {
    fn id(&self) -> (u64, &[u8]) {
        match self {
            Deferred::Value { file, ikey, .. } => (file.number, ikey),
            Deferred::Based { history, ikey, .. } => (history.number, ikey),
        }
    }
}

impl PartialEq for Deferred {
    fn eq(&self, other: &Deferred) -> bool {
        self.id() == other.id()
    }
}

impl Eq for Deferred {}

impl std::fmt::Debug for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (file, _) = self.id();
        f.debug_struct("Deferred").field("file", &file).finish()
    }
}

/// Most bases elsewhere a value of a history file goes through: one per block its key's
/// versions span, which a key with this many would never fill.
const MOST_BASES: usize = 64;

/// The value of the entry `c` of `table` stands at, block `i`: its bytes, or where they are when
/// it is a reference to a value file (FR-034) or a delta against a version elsewhere, for which
/// `data` is the data file of history file `table` (FR-035).
pub(crate) fn stored(
    table: &Arc<Table>,
    data: Option<&Arc<Table>>,
    i: usize,
    c: &BlockCursor,
) -> Result<(Bytes, Option<Box<Deferred>>), LsmError> {
    if c.pending().is_some() {
        let data = data.ok_or_else(|| corrupt(table, i))?.clone();
        let ikey = c.key().to_vec();
        let based = Deferred::Based { history: table.clone(), data, block: i, ikey };
        return Ok((Bytes::new(), Some(Box::new(based))));
    }
    if !c.is_ref() {
        return Ok((c.value(), None));
    }
    let at = ValueRef::decode(c.value_slice()).map_err(|_| corrupt(table, i))?;
    let file = table.value_file(at.slot).map_err(|_| corrupt(table, i))?.clone();
    Ok((Bytes::new(), Some(Box::new(Deferred::Value { file, ikey: c.key().to_vec(), at }))))
}

/// Reads the value `d` points to: `Ok(Err(token))` while a block is being read.
pub(crate) fn fetch<B: Blocks>(
    r: &mut B,
    d: &Deferred,
) -> Result<Result<Bytes, ReadToken>, LsmError> {
    match d {
        Deferred::Value { file, ikey, at } => {
            let bad = |i| corrupt(file, i);
            let i = value::block_of(file, ikey, at).map_err(|_| bad(0))?;
            match r.block(file, i)? {
                Fetch::Wait(token) => Ok(Err(token)),
                Fetch::Ready(block) => {
                    Ok(Ok(value::value_in(block, ikey, at).map_err(|_| bad(i))?))
                }
            }
        }
        Deferred::Based { history, data, block, ikey } => {
            based(r, (history, data), *block, ikey, MOST_BASES)
        }
    }
}

/// The value of the version `ikey` in block `i` of history file `files.0`, a delta against a
/// version elsewhere or after one in the block: the run of deltas from the entry with the base,
/// on the base found in the history file or in its data file `files.1` (FR-035).
fn based<B: Blocks>(
    r: &mut B,
    files: (&Arc<Table>, &Arc<Table>),
    i: usize,
    ikey: &[u8],
    left: usize,
) -> Result<Result<Bytes, ReadToken>, LsmError> {
    let history = files.0;
    let bad = || corrupt(history, i);
    let block = match r.block(history, i)? {
        Fetch::Wait(token) => return Ok(Err(token)),
        Fetch::Ready(b) => b,
    };
    let at = |key: &[u8]| -> Result<BlockCursor, LsmError> {
        let c = BlockCursor::seek(block.clone(), key, entry::compare).map_err(|_| bad())?;
        match c.valid() && c.key() == key {
            true => Ok(c),
            false => Err(bad()),
        }
    };
    let Some(start) = at(ikey)?.pending().map(<[u8]>::to_vec) else {
        return Ok(Ok(at(ikey)?.value()));
    };
    let mut c = at(&start)?;
    let base_key = c.base().ok_or_else(bad)?;
    let base = match version_of(r, files, &base_key, left.checked_sub(1).ok_or_else(bad)?)? {
        Err(token) => return Ok(Err(token)),
        Ok(v) => v,
    };
    c.resolve(&base).map_err(|_| bad())?;
    while c.key() != ikey {
        c.advance().map_err(|_| bad())?;
        if !c.valid() {
            return Err(bad());
        }
    }
    match c.pending() {
        None => Ok(Ok(c.value())),
        Some(_) => Err(bad()),
    }
}

/// The value of the version `ikey`, a `Put` with its value in line, in history file `files.0` or
/// in its data file `files.1` (FR-035).
fn version_of<B: Blocks>(
    r: &mut B,
    files: (&Arc<Table>, &Arc<Table>),
    ikey: &[u8],
    left: usize,
) -> Result<Result<Bytes, ReadToken>, LsmError> {
    for t in [files.0, files.1] {
        let Some(i) = t.find_block(ikey) else { continue };
        let block = match r.block(t, i)? {
            Fetch::Wait(token) => return Ok(Err(token)),
            Fetch::Ready(b) => b,
        };
        let c = BlockCursor::seek(block, ikey, entry::compare).map_err(|_| corrupt(t, i))?;
        if !c.valid() || c.key() != ikey {
            continue;
        }
        if c.kind() != Kind::Put || c.is_ref() {
            return Err(corrupt(t, i));
        }
        return match c.pending() {
            Some(_) if Arc::ptr_eq(t, files.0) => based(r, files, i, ikey, left),
            Some(_) => Err(corrupt(t, i)),
            None => Ok(Ok(c.value())),
        };
    }
    Err(corrupt(files.0, 0))
}

/// Result of [`View::get`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The key's visible version.
    Hit(Found),
    /// No visible version.
    NotFound,
    /// A block is being read: run the I/O and call again.
    Miss(ReadToken),
    /// The core is corrupt or stopped.
    Failed(LsmError),
}

/// A version returned by a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Logical key.
    pub key: Vec<u8>,
    /// Timestamp.
    pub ts: Hlc,
    /// [`Kind::Put`] or [`Kind::Intent`].
    pub kind: Kind,
    /// Value.
    pub value: Bytes,
}

/// Result of [`Scan::next`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The next key's visible version.
    Item(Item),
    /// A block is being read: run the I/O and call again.
    Miss(ReadToken),
    /// No more keys.
    Done,
    /// The core is corrupt or stopped.
    Failed(LsmError),
}

/// A block, or the read it waits for.
pub(crate) enum Fetch {
    Ready(Block),
    Wait(ReadToken),
}

/// Where a read gets the blocks of the files: the core's [`Reader`], which queues the reads it
/// misses for the disk thread, or a reader on another thread (FR-026), which asks its caller.
pub(crate) trait Blocks {
    /// Block `i` of `table`, or the read it waits for.
    fn block(&mut self, table: &Arc<Table>, i: usize) -> Result<Fetch, LsmError>;
    /// Data block `i`'s column block in `table` (010 FR-033), which it must have: its payload or
    /// the read to wait for; `None` when these reads give no column blocks.
    fn columns(
        &mut self,
        _table: &Arc<Table>,
        _i: usize,
    ) -> Result<Option<Result<Bytes, ReadToken>>, LsmError> {
        Ok(None)
    }
    /// [`Blocks::block`] for a cursor that reads `table` in order (FR-044): a miss reads with
    /// block `i` the blocks after it, up to `ahead` bytes, in one read. Also how many blocks the
    /// read it asked for holds: 0 when it asked for none (a hit, or a block already being read).
    fn block_ahead(
        &mut self,
        table: &Arc<Table>,
        i: usize,
        _ahead: u64,
    ) -> Result<(Fetch, usize), LsmError> {
        self.block(table, i).map(|f| (f, 0))
    }
    /// [`Blocks::columns`] for a cursor that reads `table` in order, as [`Blocks::block_ahead`].
    fn columns_ahead(
        &mut self,
        table: &Arc<Table>,
        i: usize,
        _ahead: u64,
    ) -> Result<(Option<Result<Bytes, ReadToken>>, usize), LsmError> {
        self.columns(table, i).map(|c| (c, 0))
    }
    /// The core's failure, which ends every read.
    fn failure(&self) -> Option<&LsmError>;
    /// Starts a read attempt.
    fn begin(&mut self) {}
    /// Ends a read attempt that missed nothing.
    fn finish(&mut self) {}
    /// Counts a point read that searched `probes` files.
    fn counted(&mut self, _probes: u64) {}
}

impl Blocks for Reader {
    fn block(&mut self, table: &Arc<Table>, i: usize) -> Result<Fetch, LsmError> {
        Reader::block(self, table, i)
    }

    fn columns(
        &mut self,
        table: &Arc<Table>,
        i: usize,
    ) -> Result<Option<Result<Bytes, ReadToken>>, LsmError> {
        Reader::columns(self, table, i).map(Some)
    }

    fn block_ahead(
        &mut self,
        table: &Arc<Table>,
        i: usize,
        ahead: u64,
    ) -> Result<(Fetch, usize), LsmError> {
        let (payload, asked) = self.payload_ahead(table, i, ahead)?;
        let fetch = match payload {
            Ok(p) => Fetch::Ready(Block::parse(p).map_err(|_| corrupt(table, i))?),
            Err(token) => Fetch::Wait(token),
        };
        Ok((fetch, asked))
    }

    fn columns_ahead(
        &mut self,
        table: &Arc<Table>,
        i: usize,
        ahead: u64,
    ) -> Result<(Option<Result<Bytes, ReadToken>>, usize), LsmError> {
        let slot = crate::sst::reader::COLUMN_SLOT + i;
        self.payload_ahead(table, slot, ahead).map(|(p, asked)| (Some(p), asked))
    }

    fn failure(&self) -> Option<&LsmError> {
        self.failure.as_ref()
    }

    fn begin(&mut self) {
        Reader::begin(self);
    }

    fn finish(&mut self) {
        Reader::finish(self);
    }

    fn counted(&mut self, probes: u64) {
        self.gets += 1;
        self.probes += probes;
    }
}

/// Reads of blocks: the cache and the reads in flight.
#[derive(Debug)]
pub(crate) struct Reader {
    pub(crate) cache: BlockCache,
    queue: VecDeque<IoOp>,
    /// Each read in flight: its file, the first slot it reads and how many slots it holds (FR-044).
    waiting: HashMap<u64, (Arc<Table>, usize, usize)>,
    by_block: HashMap<(u64, usize), u64>,
    next_token: u64,
    pub(crate) failure: Option<LsmError>,
    /// Blocks just read, held until the read that asked for them takes them, so that a cache
    /// smaller than one read's blocks cannot evict them before the read comes back.
    fresh: HashMap<(u64, usize), (Bytes, u64)>,
    /// Fresh blocks the current attempt used: they leave `fresh` when it ends without a miss.
    touched: Vec<(u64, usize)>,
    reads_done: u64,
    /// Point reads that finished, and the files they searched.
    pub(crate) gets: u64,
    pub(crate) probes: u64,
    /// The cache the readers on other threads share, when it is large enough to hold the data
    /// blocks: the core keeps its data blocks there too, so that a block both read is cached
    /// once, and `cache` holds the meta bytes of the open files.
    pub(crate) shared: Option<crate::concurrent::SharedLsm>,
}

/// Completed reads after which a block nobody came back for leaves [`Reader::fresh`].
const FRESH_LIFETIME: u64 = 1024;

/// The error of bad bytes in block `block` of `table`.
pub(crate) fn corrupt(table: &Table, block: usize) -> LsmError {
    LsmError::Corrupt { file: table.number, offset: table.slot_handle(block).offset }
}

impl Reader {
    pub(crate) fn new(budget: u64) -> Reader {
        Reader {
            cache: BlockCache::new(budget),
            queue: VecDeque::new(),
            waiting: HashMap::new(),
            by_block: HashMap::new(),
            next_token: 1,
            failure: None,
            fresh: HashMap::new(),
            touched: Vec::new(),
            reads_done: 0,
            gets: 0,
            probes: 0,
            shared: None,
        }
    }

    /// Block `block` of `file` from the cache that holds the data blocks, counting a hit.
    fn cached(&mut self, file: u64, block: u32) -> Option<Bytes> {
        let Some(shared) = &self.shared else {
            return self.cache.get(file, block);
        };
        let found = shared.cached(file, block);
        if found.is_some() {
            self.cache.hits += 1;
        }
        found
    }

    /// Whether block `block` of `file` is cached, without counting a hit.
    fn holds(&self, file: u64, block: u32) -> bool {
        match &self.shared {
            Some(shared) => shared.holds(file, block),
            None => self.cache.contains(file, block),
        }
    }

    /// Caches a data block read from the disk; not one of a retired file, which only reads of
    /// older views still read.
    fn keep(&mut self, file: u64, block: u32, payload: Bytes) {
        match &self.shared {
            Some(_) if self.cache.is_retired(file) => {}
            Some(shared) => shared.keep(file, block, payload),
            None => self.cache.insert(file, block, payload),
        }
    }

    /// One past the last block read asked for so far: what a request that just missed a block
    /// waits for ([`Reader::back`]).
    pub(crate) fn asked(&self) -> u64 {
        self.next_token
    }

    /// Whether the block reads asked before `due` ([`Reader::asked`] then) came back, as many as
    /// that in the order they were asked, or one failed: every request hears of a failure.
    pub(crate) fn back(&self, due: u64) -> bool {
        self.failure.is_some() || self.next_token - self.waiting.len() as u64 >= due
    }

    /// The next read to issue.
    pub(crate) fn pop_op(&mut self) -> Option<IoOp> {
        self.queue.pop_front()
    }

    /// Block `i` of `table`, from the cache or after a read.
    /// Reads check the core's failure before they start, and nothing sets it while they run.
    pub(crate) fn block(&mut self, table: &Arc<Table>, i: usize) -> Result<Fetch, LsmError> {
        match self.payload(table, i)? {
            Ok(payload) => Block::parse(payload).map(Fetch::Ready).map_err(|_| corrupt(table, i)),
            Err(token) => Ok(Fetch::Wait(token)),
        }
    }

    /// Data block `i`'s column block in `table` (010 FR-033), which it must have: its payload,
    /// checked when it was read, or the read to wait for.
    pub(crate) fn columns(
        &mut self,
        table: &Arc<Table>,
        i: usize,
    ) -> Result<Result<Bytes, ReadToken>, LsmError> {
        self.payload(table, crate::sst::reader::COLUMN_SLOT + i)
    }

    /// The payload of the block of slot `i` of `table` (a data block, or a column block), from
    /// the cache or after a read.
    fn payload(
        &mut self,
        table: &Arc<Table>,
        i: usize,
    ) -> Result<Result<Bytes, ReadToken>, LsmError> {
        self.payload_ahead(table, i, 0).map(|(p, _)| p)
    }

    /// [`Reader::payload`] that, on a miss, reads with slot `i` the slots after it, up to `ahead`
    /// bytes in all (FR-044); also how many blocks the read it asked for holds (0: none asked).
    fn payload_ahead(
        &mut self,
        table: &Arc<Table>,
        i: usize,
        ahead: u64,
    ) -> Result<(Result<Bytes, ReadToken>, usize), LsmError> {
        let key = (table.number, i);
        let cached = match self.fresh.get(&key) {
            Some((payload, _)) => {
                self.touched.push(key);
                Some(payload.clone())
            }
            None => self.cached(table.number, i as u32),
        };
        if let Some(payload) = cached {
            return Ok((Ok(payload), 0));
        }
        if let Some(t) = self.by_block.get(&(table.number, i)) {
            return Ok((Err(ReadToken(*t)), 0));
        }
        // A disk read asked for: the one miss of this block until it comes back (FR-013).
        self.cache.note_miss();
        let token = self.next_token;
        self.next_token += 1;
        let (offset, len) = table.pages(i);
        let mut end = offset + len;
        let mut blocks = 1;
        while let Some(stop) = self.along(table, i + blocks, (offset, end), ahead) {
            end = stop;
            blocks += 1;
        }
        self.queue.push_back(IoOp::ReadAt {
            file: file_id(table.number),
            offset,
            len: (end - offset) as u32,
            token,
        });
        self.waiting.insert(token, (table.clone(), i, blocks));
        for s in i..i + blocks {
            self.by_block.insert((table.number, s), token);
        }
        Ok((Err(ReadToken(token)), blocks))
    }

    /// The end of the pages of slot `s` of `table` when the read of `[offset, end)` takes it
    /// along (FR-044): the file has it, it starts in the read's last page or right after it,
    /// nobody holds it or reads it, and the read stays within `ahead` bytes.
    fn along(&self, table: &Table, s: usize, (offset, end): (u64, u64), ahead: u64) -> Option<u64> {
        if !table.has_slot(s) {
            return None;
        }
        let (from, len) = table.pages(s);
        let stop = from + len;
        let held = self.fresh.contains_key(&(table.number, s))
            || self.holds(table.number, s as u32)
            || self.by_block.contains_key(&(table.number, s));
        let follows = from >= offset && from <= end && stop >= end;
        (follows && stop - offset <= ahead && !held).then_some(stop)
    }

    /// Starts a read attempt.
    pub(crate) fn begin(&mut self) {
        self.touched.clear();
    }

    /// Ends a read attempt that missed nothing: the fresh blocks it used are no longer held.
    pub(crate) fn finish(&mut self) {
        for key in self.touched.drain(..) {
            self.fresh.remove(&key);
        }
    }

    /// Takes the result of one of its reads (the tokens of the jobs go elsewhere).
    pub(crate) fn on_read(&mut self, token: u64, data: Result<Bytes, IoErrno>) {
        let (table, i, blocks) = self.waiting.remove(&token).expect("a token of a block read");
        for s in i..i + blocks {
            self.by_block.remove(&(table.number, s));
        }
        let bytes = match data {
            Err(errno) => {
                self.failure.get_or_insert(LsmError::Stopped(errno));
                return;
            }
            Ok(bytes) => bytes,
        };
        self.reads_done += 1;
        let now = self.reads_done;
        self.fresh.retain(|_, (_, at)| now - *at < FRESH_LIFETIME);
        // The read starts at the first page of slot `i`; each slot after it starts where its
        // own first page falls in it (FR-044), and each is checked on its own (FR-014).
        let base = table.pages(i).0;
        for s in i..i + blocks {
            let skip = usize::try_from(table.pages(s).0 - base).unwrap_or(usize::MAX);
            let payload = (skip <= bytes.len())
                .then(|| bytes.slice(skip..))
                .and_then(|read| table.payload(s, &read).ok());
            let Some(payload) = payload else {
                self.failure.get_or_insert(corrupt(&table, s));
                return;
            };
            let payload = own_bytes(payload, &bytes);
            // Only the block the read was asked for is held for its reader; the ones read
            // ahead go to the cache.
            if s == i {
                self.fresh.insert((table.number, s), (payload.clone(), now));
            }
            self.keep(table.number, s as u32, payload);
        }
    }
}

/// `payload` with bytes of its own when it is a part of the bytes of `read` (a block stored
/// without compression): a cache that kept the part would keep the whole read, the pages around
/// the block and the blocks read with it.
pub(crate) fn own_bytes(payload: Bytes, read: &Bytes) -> Bytes {
    if payload.len() < read.len() && read.as_ptr_range().contains(&payload.as_ptr()) {
        Bytes::copy_from_slice(&payload)
    } else {
        payload
    }
}

/// The newest timestamp of a range tombstone over `key` that a read at `read` sees.
fn note_tombstones<'a>(
    hidden: &mut Option<Hlc>,
    tombstones: impl IntoIterator<Item = &'a RangeTombstone>,
    key: &[u8],
    read: Hlc,
) {
    for t in tombstones {
        if t.start.as_slice() <= key && key < t.end.as_slice() && t.ts <= read {
            *hidden = Some(hidden.map_or(t.ts, |h| h.max(t.ts)));
        }
    }
}

/// What a source says about one key: keep walking, or this version is its answer.
enum Seen {
    Next,
    Answer,
}

/// Takes a version of the key in the order the source gives them.
fn see(
    ts: Hlc,
    kind: Kind,
    value: impl FnOnce() -> Result<(Bytes, Option<Box<Deferred>>), LsmError>,
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Cand>,
) -> Result<Seen, LsmError> {
    if retracted.contains(&ts) {
        return Ok(Seen::Next);
    }
    if kind == Kind::Retract {
        retracted.push(ts);
        return Ok(Seen::Next);
    }
    if best.as_ref().is_none_or(|b| ts > b.ts) {
        let (value, deferred) = value()?;
        *best = Some(Cand { ts, kind, value, deferred });
    }
    Ok(Seen::Answer)
}

fn get_in_memtable(
    m: &Memtable,
    key: &[u8],
    target: &[u8],
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Cand>,
) {
    let mut at = m.seek(target);
    while let Some((pos, k, slot)) = at {
        let (logical, ts) = entry::split(k);
        if logical != key {
            return;
        }
        let inline = || Ok((slot.value.clone(), None));
        if let Ok(Seen::Answer) = see(ts, slot.kind, inline, retracted, best) {
            return;
        }
        at = m.next_after(pos, k);
    }
}

/// Walks one file for the key; `Ok(Some(token))` if a block is missing.
fn get_in_table<B: Blocks>(
    r: &mut B,
    (t, data): (&Arc<Table>, Option<&Arc<Table>>),
    key: &[u8],
    target: &[u8],
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Cand>,
    passed: &mut bool,
) -> Result<Option<ReadToken>, LsmError> {
    let Some(mut i) = t.find_block(target) else {
        return Ok(None);
    };
    let mut first = true;
    while i < t.blocks() {
        let block = match r.block(t, i)? {
            Fetch::Ready(b) => b,
            Fetch::Wait(token) => return Ok(Some(token)),
        };
        let mut c = if first {
            BlockCursor::seek(block, target, entry::compare)
        } else {
            BlockCursor::first(block)
        }
        .map_err(|_| corrupt(t, i))?;
        first = false;
        while c.valid() {
            let (logical, ts) = entry::split(c.key());
            if logical != key {
                return Ok(None);
            }
            if let Seen::Answer = see(ts, c.kind(), || stored(t, data, i, &c), retracted, best)? {
                return Ok(None);
            }
            // A version passed over: what the read wants is under it.
            *passed = true;
            c.advance().map_err(|_| corrupt(t, i))?;
        }
        i += 1;
    }
    Ok(None)
}

/// Whether `key` may have a version in `v`, without reading a block (H-042): a memtable
/// holds an entry of it, or a file covering it whose filter does not
/// rule it out. `false` is certain; `true` may be a false positive of a filter, or a version that
/// deletes the key.
pub(crate) fn may_exist(v: &Version, key: &[u8]) -> bool {
    let newest = entry::internal_key(key, Hlc { physical: u64::MAX, logical: u32::MAX });
    let in_memtable = v
        .memtables
        .iter()
        .any(|m| m.seek(&newest).is_some_and(|(_, k, _)| entry::split(k).0 == key));
    if in_memtable {
        return true;
    }
    let hashes = filter::hashes(key);
    v.tables_for(key).any(|t| t.may_contain(hashes))
}

pub(crate) fn get<B: Blocks>(r: &mut B, v: &Version, key: &[u8], read: Hlc) -> Lookup {
    r.begin();
    let found = get_once(r, v, key, read);
    if !matches!(found, Lookup::Miss(_)) {
        r.finish();
    }
    found
}

pub(crate) fn get_once<B: Blocks>(r: &mut B, v: &Version, key: &[u8], read: Hlc) -> Lookup {
    if let Some(e) = r.failure() {
        return Lookup::Failed(e.clone());
    }
    let target = entry::internal_key(key, read);
    let mut retracted = Vec::new();
    let mut best: Option<Cand> = None;
    let mut hidden: Option<Hlc> = None;
    for m in &v.memtables {
        note_tombstones(&mut hidden, m.tombstones(), key, read);
        let newer = best.as_ref().is_none_or(|b| m.may_hold_after(b.ts));
        if newer && m.may_hold_at(read) {
            get_in_memtable(m, key, &target, &mut retracted, &mut best);
        }
    }
    let hashes = filter::hashes(key);
    let mut waiting = None;
    let mut probes = 0;
    for t in v.tables_for(key) {
        note_tombstones(&mut hidden, &t.props.tombstones, key, read);
        let before = best.as_ref().map(|b| b.ts);
        let mut passed = false;
        // The file, then its history file (FR-031): what it holds of a key is older than the
        // version its file has of it. It is read below the file's highest mark, where that
        // version may be above the read, and when the read passed over the file's version (a
        // `Retract` of its mark in a newer source); never when the file gave the key's version.
        let history = t.history.as_ref().map(|h| (h, false));
        for (f, of_level) in std::iter::once((t, true)).chain(history) {
            let gave = best.as_ref().map(|b| b.ts) != before;
            let wanted = of_level || (!gave && (read < t.props.max_ts || passed));
            let newer = best.as_ref().is_none_or(|b| f.props.max_ts > b.ts);
            if !wanted || !newer || f.props.min_ts > read || !f.may_contain(hashes) {
                continue;
            }
            probes += 1;
            let files = (f, (!of_level).then_some(t));
            match get_in_table(r, files, key, &target, &mut retracted, &mut best, &mut passed) {
                Err(e) => return Lookup::Failed(e),
                Ok(Some(token)) => {
                    waiting.get_or_insert(token);
                }
                Ok(None) => {}
            }
        }
    }
    if let Some(token) = waiting {
        return Lookup::Miss(token);
    }
    r.counted(probes);
    match best {
        Some(mut b) if b.kind != Kind::Delete && hidden.is_none_or(|h| b.ts > h) => {
            // A value in a value file is read once the version is decided (FR-034).
            if let Some(d) = b.deferred.take() {
                match fetch(r, &d) {
                    Err(e) => return Lookup::Failed(e),
                    Ok(Err(token)) => return Lookup::Miss(token),
                    Ok(Ok(value)) => b.value = value,
                }
            }
            Lookup::Hit(Found { ts: b.ts, kind: b.kind, value: b.value })
        }
        _ => Lookup::NotFound,
    }
}

impl View {
    /// The visible version of `key` at `ts` (FR-005).
    pub fn get(&self, lsm: &mut Lsm, key: &[u8], ts: Hlc) -> Lookup {
        get(&mut lsm.reader, &self.version, key, ts)
    }

    /// A scan of `[start, end)` at `ts`: one visible version per key, in key order (FR-005).
    pub fn scan(&self, start: &[u8], end: &[u8], ts: Hlc) -> Scan {
        Scan {
            version: self.version.clone(),
            start: start.to_vec(),
            end: end.to_vec(),
            read: ts,
            cursors: None,
            heap: None,
            moved: false,
            asleep: Vec::new(),
            joining: None,
            tombstones: Vec::new(),
            current: None,
            ready: None,
            finished: false,
            skipped: 0,
            want: None,
            entries: 0,
            items: 0,
        }
    }
}

/// Older versions of a decided key a scan steps over in a file one by one before it seeks past
/// them: a key has a few versions in each file, and a seek builds a key and searches the block.
const VERSIONS_STEPPED: usize = 8;

/// Whether cursor `a` goes before cursor `b`: the smaller internal key, and with the same one
/// the source that comes first (the newer one).
fn goes_before(cursors: &[Cursor], a: usize, b: usize) -> bool {
    let key = |i: usize| cursors[i].key().expect("a cursor in the heap is at an entry");
    entry::compare(key(a), key(b)).then(a.cmp(&b)).is_lt()
}

/// Whether a filtered scan passes over data block `i` of `table` without reading it (FR-039):
/// its zones exist and `prune` rules them out, and no source after it in the order of precedence
/// may hold a key of the block. `older` are those sources' cursors, and `asleep(j)` says that the
/// `j`-th of them is a history file the read keeps closed. A newer version elsewhere hides the
/// block's versions anyway; an older one would show through once they are passed over, and could
/// match where the block's newer version, or its deletion, does not.
fn skippable(
    table: &Table,
    i: usize,
    prune: &dyn Fn(&dyn Zones) -> bool,
    older: &[Cursor],
    asleep: impl Fn(usize) -> bool,
) -> bool {
    let Some(zones) = table.zones.as_ref().and_then(|z| z.block(i)) else { return false };
    if prune(&zones) {
        return false;
    }
    let (lo, hi) = table.block_span(i);
    older.iter().enumerate().all(|(j, c)| match c {
        Cursor::Table { table: t, .. } => asleep(j) || !t.may_hold(lo, hi),
        Cursor::Mem(_) => true,
    })
}

/// Puts the cursor at `at` of `heap` in its place below.
fn sift_down(heap: &mut [usize], cursors: &[Cursor], mut at: usize) {
    loop {
        let left = 2 * at + 1;
        if left >= heap.len() {
            return;
        }
        let right = left + 1;
        let first = match right < heap.len() && goes_before(cursors, heap[right], heap[left]) {
            true => right,
            false => left,
        };
        if !goes_before(cursors, heap[first], heap[at]) {
            return;
        }
        heap.swap(first, at);
        at = first;
    }
}

/// The key a scan is deciding.
struct KeyState {
    key: Vec<u8>,
    retracted: Vec<Hlc>,
    found: Option<Cand>,
}

/// A range read in progress: call [`Scan::next`] until [`Step::Done`].
pub struct Scan {
    version: Arc<Version>,
    start: Vec<u8>,
    end: Vec<u8>,
    read: Hlc,
    cursors: Option<Vec<Cursor>>,
    /// The cursors at an entry, as a heap by [`goes_before`]: the next entry of the scan is the
    /// one of its first cursor. `None` until every cursor is at its first entry. A scan of a
    /// collection reads a cursor per file it overlaps, tens of them, and looking at each one
    /// for every entry cost more than reading the entries.
    heap: Option<Vec<usize>>,
    /// The first cursor of the heap moved: it is made ready and put in its place before the
    /// scan looks at the heap again.
    moved: bool,
    /// For each cursor of a file, the cursor of its history file when the scan left it closed
    /// (FR-031): a read at or above the file's highest mark needs it only for a key whose
    /// version in the file it passes over, and opens it then.
    asleep: Vec<Option<usize>>,
    /// A history file's cursor that a key woke: once at its entry it joins the heap.
    joining: Option<usize>,
    tombstones: Vec<RangeTombstone>,
    current: Option<KeyState>,
    /// A decided item whose value is still in its value file (FR-034); boxed, since few scans
    /// hold one and every scan carries the field.
    ready: Option<Box<(Item, Box<Deferred>)>>,
    finished: bool,
    /// Data blocks passed over by their zone maps (FR-039).
    skipped: u64,
    /// The top-level fields whose columns the scan takes in place of whole data blocks (010
    /// FR-033), when it takes any.
    want: Option<Arc<[String]>>,
    /// Entries the scan took from its sources and items it gave: its read amplification, added
    /// to the process's totals ([`scan_totals`]) when it ends. 32 bits each, saturating: a
    /// wider pair made `Scan` too large beside a backward scan in the enums that hold either.
    entries: u32,
    items: u32,
}

/// A scan's entries taken from its sources, the items it gave, and how many scans: the
/// process's totals ([`scan_totals`]) or one scan's ([`Scan::counts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanCounts {
    /// Scans ended (1 for one scan's counts).
    pub scans: u64,
    /// Entries taken from the sources: versions, deletions and intents included.
    pub entries: u64,
    /// Items given.
    pub items: u64,
}

static SCANS: AtomicU64 = AtomicU64::new(0);
static SCAN_ENTRIES: AtomicU64 = AtomicU64::new(0);
static SCAN_ITEMS: AtomicU64 = AtomicU64::new(0);

/// Why a scan took an entry and from where (002 run p44: scans took 4.0 entries for each item).
/// Tallied per thread, so that a scan's steps touch no shared counter and `Scan` does not grow,
/// and added to the process's totals ([`scan_breakdown`]) when a scan ends.
#[derive(Clone, Copy)]
enum Took {
    /// A version above the read's mark, passed over.
    Newer,
    /// A version of a key already decided, passed over.
    Older,
    /// The version that decides its key (shown, deleted, or an intent's).
    Decided,
    /// From a memtable.
    Memtable,
    /// From a file of a level.
    Tables,
    /// From a history file (FR-031).
    History,
    /// A deletion that decided its key: no item.
    Deleted,
    /// A retraction, or a version one retracted, passed over by `see`.
    Retracted,
    /// A history file opened: at the scan's start, or at a key whose version it passed over.
    HistoryOpened,
}

const TOOK_KINDS: usize = 9;

thread_local! {
    static TOOK: std::cell::Cell<[u64; TOOK_KINDS]> = const { std::cell::Cell::new([0; TOOK_KINDS]) };
}

static TOOK_TOTALS: [AtomicU64; TOOK_KINDS] = [const { AtomicU64::new(0) }; TOOK_KINDS];

fn took_one(kind: Took) {
    TOOK.with(|t| {
        let mut a = t.get();
        a[kind as usize] += 1;
        t.set(a);
    });
}

fn took(why: Took, from: Took) {
    TOOK.with(|t| {
        let mut a = t.get();
        a[why as usize] += 1;
        a[from as usize] += 1;
        t.set(a);
    });
}

/// The entries every ended scan took, by why and from where, since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanBreakdown {
    /// Versions above the read's mark, passed over.
    pub newer: u64,
    /// Versions of keys already decided, passed over.
    pub older: u64,
    /// The versions that decided their keys.
    pub decided: u64,
    /// Entries from memtables.
    pub memtable: u64,
    /// Entries from files of the levels.
    pub tables: u64,
    /// Entries from history files.
    pub history: u64,
    /// Deletions that decided their keys.
    pub deleted: u64,
    /// Retractions and the versions they retracted, passed over.
    pub retracted: u64,
    /// History files opened.
    pub history_opened: u64,
}

/// [`ScanBreakdown`] of every scan ended since the process started.
pub fn scan_breakdown() -> ScanBreakdown {
    let n = |k: Took| TOOK_TOTALS[k as usize].load(Ordering::Relaxed);
    ScanBreakdown {
        newer: n(Took::Newer),
        older: n(Took::Older),
        decided: n(Took::Decided),
        memtable: n(Took::Memtable),
        tables: n(Took::Tables),
        history: n(Took::History),
        deleted: n(Took::Deleted),
        retracted: n(Took::Retracted),
        history_opened: n(Took::HistoryOpened),
    }
}

/// The counts of every scan ended since the process started.
pub fn scan_totals() -> ScanCounts {
    ScanCounts {
        scans: SCANS.load(Ordering::Relaxed),
        entries: SCAN_ENTRIES.load(Ordering::Relaxed),
        items: SCAN_ITEMS.load(Ordering::Relaxed),
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        // Once per scan, so that a scan's steps touch no shared counter.
        SCANS.fetch_add(1, Ordering::Relaxed);
        SCAN_ENTRIES.fetch_add(u64::from(self.entries), Ordering::Relaxed);
        SCAN_ITEMS.fetch_add(u64::from(self.items), Ordering::Relaxed);
        let tallied = TOOK.with(|t| t.replace([0; TOOK_KINDS]));
        for (total, n) in TOOK_TOTALS.iter().zip(tallied) {
            if n != 0 {
                total.fetch_add(n, Ordering::Relaxed);
            }
        }
    }
}

impl std::fmt::Debug for Scan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("read", &self.read)
            .finish()
    }
}

impl Scan {
    fn open(&mut self) -> Vec<Cursor> {
        let first = entry::first_version(&self.start);
        let v = &self.version;
        let mut cursors = Vec::new();
        for m in &v.memtables {
            self.tombstones.extend(m.tombstones().filter(|t| t.ts <= self.read).cloned());
            let mut c = Cursor::Mem(MemCursor::new(m.clone()));
            c.seek(first.clone());
            cursors.push(c);
        }
        let mut asleep = vec![None; cursors.len()];
        for t in v.tables() {
            // The file, then its history file (FR-031), right after it in the order of
            // precedence. At or above the file's highest mark the history file stays closed.
            let open = self.read < t.props.max_ts;
            let history = t.history.as_ref().map(|h| (h, open));
            let mut of_level = None;
            for (f, open) in std::iter::once((t, true)).chain(history) {
                if f.props.hi.as_slice() < self.start.as_slice()
                    || f.props.lo.as_slice() >= self.end.as_slice()
                {
                    continue;
                }
                self.tombstones
                    .extend(f.props.tombstones.iter().filter(|x| x.ts <= self.read).cloned());
                if f.props.min_ts > self.read || f.props.versions == 0 {
                    continue;
                }
                let pos = if open { Pos::Seek(first.clone()) } else { Pos::Done };
                if open && !Arc::ptr_eq(f, t) {
                    took_one(Took::HistoryOpened);
                }
                let data = (!Arc::ptr_eq(f, t)).then(|| t.clone());
                cursors.push(Cursor::Table {
                    table: f.clone(),
                    data,
                    pos,
                    ahead: Readahead::default(),
                });
                asleep.push(None);
                let at = cursors.len() - 1;
                if Arc::ptr_eq(f, t) {
                    of_level = Some(at);
                } else if let (false, Some(file)) = (open, of_level) {
                    asleep[file] = Some(at);
                }
            }
        }
        self.asleep = asleep;
        cursors
    }

    /// The decided key's item, if it shows: ready, or waiting for its value (FR-034).
    fn emit(&mut self, st: KeyState) -> Option<Item> {
        let f = st.found?;
        if f.kind == Kind::Delete
            || self.tombstones.iter().any(|t| t.hides(&st.key, f.ts, self.read))
        {
            return None;
        }
        let item = Item { key: st.key, ts: f.ts, kind: f.kind, value: f.value };
        self.items = self.items.saturating_add(1);
        match f.deferred {
            None => Some(item),
            Some(d) => {
                self.ready = Some(Box::new((item, d)));
                None
            }
        }
    }

    /// The item waiting for its value, once the value is read (FR-034).
    fn deliver<B: Blocks>(&mut self, r: &mut B) -> Step {
        let (item, d) = &mut **self.ready.as_mut().expect("an item waits");
        match fetch(r, d) {
            Err(e) => Step::Failed(e),
            Ok(Err(token)) => Step::Miss(token),
            Ok(Ok(value)) => {
                item.value = value;
                Step::Item(self.ready.take().expect("an item waits").0)
            }
        }
    }

    /// The next key's visible version.
    pub fn next(&mut self, lsm: &mut Lsm) -> Step {
        self.next_with(&mut lsm.reader)
    }

    /// [`Scan::next`] passing over the data blocks whose zones `prune` rules out (FR-039):
    /// `prune` says whether a block may hold a version the scan's filter accepts.
    pub fn next_pruned(&mut self, lsm: &mut Lsm, prune: &dyn Fn(&dyn Zones) -> bool) -> Step {
        self.next_pruned_with(&mut lsm.reader, Some(prune))
    }

    /// The scan taking each data block it may as its column block, with the columns of the
    /// top-level fields `want` (010 FR-033): [`Scan::next_columns`] gives those batches in key
    /// order among the other documents.
    pub fn with_columns(mut self, want: Arc<[String]>) -> Scan {
        self.want = Some(want);
        self
    }

    /// The entries this scan took from its sources and the items it gave so far.
    pub fn counts(&self) -> ScanCounts {
        ScanCounts { scans: 1, entries: u64::from(self.entries), items: u64::from(self.items) }
    }

    /// [`Scan::next_pruned`] for a scan [`Scan::with_columns`]: the next document, or the next
    /// batch of them.
    pub fn next_columns(
        &mut self,
        lsm: &mut Lsm,
        prune: Option<&dyn Fn(&dyn Zones) -> bool>,
    ) -> ColumnStep {
        self.next_columns_with(&mut lsm.reader, prune)
    }

    /// [`Scan::next_columns`] with the blocks from `r`.
    pub(crate) fn next_columns_with<B: Blocks>(
        &mut self,
        r: &mut B,
        prune: Option<&dyn Fn(&dyn Zones) -> bool>,
    ) -> ColumnStep {
        r.begin();
        let step = self.step(r, prune);
        if !matches!(step, ColumnStep::Step(Step::Miss(_))) {
            r.finish();
        }
        step
    }

    /// Data blocks the scan passed over by their zone maps so far (FR-039).
    pub fn skipped_blocks(&self) -> u64 {
        self.skipped
    }

    /// [`Scan::next`] with the blocks from `r`.
    pub(crate) fn next_with<B: Blocks>(&mut self, r: &mut B) -> Step {
        self.next_pruned_with(r, None)
    }

    /// [`Scan::next_pruned`] with the blocks from `r`.
    pub(crate) fn next_pruned_with<B: Blocks>(
        &mut self,
        r: &mut B,
        prune: Option<&dyn Fn(&dyn Zones) -> bool>,
    ) -> Step {
        r.begin();
        let step = match self.step(r, prune) {
            ColumnStep::Step(s) => s,
            ColumnStep::Columns(_) => unreachable!("only a scan that asks for columns takes them"),
        };
        if !matches!(step, Step::Miss(_)) {
            r.finish();
        }
        step
    }

    /// Makes cursor `at` ready, passing over the blocks `prune` lets it skip.
    fn ready_at<B: Blocks>(
        &mut self,
        at: usize,
        r: &mut B,
        prune: Option<&dyn Fn(&dyn Zones) -> bool>,
    ) -> Result<Option<ReadToken>, LsmError> {
        let cursors = self.cursors.as_mut().expect("opened");
        if prune.is_none() && self.want.is_none() {
            return cursors[at].ready(r);
        }
        let (newer, rest) = cursors.split_at_mut(at);
        let (this, older) = rest.split_first_mut().expect("a cursor at `at`");
        let asleep = &self.asleep;
        let skipped = &mut self.skipped;
        let want = self.want.as_deref();
        let ask = columnar::Ask { read: self.read, end: &self.end, tombstones: &self.tombstones };
        let mut choose = |table: &Table, i: usize, target: Option<&[u8]>| {
            if let Some(prune) = prune {
                let asleep = |j| asleep.contains(&Some(at + 1 + j));
                if skippable(table, i, prune, older, asleep) {
                    *skipped += 1;
                    return Choice::Skip;
                }
            }
            let others = || newer.iter().chain(older.iter());
            match want.is_some() && columnar::takes(table, i, target, &ask, others()) {
                true => Choice::Columns,
                false => Choice::Read,
            }
        };
        let fits = |payload: &Bytes| want.and_then(|w| columnar::fits(payload, w));
        this.ready_choosing(r, &mut choose, &fits)
    }

    fn step<B: Blocks>(
        &mut self,
        r: &mut B,
        prune: Option<&dyn Fn(&dyn Zones) -> bool>,
    ) -> ColumnStep {
        if let Some(e) = r.failure() {
            return ColumnStep::Step(Step::Failed(e.clone()));
        }
        if self.ready.is_some() {
            return ColumnStep::Step(self.deliver(r));
        }
        if self.finished {
            return ColumnStep::Step(Step::Done);
        }
        if self.cursors.is_none() {
            let c = self.open();
            self.cursors = Some(c);
        }
        if self.heap.is_none() {
            // Every cursor at its first entry; then they are ordered once.
            let mut waiting = None;
            for at in 0..self.cursors.as_ref().expect("opened").len() {
                match self.ready_at(at, r, prune) {
                    Err(e) => return ColumnStep::Step(Step::Failed(e)),
                    Ok(Some(t)) => {
                        waiting.get_or_insert(t);
                    }
                    Ok(None) => {}
                }
            }
            if let Some(t) = waiting {
                return ColumnStep::Step(Step::Miss(t));
            }
            let cursors = self.cursors.as_mut().expect("opened");
            let mut heap: Vec<usize> =
                (0..cursors.len()).filter(|i| cursors[*i].key().is_some()).collect();
            for at in (0..heap.len() / 2).rev() {
                sift_down(&mut heap, cursors, at);
            }
            self.heap = Some(heap);
        }
        loop {
            if self.moved {
                // Only the first cursor moves: ready again, it goes to its place or leaves.
                let top = self.heap.as_ref().expect("ordered")[0];
                match self.ready_at(top, r, prune) {
                    Err(e) => return ColumnStep::Step(Step::Failed(e)),
                    Ok(Some(t)) => return ColumnStep::Step(Step::Miss(t)),
                    Ok(None) => {}
                }
                let cursors = self.cursors.as_mut().expect("opened");
                let heap = self.heap.as_mut().expect("ordered");
                self.moved = false;
                if cursors[top].key().is_none() {
                    heap.swap_remove(0);
                }
                sift_down(heap, cursors, 0);
            }
            if let Some(j) = self.joining {
                // A history file a key woke: at its entry, it takes its place among the others.
                match self.ready_at(j, r, prune) {
                    Err(e) => return ColumnStep::Step(Step::Failed(e)),
                    Ok(Some(t)) => return ColumnStep::Step(Step::Miss(t)),
                    Ok(None) => {}
                }
                let cursors = self.cursors.as_mut().expect("opened");
                self.joining = None;
                if cursors[j].key().is_some() {
                    let heap = self.heap.as_mut().expect("ordered");
                    heap.push(j);
                    for at in (0..heap.len() / 2).rev() {
                        sift_down(heap, cursors, at);
                    }
                }
            }
            // A column block first in the heap: the key being decided goes out before its batch.
            let top = self.heap.as_ref().expect("ordered").first().copied();
            if let Some(i) = top.filter(|&i| self.cursors.as_ref().expect("opened")[i].at_columns())
            {
                if let Some(st) = self.current.take() {
                    if let Some(item) = self.emit(st) {
                        return ColumnStep::Step(Step::Item(item));
                    }
                    if self.ready.is_some() {
                        return ColumnStep::Step(self.deliver(r));
                    }
                }
                let cursors = self.cursors.as_mut().expect("opened");
                let (table, block, payload, docs) =
                    cursors[i].take_columns().expect("a cursor at a column block");
                self.moved = true;
                return ColumnStep::Columns(columnar::batch(&table, block, payload, docs));
            }
            // The first cursor's entry: its number, its mark and whether it is of the key being
            // decided (`None`: no key is).
            let head = self.heap.as_ref().expect("ordered").first().and_then(|&i| {
                let key = self.cursors.as_ref().expect("opened")[i].key().expect("in the heap");
                let (logical, ts) = entry::split(key);
                let same = self.current.as_ref().map(|s| s.key.as_slice() == logical);
                (logical < self.end.as_slice()).then_some((i, ts, same))
            });
            let Some((i, ts, same)) = head else {
                self.finished = true;
                let st = self.current.take();
                return ColumnStep::Step(match st.and_then(|s| self.emit(s)) {
                    Some(item) => Step::Item(item),
                    None if self.ready.is_some() => self.deliver(r),
                    None => Step::Done,
                });
            };
            if same == Some(false) {
                let st = self.current.take().expect("checked");
                if let Some(item) = self.emit(st) {
                    return ColumnStep::Step(Step::Item(item));
                }
                if self.ready.is_some() {
                    return ColumnStep::Step(self.deliver(r));
                }
                continue;
            }
            let read = self.read;
            let cursors = self.cursors.as_mut().expect("opened");
            // The key is copied once, when the scan starts to decide it.
            let st = self.current.get_or_insert_with(|| KeyState {
                key: entry::split(cursors[i].key().expect("in the heap")).0.to_vec(),
                retracted: Vec::new(),
                found: None,
            });
            self.moved = true;
            self.entries = self.entries.saturating_add(1);
            let from = match &cursors[i] {
                Cursor::Mem(_) => Took::Memtable,
                Cursor::Table { data: Some(_), .. } => Took::History,
                Cursor::Table { .. } => Took::Tables,
            };
            if ts > read {
                took(Took::Newer, from);
                cursors[i].seek(entry::internal_key(&st.key, read));
                continue;
            }
            if st.found.is_some() {
                took(Took::Older, from);
                if let Err(e) = cursors[i].skip_versions(&st.key, VERSIONS_STEPPED) {
                    return ColumnStep::Step(Step::Failed(e));
                }
                continue;
            }
            took(Took::Decided, from);
            let c = &cursors[i];
            let kind = c.kind();
            let seen = match see(ts, kind, || c.stored(), &mut st.retracted, &mut st.found) {
                Ok(seen) => seen,
                Err(e) => return ColumnStep::Step(Step::Failed(e)),
            };
            match seen {
                Seen::Next => took_one(Took::Retracted),
                Seen::Answer if kind == Kind::Delete => took_one(Took::Deleted),
                Seen::Answer => {}
            }
            // The scan passes over this version: the ones under it may be in the history file
            // it left closed, which it opens at this key (FR-031).
            if let (Seen::Next, Some(j)) = (seen, self.asleep[i]) {
                self.asleep[i] = None;
                took_one(Took::HistoryOpened);
                cursors[j].seek(entry::internal_key(&st.key, read));
                self.joining = Some(j);
            }
            if let Err(e) = cursors[i].advance() {
                return ColumnStep::Step(Step::Failed(e));
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_block_cut_from_a_larger_read_gets_bytes_of_its_own() {
        let read = Bytes::from_vec((0..=255u8).cycle().take(3 * 4096).collect());
        let part = read.slice(100..4196);
        let owned = own_bytes(part.clone(), &read);
        assert_eq!(owned, part);
        assert!(!read.as_ptr_range().contains(&owned.as_ptr()), "a copy, not a part of the read");
        // The whole read, and bytes from elsewhere (a decompressed block), stay as they are.
        assert_eq!(own_bytes(read.clone(), &read).as_ptr(), read.as_ptr());
        let other = Bytes::from_vec(vec![1; 10]);
        assert_eq!(own_bytes(other.clone(), &read).as_ptr(), other.as_ptr());
    }
}
