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
//! A file's header is durable before any record goes into it. On recovery a file without a valid
//! header is the one whose creation a crash cut short, and holds only zeros after its header; one
//! with anything else there was damaged, and the log does not start.

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

/// Bytes of a WAL segment held at one place of a file of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    end: u64,
    file: u64,
    /// Where its first byte is in the file.
    pos: u64,
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

/// A frame of the flush being built, to apply to the maps once its place is known.
#[derive(Debug)]
struct Pending {
    kind: FrameKind,
    core: u16,
    wal: u64,
    arg: u64,
    len: u64,
    data_at: usize,
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
    failed: Option<IoErrno>,
    disk_full: bool,
    syncs: u64,
    /// Whether old files may be copied forward and removed: not while cores recover.
    tidy: bool,
    /// Whether the log's directory may be new: its name in the data directory is synced with the
    /// first file's.
    dir_new: bool,
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
            failed: None,
            disk_full: false,
            syncs: 0,
            tidy: false,
            dir_new: false,
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
                    let pos = base + f.data_at as u64;
                    s.apply(f.kind, (f.core, f.segment), f.arg, (seq, pos, u64::from(f.len)));
                }
                expected = Some(r.flush + 1);
                at += r.len;
            }
        }
        s.next_flush = expected.unwrap_or(1);
        s.durable = s.next_flush - 1;
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

    fn frame(&mut self, kind: FrameKind, core: u16, wal: u64, arg: u64, data: &[u8]) {
        let data_at = self.body.len() + format::FRAME_HEAD;
        format::push_frame(&mut self.body, kind, core, wal, arg, data);
        self.pending.push(Pending { kind, core, wal, arg, len: data.len() as u64, data_at });
    }

    /// WAL segment `wal` of `core` exists, with at least `len` bytes.
    pub fn create(&mut self, core: u16, wal: u64, len: u64) {
        self.frame(FrameKind::Create, core, wal, len, &[]);
    }

    /// `data` at `offset` of WAL segment `wal` of `core`.
    pub fn write(&mut self, core: u16, wal: u64, offset: u64, data: &[u8]) {
        self.frame(FrameKind::Write, core, wal, offset, data);
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
        let hit: Vec<(u64, Extent)> =
            w.extents.range(..end).filter(|(_, e)| e.end > start).map(|(s, e)| (*s, *e)).collect();
        let mut refs = Vec::new();
        for (s, e) in hit {
            w.extents.remove(&s);
            refs.push((e.file, e.end - s, false));
            if s < start {
                w.extents.insert(s, Extent { end: start, ..e });
                refs.push((e.file, start - s, true));
            }
            if e.end > end {
                w.extents.insert(end, Extent { end: e.end, file: e.file, pos: e.pos + (end - s) });
                refs.push((e.file, e.end - end, true));
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
            FrameKind::Write => {
                self.cut(key, arg, arg + len);
                let w = self.wals.entry(key).or_default();
                w.len = w.len.max(arg + len);
                if len > 0 {
                    w.extents.insert(arg, Extent { end: arg + len, file, pos });
                    self.add_ref(file, len);
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
        }
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
            let data = self.read_at(disk, e.file, e.pos, e.end - s)?;
            let at = usize::try_from(s).map_err(|_| IoErrno::Io)?;
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

    /// Copies forward what is still needed of the oldest file, if it is time to.
    fn copy_oldest(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        let old: Vec<u64> =
            self.files.keys().copied().filter(|f| Some(*f) != self.active).collect();
        if !self.tidy || old.len() < KEPT_FILES {
            return Ok(());
        }
        let oldest = old[0];
        let f = &self.files[&oldest];
        if f.refs == 0 || f.live * 2 > self.file_bytes {
            return Ok(());
        }
        let mut parts = Vec::new();
        let mut creates = Vec::new();
        for (key, w) in &self.wals {
            if w.created_in == Some(oldest) {
                creates.push((*key, w.len));
            }
            for (s, e) in &w.extents {
                if e.file == oldest {
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
            let data = self.read_at(disk, e.file, e.pos, e.end - s)?;
            self.write(core, wal, s, &data);
        }
        let shift = self.body.len();
        self.body.extend_from_slice(&body);
        self.pending
            .extend(pending.into_iter().map(|p| Pending { data_at: p.data_at + shift, ..p }));
        Ok(())
    }

    /// Makes file `seq` for a record of `need` bytes, with its header and its name durable before
    /// any record goes into it: a file without a valid header then holds nothing.
    fn new_file(&mut self, disk: &mut dyn Disk, need: u64) -> Result<u64, IoErrno> {
        let seq = self.next_file;
        let size = self.file_bytes.max(BLOCK + need);
        let id = self.new_id();
        let path = format::segment_path(seq);
        // A full disk leaves the record waiting for the next try; nothing was made.
        let made = ok(disk.execute(IoOp::Create { file: id, path, prealloc: size }));
        self.disk_full = matches!(made, Err(IoErrno::NoSpace));
        made?;
        self.next_file += 1;
        let file = LogFile { id, size, used: BLOCK, refs: 0, live: 0, freed_at: None };
        self.files.insert(seq, file);
        self.active = Some(seq);
        let header = Bytes::from_vec(format::segment_header(seq));
        ok(disk.execute(IoOp::WriteAt { file: id, offset: 0, data: header, batch: 0 }))?;
        ok(disk.execute(IoOp::Sync { file: id, batch: 0 }))?;
        ok(disk.execute(IoOp::SyncDir { path: format::DIR.into() }))?;
        if std::mem::take(&mut self.dir_new) {
            ok(disk.execute(IoOp::SyncDir { path: "".into() }))?;
        }
        Ok(seq)
    }

    /// Writes the flush being built as one record, unsynced.
    fn write_out(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        let write = self.place(disk)?;
        ok(disk.execute(write)).map(|_| ())
    }

    /// Places the flush being built as one record of the active file (a new one if it does not
    /// fit, made on `disk`) and returns its write, unsynced.
    fn place(&mut self, disk: &mut dyn Disk) -> Result<IoOp, IoErrno> {
        let record = format::record(self.next_flush, &self.body);
        let need = record.len() as u64;
        let fits = self.active.is_some_and(|a| self.files[&a].used + need <= self.files[&a].size);
        let seq =
            if fits { self.active.expect("an active file") } else { self.new_file(disk, need)? };
        let (id, at) = {
            let f = &self.files[&seq];
            (f.id, f.used)
        };
        let data = Bytes::from_vec(record);
        self.files.get_mut(&seq).expect("the file written").used = at + need;
        let base = at + HEAD as u64;
        for p in std::mem::take(&mut self.pending) {
            let place = (seq, base + p.data_at as u64, p.len);
            self.apply(p.kind, (p.core, p.wal), p.arg, place);
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
            let room = self.ahead < AHEAD_RECORDS || self.body.len() >= WRITE_AHEAD;
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
        self.copy_oldest(disk)?;
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
        self.durable = self.sync_upto;
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
