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
use std::rc::Rc;

use teseladb_keys::Hlc;
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
use crate::sst::{align_up, file_id, filter};
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

/// Reads of blocks: the cache and the reads in flight.
#[derive(Debug)]
pub(crate) struct Reader {
    pub(crate) cache: BlockCache,
    queue: VecDeque<IoOp>,
    waiting: HashMap<u64, (Rc<Table>, usize)>,
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

    /// The next read to issue.
    pub(crate) fn pop_op(&mut self) -> Option<IoOp> {
        self.queue.pop_front()
    }

    /// Block `i` of `table`, from the cache or after a read.
    /// Reads check the core's failure before they start, and nothing sets it while they run.
    pub(crate) fn block(&mut self, table: &Rc<Table>, i: usize) -> Result<Fetch, LsmError> {
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
        let h = table.handle(i);
        self.queue.push_back(IoOp::ReadAt {
            file: file_id(table.number),
            offset: h.offset,
            len: align_up(h.len) as u32,
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
fn note_tombstones(hidden: &mut Option<Hlc>, tombstones: &[RangeTombstone], key: &[u8], read: Hlc) {
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
    value: impl FnOnce() -> Bytes,
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Found>,
) -> Seen {
    if retracted.contains(&ts) {
        return Seen::Next;
    }
    if kind == Kind::Retract {
        retracted.push(ts);
        return Seen::Next;
    }
    if best.as_ref().is_none_or(|b| ts > b.ts) {
        *best = Some(Found { ts, kind, value: value() });
    }
    Seen::Answer
}

fn get_in_memtable(
    m: &Memtable,
    key: &[u8],
    target: &[u8],
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Found>,
) {
    let mut at = m.seek(target);
    while let Some((pos, k, slot)) = at {
        let (logical, ts) = entry::split(k);
        if logical != key {
            return;
        }
        if let Seen::Answer = see(ts, slot.kind, || slot.value.clone(), retracted, best) {
            return;
        }
        at = m.next_after(pos, k);
    }
}

/// Walks one file for the key; `Ok(Some(token))` if a block is missing.
fn get_in_table(
    r: &mut Reader,
    t: &Rc<Table>,
    key: &[u8],
    target: &[u8],
    retracted: &mut Vec<Hlc>,
    best: &mut Option<Found>,
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
            if let Seen::Answer = see(ts, c.kind(), || c.value(), retracted, best) {
                return Ok(None);
            }
            c.advance().map_err(|_| corrupt(t, i))?;
        }
        i += 1;
    }
    Ok(None)
}

pub(crate) fn get(r: &mut Reader, v: &Version, key: &[u8], read: Hlc) -> Lookup {
    r.begin();
    let found = get_once(r, v, key, read);
    if !matches!(found, Lookup::Miss(_)) {
        r.finish();
    }
    found
}

pub(crate) fn get_once(r: &mut Reader, v: &Version, key: &[u8], read: Hlc) -> Lookup {
    if let Some(e) = &r.failure {
        return Lookup::Failed(e.clone());
    }
    let target = entry::internal_key(key, read);
    let mut retracted = Vec::new();
    let mut best: Option<Found> = None;
    let mut hidden: Option<Hlc> = None;
    for m in &v.memtables {
        let m = m.borrow();
        note_tombstones(&mut hidden, &m.tombstones, key, read);
        let newer = best.as_ref().is_none_or(|b| m.max_ts.is_some_and(|t| t > b.ts));
        if newer && m.min_ts.is_some_and(|t| t <= read) {
            get_in_memtable(&m, key, &target, &mut retracted, &mut best);
        }
    }
    let hashes = filter::hashes(key);
    let mut waiting = None;
    let mut probes = 0;
    for t in v.tables() {
        if key < t.props.lo.as_slice() || key > t.props.hi.as_slice() {
            continue;
        }
        note_tombstones(&mut hidden, &t.props.tombstones, key, read);
        let newer = best.as_ref().is_none_or(|b| t.props.max_ts > b.ts);
        if !newer || t.props.min_ts > read || !t.may_contain(hashes) {
            continue;
        }
        probes += 1;
        match get_in_table(r, t, key, &target, &mut retracted, &mut best) {
            Err(e) => return Lookup::Failed(e),
            Ok(Some(token)) => {
                waiting.get_or_insert(token);
            }
            Ok(None) => {}
        }
    }
    if let Some(token) = waiting {
        return Lookup::Miss(token);
    }
    r.gets += 1;
    r.probes += probes;
    match best {
        Some(b) if b.kind != Kind::Delete && hidden.is_none_or(|h| b.ts > h) => Lookup::Hit(b),
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
            tombstones: Vec::new(),
            current: None,
            finished: false,
        }
    }
}

/// The key a scan is deciding.
struct KeyState {
    key: Vec<u8>,
    retracted: Vec<Hlc>,
    found: Option<Found>,
}

/// A range read in progress: call [`Scan::next`] until [`Step::Done`].
pub struct Scan {
    version: Rc<Version>,
    start: Vec<u8>,
    end: Vec<u8>,
    read: Hlc,
    cursors: Option<Vec<Cursor>>,
    tombstones: Vec<RangeTombstone>,
    current: Option<KeyState>,
    finished: bool,
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
            let mem = m.borrow();
            self.tombstones.extend(mem.tombstones.iter().filter(|t| t.ts <= self.read).cloned());
            drop(mem);
            let mut c = Cursor::Mem(MemCursor::new(m.clone()));
            c.seek(first.clone());
            cursors.push(c);
        }
        for t in v.tables() {
            if t.props.hi.as_slice() < self.start.as_slice()
                || t.props.lo.as_slice() >= self.end.as_slice()
            {
                continue;
            }
            self.tombstones
                .extend(t.props.tombstones.iter().filter(|x| x.ts <= self.read).cloned());
            if t.props.min_ts > self.read || t.props.versions == 0 {
                continue;
            }
            cursors.push(Cursor::Table { table: t.clone(), pos: Pos::Seek(first.clone()) });
        }
        cursors
    }

    /// The decided key's item, if it shows.
    fn emit(&self, st: KeyState) -> Option<Item> {
        let f = st.found?;
        if f.kind == Kind::Delete
            || self.tombstones.iter().any(|t| t.hides(&st.key, f.ts, self.read))
        {
            return None;
        }
        Some(Item { key: st.key, ts: f.ts, kind: f.kind, value: f.value })
    }

    /// The next key's visible version.
    pub fn next(&mut self, lsm: &mut Lsm) -> Step {
        lsm.reader.begin();
        let step = self.step(lsm);
        if !matches!(step, Step::Miss(_)) {
            lsm.reader.finish();
        }
        step
    }

    fn step(&mut self, lsm: &mut Lsm) -> Step {
        let r = &mut lsm.reader;
        if let Some(e) = &r.failure {
            return Step::Failed(e.clone());
        }
        if self.finished {
            return Step::Done;
        }
        if self.cursors.is_none() {
            let c = self.open();
            self.cursors = Some(c);
        }
        loop {
            let cursors = self.cursors.as_mut().expect("opened");
            let mut waiting = None;
            for c in cursors.iter_mut() {
                match c.ready(r) {
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
            let mut min: Option<usize> = None;
            for (i, c) in cursors.iter().enumerate() {
                if let Some(k) = c.key()
                    && min.is_none_or(|m| {
                        entry::compare(k, cursors[m].key().expect("has a key")).is_lt()
                    })
                {
                    min = Some(i);
                }
            }
            let at = min.and_then(|i| {
                let (logical, ts) = entry::split(cursors[i].key().expect("has a key"));
                (logical < self.end.as_slice()).then(|| (i, logical.to_vec(), ts))
            });
            let Some((i, logical, ts)) = at else {
                self.finished = true;
                let st = self.current.take();
                return match st.and_then(|s| self.emit(s)) {
                    Some(item) => Step::Item(item),
                    None => Step::Done,
                };
            };
            if self.current.as_ref().is_some_and(|s| s.key != logical) {
                let st = self.current.take().expect("checked");
                if let Some(item) = self.emit(st) {
                    return Step::Item(item);
                }
                continue;
            }
            let read = self.read;
            let st = self.current.get_or_insert_with(|| KeyState {
                key: logical.clone(),
                retracted: Vec::new(),
                found: None,
            });
            let cursors = self.cursors.as_mut().expect("opened");
            if ts > read {
                cursors[i].seek(entry::internal_key(&logical, read));
                continue;
            }
            if st.found.is_some() {
                cursors[i].seek(entry::after_all_versions(&logical));
                continue;
            }
            let c = &cursors[i];
            let _ = see(ts, c.kind(), || c.value(), &mut st.retracted, &mut st.found);
            if let Err(e) = cursors[i].advance() {
                return Step::Failed(e);
            }
        }
    }
}
