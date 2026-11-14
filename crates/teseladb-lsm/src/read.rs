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

use teseladb_keys::Hlc;
use teseladb_keys::zone::Zones;
use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::cache::BlockCache;
use crate::cursor::{Cursor, MemCursor, Pos};
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
    waiting: HashMap<u64, (Arc<Table>, usize)>,
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
}

/// Completed reads after which a block nobody came back for leaves [`Reader::fresh`].
const FRESH_LIFETIME: u64 = 1024;

/// The error of bad bytes in block `block` of `table`.
pub(crate) fn corrupt(table: &Table, block: usize) -> LsmError {
    LsmError::Corrupt { file: table.number, offset: table.handle(block).offset }
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
        let key = (table.number, i);
        let cached = match self.fresh.get(&key) {
            Some((payload, _)) => {
                self.touched.push(key);
                Some(payload.clone())
            }
            None => self.cache.get(table.number, i as u32),
        };
        if let Some(payload) = cached {
            return Block::parse(payload).map(Fetch::Ready).map_err(|_| corrupt(table, i));
        }
        if let Some(t) = self.by_block.get(&(table.number, i)) {
            return Ok(Fetch::Wait(ReadToken(*t)));
        }
        let token = self.next_token;
        self.next_token += 1;
        let (offset, len) = table.pages(i);
        self.queue.push_back(IoOp::ReadAt {
            file: file_id(table.number),
            offset,
            len: len as u32,
            token,
        });
        self.waiting.insert(token, (table.clone(), i));
        self.by_block.insert((table.number, i), token);
        Ok(Fetch::Wait(ReadToken(token)))
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
        let (table, i) = self.waiting.remove(&token).expect("a token of a block read");
        self.by_block.remove(&(table.number, i));
        match data {
            Err(errno) => {
                self.failure.get_or_insert(LsmError::Stopped(errno));
            }
            Ok(bytes) => match table.payload(i, &bytes) {
                Ok(payload) => {
                    self.reads_done += 1;
                    let now = self.reads_done;
                    self.fresh.retain(|_, (_, at)| now - *at < FRESH_LIFETIME);
                    self.fresh.insert((table.number, i), (payload.clone(), now));
                    self.cache.insert(table.number, i as u32, payload);
                }
                Err(_) => {
                    self.failure.get_or_insert(corrupt(&table, i));
                }
            },
        }
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
                let data = (!Arc::ptr_eq(f, t)).then(|| t.clone());
                cursors.push(Cursor::Table { table: f.clone(), data, pos });
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
        let step = self.step(r, prune);
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
        let Some(prune) = prune else { return cursors[at].ready(r) };
        let (head, older) = cursors.split_at_mut(at + 1);
        let asleep = &self.asleep;
        let skipped = &mut self.skipped;
        let mut skip = |table: &Table, i: usize| {
            let gone = skippable(table, i, prune, older, |j| asleep.contains(&Some(at + 1 + j)));
            *skipped += u64::from(gone);
            gone
        };
        head[at].ready_skipping(r, &mut skip)
    }

    fn step<B: Blocks>(&mut self, r: &mut B, prune: Option<&dyn Fn(&dyn Zones) -> bool>) -> Step {
        if let Some(e) = r.failure() {
            return Step::Failed(e.clone());
        }
        if self.ready.is_some() {
            return self.deliver(r);
        }
        if self.finished {
            return Step::Done;
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
                    Err(e) => return Step::Failed(e),
                    Ok(Some(t)) => {
                        waiting.get_or_insert(t);
                    }
                    Ok(None) => {}
                }
            }
            if let Some(t) = waiting {
                return Step::Miss(t);
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
                    Err(e) => return Step::Failed(e),
                    Ok(Some(t)) => return Step::Miss(t),
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
                    Err(e) => return Step::Failed(e),
                    Ok(Some(t)) => return Step::Miss(t),
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
                return match st.and_then(|s| self.emit(s)) {
                    Some(item) => Step::Item(item),
                    None if self.ready.is_some() => self.deliver(r),
                    None => Step::Done,
                };
            };
            if same == Some(false) {
                let st = self.current.take().expect("checked");
                if let Some(item) = self.emit(st) {
                    return Step::Item(item);
                }
                if self.ready.is_some() {
                    return self.deliver(r);
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
            if ts > read {
                cursors[i].seek(entry::internal_key(&st.key, read));
                continue;
            }
            if st.found.is_some() {
                if let Err(e) = cursors[i].skip_versions(&st.key, VERSIONS_STEPPED) {
                    return Step::Failed(e);
                }
                continue;
            }
            let c = &cursors[i];
            let seen = match see(ts, c.kind(), || c.stored(), &mut st.retracted, &mut st.found) {
                Ok(seen) => seen,
                Err(e) => return Step::Failed(e),
            };
            // The scan passes over this version: the ones under it may be in the history file
            // it left closed, which it opens at this key (FR-031).
            if let (Seen::Next, Some(j)) = (seen, self.asleep[i]) {
                self.asleep[i] = None;
                cursors[j].seek(entry::internal_key(&st.key, read));
                self.joining = Some(j);
            }
            if let Err(e) = cursors[i].advance() {
                return Step::Failed(e);
            }
        }
    }
}
