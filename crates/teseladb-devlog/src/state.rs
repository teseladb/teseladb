//! The state of a device log, without threads: what each core's WAL segments hold and where, the
//! flush being built, and the steps of a flush on a [`Disk`]. The node's writer thread drives it,
//! the simulator drives it on its simulated disk, and the tests drive it directly, also on a
//! simulated disk that crashes.
//!
//! Each core's WAL segment lives in the log as a map from its offsets to the places of the log's
//! files that hold them. A file of the log is needed while a map points into it; the oldest files
//! go once nothing points into them, always oldest first, so that a `REMOVE` or a `TRUNCATE` is
//! never replayed without what came before it. A core that writes little keeps its current WAL
//! segment for a long time: when the log has more than [`KEPT_FILES`] files and the oldest holds
//! little that is still needed, a flush copies that part forward and the file goes.
//!
//! The writer works in a pipeline ([`LogState::next`] and [`LogState::on_done`]): while the sync
//! of one group runs, what the cores queue is written in new records, and the group's sync starts
//! as soon as the one before ends, covering them. A file is removed only once the record that let
//! go of its last live byte is durable, since records written behind a sync that ends are not.
//!
//! A file's header is durable before any record goes into it, and its space is written with
//! zeros before its header ([`ZERO_CHUNK`]): a durable write into space that was reserved and
//! never written makes the file system convert the extent and commit that in its journal. On
//! recovery a file without a valid header is the one whose creation a crash cut short, and holds
//! only zeros after its header; one with anything else there was damaged, and the log does not
//! start.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, StorageError};
use teseladb_types::Bytes;

use crate::format::{self, BLOCK, FrameKind, HEAD};

/// Files of the log kept before the oldest one is copied forward.
pub const KEPT_FILES: usize = 4;
/// Bytes waiting that are written without waiting for a sync.
pub const WRITE_AHEAD: usize = 1 << 20;
/// Records written while one sync runs, unless what waits passes [`WRITE_AHEAD`]: each record
/// takes whole blocks.
pub const AHEAD_RECORDS: usize = 4;

/// Bytes of a file between two pieces still held that a copy forward reads along rather than
/// starting another read.
pub const COPY_GAP: u64 = 64 << 10;
/// Most bytes one read of a copy forward takes.
pub const COPY_READ: u64 = 1 << 20;

/// The shortest run of one byte at the end of a write that goes as a `FILL` frame: below it the
/// frame's head costs about what the bytes do.
pub const FILL_MIN: usize = 64;

/// Bytes of a WAL segment held at one place of a file of the log, or a run of one byte that a
/// `FILL` frame of that file put there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    end: u64,
    file: u64,
    /// Where its first byte is in the file.
    pos: u64,
    /// The byte of a `FILL`: the file holds the frame, not the bytes.
    fill: Option<u8>,
}

impl Extent {
    /// Bytes of the file it keeps live, `len` of its bytes.
    fn live(&self, len: u64) -> u64 {
        if self.fill.is_some() { 0 } else { len }
    }
}

/// What the log wrote since it started, for the node's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogStats {
    /// Records written.
    pub records: u64,
    /// Bytes of the records, padding included.
    pub record_bytes: u64,
    /// Zeros after the records, to the next block.
    pub padding_bytes: u64,
    /// Bytes of WAL segments that `FILL` frames stood for.
    pub filled_bytes: u64,
    /// Bytes copied forward from the oldest file.
    pub copied_bytes: u64,
    /// Bytes read from the log to copy them.
    pub copy_read_bytes: u64,
    /// Files made.
    pub files_made: u64,
    /// Files made while a record waited for them.
    pub files_made_waiting: u64,
    /// The cores' syncs a sync of the log answered.
    pub synced_requests: u64,
}

/// The trailing run of one byte of `data` long enough for a `FILL`: where it starts and its byte.
fn trailing_run(data: &[u8]) -> Option<(usize, u8)> {
    let &last = data.last()?;
    let run = data.iter().rev().take_while(|b| **b == last).count();
    (run >= FILL_MIN).then_some((data.len() - run, last))
}

/// A core's WAL segment.
#[derive(Debug, Default)]
struct Wal {
    len: u64,
    extents: BTreeMap<u64, Extent>,
    /// The file holding its newest `CREATE`, if one is still kept.
    created_in: Option<u64>,
}

/// A file of the log.
#[derive(Debug)]
struct LogFile {
    id: FileId,
    size: u64,
    /// Where the next record goes.
    used: u64,
    /// Extents and `CREATE`s pointing into it.
    refs: u64,
    /// Bytes of its extents.
    live: u64,
    /// The flush whose record left it with nothing pointing into it, while that holds.
    freed_at: Option<u64>,
}

/// Bytes of zeros a file being made is written with at a time.
pub const ZERO_CHUNK: u64 = 1 << 20;

/// The writes that fill file `id`, of `size` bytes, with zeros before its header and its records
/// (FR-035). A durable write into space reserved and never written costs the conversion of its
/// extent, which the file system commits in its journal: 280 µs against 54 µs for 4 KiB on the
/// disk of the loads of 002 (19-11-2026).
fn zeros(id: FileId, size: u64) -> impl Iterator<Item = IoOp> {
    let chunk = Bytes::from_vec(vec![0; ZERO_CHUNK.min(size) as usize]);
    (0..size).step_by(ZERO_CHUNK as usize).map(move |offset| {
        let len = ZERO_CHUNK.min(size - offset) as usize;
        IoOp::WriteAt { file: id, offset, data: chunk.slice(..len), batch: 0 }
    })
}

/// A frame of the flush being built, to apply to the maps once its place is known.
#[derive(Debug)]
struct Pending {
    kind: FrameKind,
    core: u16,
    wal: u64,
    arg: u64,
    len: u64,
    data_at: usize,
    fill: Option<u8>,
}

/// The answers of a flush: for each core, the operations now durable (or failed).
pub type Answers = Vec<(u16, IoDone)>;

/// A device log.
#[derive(Debug)]
pub struct LogState {
    file_bytes: u64,
    files: BTreeMap<u64, LogFile>,
    /// The file this run writes to; the files of earlier runs are only read.
    active: Option<u64>,
    wals: BTreeMap<(u16, u64), Wal>,
    next_flush: u64,
    next_file: u64,
    next_id: u32,
    body: Vec<u8>,
    pending: Vec<Pending>,
    /// Files with records written and not synced (two when a flush opened a file).
    dirty: Vec<u64>,
    waiting: Vec<(u16, IoOp)>,
    /// The operations the sync in flight answers.
    syncing: Vec<(u16, IoOp)>,
    /// Syncs of files handed out and not done.
    sync_left: usize,
    /// The last flush the sync in flight covers.
    sync_upto: u64,
    /// The last flush a sync made durable.
    durable: u64,
    /// Records written since the sync in flight started.
    ahead: usize,
    /// The most records written while a sync runs: [`AHEAD_RECORDS`], or none when the files
    /// write through ([`LogState::set_write_through`]).
    ahead_records: usize,
    failed: Option<IoErrno>,
    disk_full: bool,
    syncs: u64,
    /// Extents of the cores' WAL segments looked at to place frames.
    examined: u64,
    /// Whether old files may be copied forward and removed: not while cores recover.
    tidy: bool,
    /// Whether the log's directory may be new: its name in the data directory is synced with the
    /// first file's.
    dir_new: bool,
    /// The HLC bound every record carries from now on, the highest offered (005 FR-003, D-172).
    bound: u64,
    /// The bound each record written and not yet durable carries, by flush, oldest first.
    stamped: std::collections::VecDeque<(u64, u64)>,
    /// The highest bound in a durable record, or found on recovery.
    durable_bound: u64,
    stats: LogStats,
    /// The next file, made ahead of the record that needs it ([`LogState::spare_job`]).
    spare: Option<Spare>,
    /// Files made ahead whose turn passed while they were being made: once made, they hold
    /// nothing and go as any empty file.
    passed: Vec<(u64, FileId)>,
    /// The last file made ahead could not be made: none is tried again until a record made its
    /// own, so that a disk that fails is not asked over and over while nothing writes.
    spare_failed: bool,
    /// The file whose copy forward is being read ([`LogState::copy_job`]).
    copying: Option<u64>,
}

/// The reads that copy forward what the oldest file still holds ([`LogState::copy_job`]).
#[derive(Debug)]
pub struct CopyJob {
    /// The file.
    pub file: u64,
    /// Its reads, each with its place in the list as its token.
    pub reads: Vec<IoOp>,
}

/// A file made ahead, off the writer's path: its header and its name are durable once `ready`.
#[derive(Clone, Copy, Debug)]
struct Spare {
    seq: u64,
    id: FileId,
    ready: bool,
}

/// The operations that make the next file of the log ahead of time, to run in order apart from
/// the writer's; their outcome goes to [`LogState::spare_done`].
#[derive(Debug)]
pub struct SpareJob {
    /// The file's number.
    pub seq: u64,
    /// Its creation, its header's write, its sync and its directory's.
    pub ops: Vec<IoOp>,
}

fn ok(done: IoDone) -> Result<Option<Bytes>, IoErrno> {
    match done {
        IoDone::Ok(_) => Ok(None),
        IoDone::Read { data, .. } => data.map(Some),
        IoDone::Err { errno, .. } => Err(errno),
    }
}

impl LogState {
    /// Reads the log of `disk` (an empty one if it has none). Files of `file_bytes` at least.
    ///
    /// # Errors
    ///
    /// [`StorageError::Corrupt`] for a file whose header was damaged, and [`StorageError::Io`]
    /// with the error of a listing, read or open that failed.
    pub fn recover(disk: &mut dyn Disk, file_bytes: u64) -> Result<LogState, StorageError> {
        let mut s = LogState {
            file_bytes: file_bytes.max(2 * BLOCK),
            files: BTreeMap::new(),
            active: None,
            wals: BTreeMap::new(),
            next_flush: 1,
            next_file: 1,
            next_id: 1,
            body: Vec::new(),
            pending: Vec::new(),
            dirty: Vec::new(),
            waiting: Vec::new(),
            syncing: Vec::new(),
            sync_left: 0,
            sync_upto: 0,
            durable: 0,
            ahead: 0,
            ahead_records: AHEAD_RECORDS,
            failed: None,
            disk_full: false,
            syncs: 0,
            examined: 0,
            tidy: false,
            dir_new: false,
            bound: 0,
            stamped: std::collections::VecDeque::new(),
            durable_bound: 0,
            stats: LogStats::default(),
            spare: None,
            passed: Vec::new(),
            spare_failed: false,
            copying: None,
        };
        let mut paths: Vec<(u64, std::path::PathBuf)> = match disk.list(Path::new(format::DIR)) {
            Ok(v) => {
                v.into_iter().filter_map(|p| format::segment_seq(&p).map(|q| (q, p))).collect()
            }
            Err(IoErrno::NotFound) => Vec::new(),
            Err(errno) => return Err(StorageError::Io { op: "list", errno }),
        };
        paths.sort();
        s.dir_new = paths.is_empty();
        let mut expected: Option<u64> = None;
        for (seq, path) in paths {
            let data =
                disk.read_all(&path).map_err(|errno| StorageError::Io { op: "read", errno })?;
            if !format::header_ok(&data, seq) {
                // Its creation was cut short, or it was damaged.
                if data.iter().skip(BLOCK as usize).any(|b| *b != 0) {
                    return Err(StorageError::Corrupt { file: path, offset: 0 });
                }
            }
            let id = s.new_id();
            let opened = ok(disk.execute(IoOp::Open { file: id, path: path.clone() }));
            opened.map_err(|errno| StorageError::Io { op: "open", errno })?;
            let size = data.len() as u64;
            s.files.insert(seq, LogFile { id, size, used: size, refs: 0, live: 0, freed_at: None });
            s.next_file = seq + 1;
            if !format::header_ok(&data, seq) {
                continue;
            }
            let mut at = BLOCK as usize;
            while let Some(r) = data.get(at..).and_then(format::read_record) {
                if expected.is_some_and(|e| e != r.flush) {
                    break;
                }
                let Some(frames) = format::frames(r.body) else { break };
                let base = (at + HEAD) as u64;
                for f in frames {
                    if f.kind == FrameKind::Bound {
                        s.durable_bound = s.durable_bound.max(f.arg);
                        continue;
                    }
                    let pos = base + f.data_at as u64;
                    let fill = (f.kind == FrameKind::Fill).then_some(f.fill);
                    let place = (seq, pos, u64::from(f.len));
                    s.apply(f.kind, (f.core, f.segment), f.arg, place, fill);
                }
                expected = Some(r.flush + 1);
                at += r.len;
            }
        }
        s.next_flush = expected.unwrap_or(1);
        s.durable = s.next_flush - 1;
        s.bound = s.durable_bound;
        for f in s.files.values_mut().filter(|f| f.refs == 0) {
            f.freed_at = Some(0);
        }
        Ok(s)
    }

    fn new_id(&mut self) -> FileId {
        let id = FileId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Lets the log copy and remove old files: every core has recovered.
    pub fn allow_tidying(&mut self) {
        self.tidy = true;
    }

    /// The WAL segments of `core`, in order.
    #[must_use]
    pub fn segments_of(&self, core: u16) -> Vec<u64> {
        self.wals.range((core, 0)..=(core, u64::MAX)).map(|(k, _)| k.1).collect()
    }

    /// Syncs made since it started.
    #[must_use]
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// Extents of the cores' WAL segments looked at to place frames since it started: those a
    /// frame overlaps, and the one before its start.
    #[must_use]
    pub fn extents_examined(&self) -> u64 {
        self.examined
    }

    /// Files of the log on disk.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files.len()
    }

    /// Whether the last attempt to make a file found the disk full.
    #[must_use]
    pub fn disk_full(&self) -> bool {
        self.disk_full
    }

    /// The error that stopped the log, if one did.
    #[must_use]
    pub fn failed(&self) -> Option<IoErrno> {
        self.failed
    }

    /// Whether an operation waits for the next sync.
    #[must_use]
    pub fn has_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Whether the disk's files write through (`O_DSYNC`): each write ends durable, so a record
    /// written while a sync runs is a durable write of its own that the next group waits for (on
    /// the measuring disk such writes go one after another). Then nothing is written ahead: what
    /// comes during a sync goes in the next group's one record (012 FR-035).
    pub fn set_write_through(&mut self, through: bool) {
        self.ahead_records = if through { 0 } else { AHEAD_RECORDS };
    }

    /// Whether a sync handed out by [`next`](LogState::next) has not ended.
    #[must_use]
    pub fn syncing(&self) -> bool {
        self.sync_left > 0
    }

    /// Bytes waiting for the next flush.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.body.len()
    }

    /// What the log wrote since it started.
    #[must_use]
    pub fn stats(&self) -> LogStats {
        self.stats
    }

    fn frame(&mut self, kind: FrameKind, core: u16, wal: u64, arg: u64, data: &[u8]) {
        let data_at = self.body.len() + format::FRAME_HEAD;
        format::push_frame(&mut self.body, kind, core, wal, arg, data);
        let len = data.len() as u64;
        self.pending.push(Pending { kind, core, wal, arg, len, data_at, fill: None });
    }

    fn fill(&mut self, core: u16, wal: u64, arg: u64, len: u32, byte: u8) {
        let data_at = self.body.len() + format::FRAME_HEAD;
        format::push_fill(&mut self.body, core, wal, arg, len, byte);
        let (kind, len) = (FrameKind::Fill, u64::from(len));
        self.pending.push(Pending { kind, core, wal, arg, len, data_at, fill: Some(byte) });
    }

    /// WAL segment `wal` of `core` exists, with at least `len` bytes.
    pub fn create(&mut self, core: u16, wal: u64, len: u64) {
        self.frame(FrameKind::Create, core, wal, len, &[]);
    }

    /// `data` at `offset` of WAL segment `wal` of `core`. A long run of one byte at its end (the
    /// padding of a WAL batch) goes as a `FILL` frame.
    pub fn write(&mut self, core: u16, wal: u64, offset: u64, data: &[u8]) {
        match trailing_run(data)
            .and_then(|(at, b)| Some((at, b, u32::try_from(data.len() - at).ok()?)))
        {
            Some((at, byte, run)) => {
                if at > 0 {
                    self.frame(FrameKind::Write, core, wal, offset, &data[..at]);
                }
                self.fill(core, wal, offset + at as u64, run, byte);
                self.stats.filled_bytes += u64::from(run);
            }
            None => self.frame(FrameKind::Write, core, wal, offset, data),
        }
    }

    /// WAL segment `wal` of `core` ends at `len`.
    pub fn truncate(&mut self, core: u16, wal: u64, len: u64) {
        self.frame(FrameKind::Truncate, core, wal, len, &[]);
    }

    /// WAL segment `wal` of `core` is gone.
    pub fn remove(&mut self, core: u16, wal: u64) {
        self.frame(FrameKind::Remove, core, wal, 0, &[]);
    }

    /// Answers `op` of `core` once everything asked before it is durable.
    pub fn sync(&mut self, core: u16, op: IoOp) {
        self.waiting.push((core, op));
    }

    fn add_ref(&mut self, file: u64, bytes: u64) {
        if let Some(f) = self.files.get_mut(&file) {
            f.refs += 1;
            f.live += bytes;
        }
    }

    fn drop_ref(&mut self, file: u64, bytes: u64) {
        if let Some(f) = self.files.get_mut(&file) {
            f.refs -= 1;
            f.live -= bytes;
        }
    }

    /// Takes the extents of `key` overlapping `[start, end)` out of its map, keeping the parts
    /// outside the range.
    fn cut(&mut self, key: (u16, u64), start: u64, end: u64) {
        let Some(w) = self.wals.get_mut(&key) else { return };
        // The extents of a segment share no byte (each write cuts what it covers before it goes
        // in), so those that overlap the range are the one that starts before it, if it reaches
        // into it, and those that start inside. Walking the segment from its first extent
        // instead made every write at its end look at all of them.
        let before = w.extents.range(..start).next_back();
        let inside = w.extents.range(start..end);
        let mut examined = 0;
        let hit: Vec<(u64, Extent)> = before
            .into_iter()
            .chain(inside)
            .inspect(|_| examined += 1)
            .filter(|(_, e)| e.end > start)
            .map(|(s, e)| (*s, *e))
            .collect();
        self.examined += examined;
        let mut refs = Vec::new();
        for (s, e) in hit {
            w.extents.remove(&s);
            refs.push((e.file, e.live(e.end - s), false));
            if s < start {
                w.extents.insert(s, Extent { end: start, ..e });
                refs.push((e.file, e.live(start - s), true));
            }
            if e.end > end {
                w.extents.insert(end, Extent { pos: e.pos + (end - s), ..e });
                refs.push((e.file, e.live(e.end - end), true));
            }
        }
        for (file, bytes, add) in refs {
            if add {
                self.add_ref(file, bytes);
            } else {
                self.drop_ref(file, bytes);
            }
        }
    }

    /// Applies a frame found at `(file, pos)` of the log: on recovery, and once a flush is placed.
    fn apply(
        &mut self,
        kind: FrameKind,
        key: (u16, u64),
        arg: u64,
        (file, pos, len): (u64, u64, u64),
        fill: Option<u8>,
    ) {
        match kind {
            FrameKind::Create => {
                let w = self.wals.entry(key).or_default();
                w.len = w.len.max(arg);
                let old = w.created_in.replace(file);
                if let Some(old) = old {
                    self.drop_ref(old, 0);
                }
                self.add_ref(file, 0);
            }
            FrameKind::Write | FrameKind::Fill => {
                self.cut(key, arg, arg + len);
                let w = self.wals.entry(key).or_default();
                w.len = w.len.max(arg + len);
                if len > 0 {
                    let e = Extent { end: arg + len, file, pos, fill };
                    w.extents.insert(arg, e);
                    self.add_ref(file, e.live(len));
                }
            }
            FrameKind::Truncate => {
                self.cut(key, arg, u64::MAX);
                if let Some(w) = self.wals.get_mut(&key) {
                    w.len = arg;
                }
            }
            FrameKind::Remove => {
                self.cut(key, 0, u64::MAX);
                if let Some(w) = self.wals.remove(&key)
                    && let Some(c) = w.created_in
                {
                    self.drop_ref(c, 0);
                }
            }
            // It names no segment: recovery and the writer read it apart.
            FrameKind::Bound => {}
        }
    }

    /// From now on every record carries `bound` as the node's HLC bound, if it is higher than the
    /// one offered before (005 FR-003, D-172). No record is written for it alone.
    pub fn offer_bound(&mut self, bound: u64) {
        self.bound = self.bound.max(bound);
    }

    /// The highest HLC bound in a durable record of the log (or found on recovery), 0 if none.
    pub fn durable_bound(&self) -> u64 {
        self.durable_bound
    }

    /// The contents of WAL segment `wal` of `core`, with the zeros of its reservation.
    ///
    /// # Errors
    ///
    /// `NotFound` if the log has no such segment, or the error of a read or of writing what was
    /// waiting (which it writes first, unsynced, so that it reads the newest bytes).
    pub fn read(&mut self, disk: &mut dyn Disk, core: u16, wal: u64) -> Result<Vec<u8>, IoErrno> {
        if !self.body.is_empty() {
            self.write_out(disk)?;
        }
        let w = self.wals.get(&(core, wal)).ok_or(IoErrno::NotFound)?;
        let len = usize::try_from(w.len).map_err(|_| IoErrno::Io)?;
        let mut out = vec![0u8; len];
        let parts: Vec<(u64, Extent)> = w.extents.iter().map(|(s, e)| (*s, *e)).collect();
        for (s, e) in parts {
            let at = usize::try_from(s).map_err(|_| IoErrno::Io)?;
            let end = usize::try_from(e.end).map_err(|_| IoErrno::Io)?;
            if let Some(b) = e.fill {
                out[at..end].fill(b);
                continue;
            }
            let data = self.read_at(disk, e.file, e.pos, e.end - s)?;
            out[at..at + data.len()].copy_from_slice(&data);
        }
        Ok(out)
    }

    fn read_at(
        &mut self,
        disk: &mut dyn Disk,
        file: u64,
        pos: u64,
        len: u64,
    ) -> Result<Vec<u8>, IoErrno> {
        let id = self.files.get(&file).ok_or(IoErrno::NotFound)?.id;
        let len = u32::try_from(len).map_err(|_| IoErrno::Io)?;
        let data = ok(disk.execute(IoOp::ReadAt { file: id, offset: pos, len, token: 0 }))?
            .unwrap_or_default();
        if data.len() != len as usize {
            return Err(IoErrno::Io);
        }
        Ok(data.to_vec())
    }

    /// The oldest file, if it is time to copy forward what it still holds: once it is at most half
    /// live, or whatever it holds once the log takes more than twice what its files hold live
    /// plus the files it keeps. Files go oldest first, so one more than half live kept every
    /// newer file behind it however empty: a core that held its WAL a while left 6.6 GiB of log
    /// for 2 GiB held in the one-node load.
    fn copy_due(&self) -> Option<u64> {
        let mut old = self.files.keys().copied().filter(|f| Some(*f) != self.active);
        let oldest = old.next()?;
        if !self.tidy || old.count() + 1 < KEPT_FILES {
            return None;
        }
        let f = &self.files[&oldest];
        let due = f.refs > 0 && (f.live * 2 <= self.file_bytes || self.bloated());
        due.then_some(oldest)
    }

    /// Whether the files of the log take more than twice the bytes they hold live, plus
    /// [`KEPT_FILES`] files.
    fn bloated(&self) -> bool {
        let (taken, live) =
            self.files.values().fold((0u64, 0u64), |(t, l), f| (t + f.size, l + f.live));
        taken > 2 * live + KEPT_FILES as u64 * self.file_bytes
    }

    /// The reads that copy forward what the oldest file still holds, once it is time to and no
    /// copy is in flight: few and long (bytes of the file within [`COPY_GAP`] of each other go in
    /// one read of at most [`COPY_READ`] bytes), to run apart from the writer's operations; what
    /// they read goes to [`LogState::copy_done`]. Each read's token is its place in the list.
    pub fn copy_job(&mut self) -> Option<CopyJob> {
        if self.failed.is_some() || self.copying.is_some() {
            return None;
        }
        let file = self.copy_due()?;
        let mut held: Vec<(u64, u64)> = self
            .wals
            .values()
            .flat_map(|w| w.extents.iter())
            .filter(|(_, e)| e.file == file && e.fill.is_none())
            .map(|(s, e)| (e.pos, e.pos + (e.end - s)))
            .collect();
        held.sort_unstable();
        let mut spans: Vec<(u64, u64)> = Vec::new();
        for (start, end) in held {
            match spans.last_mut() {
                Some(last) if start <= last.1 + COPY_GAP && end - last.0 <= COPY_READ => {
                    last.1 = last.1.max(end);
                }
                _ => spans.push((start, end)),
            }
        }
        let id = self.files[&file].id;
        let mut reads = Vec::with_capacity(spans.len());
        for (token, (start, end)) in spans.into_iter().enumerate() {
            let len = u32::try_from(end - start).ok()?;
            reads.push(IoOp::ReadAt { file: id, offset: start, len, token: token as u64 });
        }
        self.copying = Some(file);
        Some(CopyJob { file, reads })
    }

    /// What the reads of the [`CopyJob`] of `file` read (each at its offset), or the first error:
    /// the bytes the file still holds go forward in the next record, ahead of what it holds. A
    /// file that went meanwhile needs nothing; a read that failed stops the log, as any failed
    /// operation of the writer does, and gives the answers of what waited.
    pub fn copy_done(&mut self, file: u64, read: Result<Vec<(u64, Bytes)>, IoErrno>) -> Answers {
        if self.copying != Some(file) {
            return Vec::new();
        }
        self.copying = None;
        if self.failed.is_some() || !self.files.contains_key(&file) {
            return Vec::new();
        }
        match read.and_then(|mut spans| {
            spans.sort_unstable_by_key(|(at, _)| *at);
            self.copy_forward(file, &spans)
        }) {
            Ok(()) => Vec::new(),
            Err(errno) => self.fail(errno),
        }
    }

    /// Puts in front of the flush being built a frame for each `CREATE`, data extent and run that
    /// file `file` still holds, the data taken from `spans` (bytes read at each offset of it).
    fn copy_forward(&mut self, file: u64, spans: &[(u64, Bytes)]) -> Result<(), IoErrno> {
        self.stats.copy_read_bytes += spans.iter().map(|(_, b)| b.len() as u64).sum::<u64>();
        let mut parts = Vec::new();
        let mut creates = Vec::new();
        for (key, w) in &self.wals {
            if w.created_in == Some(file) {
                creates.push((*key, w.len));
            }
            for (s, e) in &w.extents {
                if e.file == file {
                    parts.push((*key, *s, *e));
                }
            }
        }
        // The copies go before what the flush already holds, which is newer than them.
        let body = std::mem::take(&mut self.body);
        let pending = std::mem::take(&mut self.pending);
        for ((core, wal), len) in creates {
            self.create(core, wal, len);
        }
        for ((core, wal), s, e) in parts {
            let len = e.end - s;
            if let Some(b) = e.fill {
                self.fill(core, wal, s, u32::try_from(len).map_err(|_| IoErrno::Io)?, b);
                continue;
            }
            // Every extent of an old file lies within one extent it had when the reads were
            // made: extents only shrink and split.
            let at = spans.partition_point(|(p, _)| *p <= e.pos);
            let (p, data) = at.checked_sub(1).map(|i| &spans[i]).ok_or(IoErrno::Io)?;
            let from = usize::try_from(e.pos - p).map_err(|_| IoErrno::Io)?;
            let to = from + usize::try_from(len).map_err(|_| IoErrno::Io)?;
            let bytes = data.get(from..to).ok_or(IoErrno::Io)?.to_vec();
            self.stats.copied_bytes += len;
            self.frame(FrameKind::Write, core, wal, s, &bytes);
        }
        let shift = self.body.len();
        self.body.extend_from_slice(&body);
        self.pending
            .extend(pending.into_iter().map(|p| Pending { data_at: p.data_at + shift, ..p }));
        Ok(())
    }

    /// Runs the [`CopyJob`] due, if one is, on `disk` now: for the callers that do not run it
    /// apart.
    fn copy_now(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        let Some(job) = self.copy_job() else { return Ok(()) };
        self.copying = None;
        let mut spans = Vec::with_capacity(job.reads.len());
        for op in job.reads {
            let IoOp::ReadAt { offset, len, .. } = op else { continue };
            match ok(disk.execute(op))? {
                Some(data) if data.len() == len as usize => spans.push((offset, data)),
                _ => return Err(IoErrno::Io),
            }
        }
        self.copy_forward(job.file, &spans)
    }

    /// Makes file `seq` for a record of `need` bytes, with its header and its name durable before
    /// any record goes into it: a file without a valid header then holds nothing.
    fn new_file(&mut self, disk: &mut dyn Disk, need: u64) -> Result<u64, IoErrno> {
        // A file still being made ahead comes before this one: it holds nothing.
        if let Some(sp) = self.spare.take() {
            self.pass(sp);
        }
        let seq = self.next_file;
        let size = self.file_bytes.max(BLOCK + need);
        let id = self.new_id();
        let path = format::segment_path(seq);
        // A full disk leaves the record waiting for the next try; nothing was made.
        let made = ok(disk.execute(IoOp::Create { file: id, path, prealloc: size }));
        self.disk_full = matches!(made, Err(IoErrno::NoSpace));
        made?;
        self.stats.files_made += 1;
        self.stats.files_made_waiting += 1;
        self.spare_failed = false;
        self.next_file += 1;
        let file = LogFile { id, size, used: BLOCK, refs: 0, live: 0, freed_at: None };
        self.files.insert(seq, file);
        self.active = Some(seq);
        for op in zeros(id, size) {
            ok(disk.execute(op))?;
        }
        let header = Bytes::from_vec(format::segment_header(seq));
        ok(disk.execute(IoOp::WriteAt { file: id, offset: 0, data: header, batch: 0 }))?;
        ok(disk.execute(IoOp::Sync { file: id, batch: 0 }))?;
        ok(disk.execute(IoOp::SyncDir { path: format::DIR.into() }))?;
        if std::mem::take(&mut self.dir_new) {
            ok(disk.execute(IoOp::SyncDir { path: "".into() }))?;
        }
        Ok(seq)
    }

    /// The operations that make the next file ahead, once the active one is half full, or there
    /// is none yet (a log that just started), and no file is being made ahead: the record that
    /// opens it then neither creates a file, nor writes its space, nor syncs the log's directory
    /// while the cores wait. `None` when it is not time to, and after a job that failed until a
    /// record made its own file.
    pub fn spare_job(&mut self) -> Option<SpareJob> {
        if self.failed.is_some() || self.spare.is_some() || self.disk_full || self.spare_failed {
            return None;
        }
        if self.active.is_some_and(|a| self.files[&a].used * 2 < self.files[&a].size) {
            return None;
        }
        let seq = self.next_file;
        self.next_file += 1;
        let id = self.new_id();
        self.spare = Some(Spare { seq, id, ready: false });
        let path = format::segment_path(seq);
        let header = Bytes::from_vec(format::segment_header(seq));
        let mut ops = vec![IoOp::Create { file: id, path, prealloc: self.file_bytes }];
        ops.extend(zeros(id, self.file_bytes));
        ops.extend([
            IoOp::WriteAt { file: id, offset: 0, data: header, batch: 0 },
            IoOp::Sync { file: id, batch: 0 },
            IoOp::SyncDir { path: format::DIR.into() },
        ]);
        Some(SpareJob { seq, ops })
    }

    /// The outcome of the [`SpareJob`] of file `seq`: `Ok` once all its operations succeeded, or
    /// the first error. A file that could not be made is made when a record needs it, as without
    /// one made ahead; one whose turn passed meanwhile holds nothing and goes in its turn.
    pub fn spare_done(&mut self, seq: u64, made: Result<(), IoErrno>) {
        if let Some(sp) = self.spare.as_mut().filter(|sp| sp.seq == seq) {
            match made {
                Ok(()) => sp.ready = true,
                Err(_) => {
                    self.spare = None;
                    self.spare_failed = true;
                }
            }
            return;
        }
        let Some(at) = self.passed.iter().position(|(s, _)| *s == seq) else { return };
        let (seq, id) = self.passed.swap_remove(at);
        if made.is_ok() {
            self.insert_empty(seq, id);
        }
    }

    /// A file made ahead whose turn passed: it goes once made, or now if it is.
    fn pass(&mut self, sp: Spare) {
        if sp.ready {
            self.insert_empty(sp.seq, sp.id);
        } else {
            self.passed.push((sp.seq, sp.id));
        }
    }

    /// Keeps file `seq`, made with its header and nothing else, until the oldest files go.
    fn insert_empty(&mut self, seq: u64, id: FileId) {
        let size = self.file_bytes;
        let file = LogFile { id, size, used: BLOCK, refs: 0, live: 0, freed_at: Some(0) };
        self.files.insert(seq, file);
    }

    /// The file made ahead, as the active one, if it is made and the record of `need` bytes fits.
    fn take_spare(&mut self, need: u64) -> Option<u64> {
        let sp = self.spare.filter(|sp| sp.ready)?;
        self.spare = None;
        if BLOCK + need > self.file_bytes {
            self.pass(sp);
            return None;
        }
        let file = LogFile {
            id: sp.id,
            size: self.file_bytes,
            used: BLOCK,
            refs: 0,
            live: 0,
            freed_at: None,
        };
        self.files.insert(sp.seq, file);
        self.active = Some(sp.seq);
        self.stats.files_made += 1;
        Some(sp.seq)
    }

    /// Writes the flush being built as one record, unsynced.
    fn write_out(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        let write = self.place(disk)?;
        ok(disk.execute(write)).map(|_| ())
    }

    /// Places the flush being built as one record of the active file (a new one if it does not
    /// fit, made on `disk`) and returns its write, unsynced.
    fn place(&mut self, disk: &mut dyn Disk) -> Result<IoOp, IoErrno> {
        // Every record carries the bound in force (D-172): the newest record has it even when the
        // files that had it went.
        let unbound = self.body.len();
        if self.bound > 0 {
            format::push_frame(&mut self.body, FrameKind::Bound, 0, 0, self.bound, &[]);
        }
        let record = format::record(self.next_flush, &self.body);
        let need = record.len() as u64;
        let fits = self.active.is_some_and(|a| self.files[&a].used + need <= self.files[&a].size);
        let seq = if fits {
            self.active.expect("an active file")
        } else if let Some(seq) = self.take_spare(need) {
            seq
        } else {
            match self.new_file(disk, need) {
                Ok(seq) => seq,
                Err(e) => {
                    // The record waits for the next try, which stamps it again.
                    self.body.truncate(unbound);
                    return Err(e);
                }
            }
        };
        if self.bound > 0 {
            self.stamped.push_back((self.next_flush, self.bound));
        }
        self.stats.records += 1;
        self.stats.record_bytes += need;
        self.stats.padding_bytes += need - (HEAD + self.body.len()) as u64;
        let (id, at) = {
            let f = &self.files[&seq];
            (f.id, f.used)
        };
        let data = Bytes::from_vec(record);
        self.files.get_mut(&seq).expect("the file written").used = at + need;
        let base = at + HEAD as u64;
        for p in std::mem::take(&mut self.pending) {
            let place = (seq, base + p.data_at as u64, p.len);
            self.apply(p.kind, (p.core, p.wal), p.arg, place, p.fill);
        }
        let flush = self.next_flush;
        for f in self.files.values_mut() {
            if f.refs > 0 {
                f.freed_at = None;
            } else if f.freed_at.is_none() {
                f.freed_at = Some(flush);
            }
        }
        self.body.clear();
        self.next_flush += 1;
        if !self.dirty.contains(&seq) {
            self.dirty.push(seq);
        }
        Ok(IoOp::WriteAt { file: id, offset: at, data, batch: 0 })
    }

    /// Writes what is waiting as one record and, if anything waits for durability, makes it
    /// durable with one sync, running the operations of [`next`](LogState::next) on `disk` one
    /// after another: for the callers that cannot wait for them apart.
    /// Returns the answers of the operations waiting. A full disk leaves everything waiting for
    /// the next try; any other error fails every operation waiting and every later one.
    pub fn flush(&mut self, disk: &mut dyn Disk) -> Answers {
        let mut out = Vec::new();
        let mut ops = Vec::new();
        loop {
            out.extend(self.step(disk, &mut ops, true));
            if ops.is_empty() {
                return out;
            }
            for op in ops.drain(..) {
                let done = disk.execute(op);
                out.extend(self.on_done(&done));
            }
        }
    }

    /// What the writer does next (012 FR-035), with the operations to hand to the disk in `ops`,
    /// whose results go to [`on_done`](LogState::on_done) in that order. With no sync in flight,
    /// it writes what waits as one record and starts the sync of every operation waiting; while
    /// one runs, it writes what came since as records of the next group, up to
    /// [`AHEAD_RECORDS`] of them. Without anything waiting for a sync it writes only once
    /// [`WRITE_AHEAD`] bytes wait. The rare steps (a new file, copying the oldest one forward,
    /// removing old files) run on `disk` before it returns. Returns the answers ready now: the
    /// operations waiting when nothing is left to sync, or failed.
    pub fn next(&mut self, disk: &mut dyn Disk, ops: &mut Vec<IoOp>) -> Answers {
        self.step(disk, ops, false)
    }

    fn step(&mut self, disk: &mut dyn Disk, ops: &mut Vec<IoOp>, all: bool) -> Answers {
        if let Some(errno) = self.failed {
            return self.answer(Some(errno));
        }
        match self.step_ops(disk, ops, all) {
            Ok(answers) => answers,
            Err(IoErrno::NoSpace) if self.disk_full => Vec::new(),
            Err(e) => self.fail(e),
        }
    }

    fn step_ops(
        &mut self,
        disk: &mut dyn Disk,
        ops: &mut Vec<IoOp>,
        all: bool,
    ) -> Result<Answers, IoErrno> {
        if self.sync_left > 0 {
            let room = self.ahead < self.ahead_records || self.body.len() >= WRITE_AHEAD;
            if !self.body.is_empty() && room {
                ops.push(self.place(disk)?);
                self.ahead += 1;
            }
            return Ok(Vec::new());
        }
        self.release(disk)?;
        if self.waiting.is_empty() {
            if !self.body.is_empty() && (all || self.body.len() >= WRITE_AHEAD) {
                ops.push(self.place(disk)?);
            }
            return Ok(Vec::new());
        }
        if all {
            self.copy_now(disk)?;
        }
        if !self.body.is_empty() {
            ops.push(self.place(disk)?);
        }
        // Everything written before is durable: nothing to sync.
        if self.dirty.is_empty() {
            return Ok(self.answer(None));
        }
        for seq in std::mem::take(&mut self.dirty) {
            ops.push(IoOp::Sync { file: self.files[&seq].id, batch: 0 });
            self.sync_left += 1;
        }
        self.syncing = std::mem::take(&mut self.waiting);
        self.sync_upto = self.next_flush - 1;
        self.ahead = 0;
        Ok(Vec::new())
    }

    /// The result of an operation [`next`](LogState::next) handed out, in the order it handed
    /// them out. Returns the answers of the group whose sync ended, or of every operation waiting
    /// if it failed.
    pub fn on_done(&mut self, done: &IoDone) -> Answers {
        let (op, err) = match done {
            IoDone::Ok(op) => (op, None),
            IoDone::Err { op, errno } => (op, Some(*errno)),
            IoDone::Read { .. } => return Vec::new(),
        };
        let sync = matches!(op, IoOp::Sync { .. });
        if sync {
            self.sync_left = self.sync_left.saturating_sub(1);
        }
        if self.failed.is_some() {
            return Vec::new();
        }
        if let Some(errno) = err {
            return self.fail(errno);
        }
        if !sync || self.sync_left > 0 {
            return Vec::new();
        }
        self.syncs += 1;
        self.stats.synced_requests += self.syncing.len() as u64;
        self.durable = self.sync_upto;
        while let Some(&(flush, bound)) = self.stamped.front()
            && flush <= self.durable
        {
            self.durable_bound = self.durable_bound.max(bound);
            self.stamped.pop_front();
        }
        std::mem::take(&mut self.syncing).into_iter().map(|(c, op)| (c, IoDone::Ok(op))).collect()
    }

    /// Stops the log with `errno`: the group in flight and every operation waiting fail.
    fn fail(&mut self, errno: IoErrno) -> Answers {
        self.failed = Some(errno);
        let mut out: Answers = std::mem::take(&mut self.syncing)
            .into_iter()
            .map(|(core, op)| (core, IoDone::Err { op, errno }))
            .collect();
        out.extend(self.answer(Some(errno)));
        out
    }

    fn answer(&mut self, err: Option<IoErrno>) -> Answers {
        std::mem::take(&mut self.waiting)
            .into_iter()
            .map(|(core, op)| match err {
                None => (core, IoDone::Ok(op)),
                Some(errno) => (core, IoDone::Err { op, errno }),
            })
            .collect()
    }

    /// Removes the oldest files nothing points into: after a sync, so that the frames that let go
    /// of them are durable.
    fn release(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        if !self.tidy {
            return Ok(());
        }
        let mut removed = false;
        while let Some((&seq, f)) = self.files.iter().next() {
            // A file freed by a record not durable yet is still needed after a crash.
            let freed = f.freed_at.is_some_and(|at| at <= self.durable);
            if Some(seq) == self.active || f.refs > 0 || !freed {
                break;
            }
            let id = f.id;
            ok(disk.execute(IoOp::Close { file: id }))?;
            ok(disk.execute(IoOp::Remove { path: format::segment_path(seq) }))?;
            self.files.remove(&seq);
            removed = true;
        }
        if removed {
            ok(disk.execute(IoOp::SyncDir { path: format::DIR.into() }))?;
        }
        Ok(())
    }
}
