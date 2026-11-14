//! The state of a device log, without threads: what each core's WAL segments hold and where, the
//! flush being built, and the steps of a flush on a [`Disk`]. The writer thread drives it
//! ([`super::writer`]); the tests drive it directly, also on a simulated disk that crashes.
//!
//! Each core's WAL segment lives in the log as a map from its offsets to the places of the log's
//! files that hold them. A file of the log is needed while a map points into it; the oldest files
//! go once nothing points into them, always oldest first, so that a `REMOVE` or a `TRUNCATE` is
//! never replayed without what came before it. A core that writes little keeps its current WAL
//! segment for a long time: when the log has more than [`KEPT_FILES`] files and the oldest holds
//! little that is still needed, a flush copies that part forward and the file goes.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

use super::format::{self, BLOCK, FrameKind, HEAD};

/// Files of the log kept before the oldest one is copied forward.
pub const KEPT_FILES: usize = 4;

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
    /// A file made and its name not synced.
    name_dirty: bool,
    waiting: Vec<(u16, IoOp)>,
    failed: Option<IoErrno>,
    disk_full: bool,
    syncs: u64,
    /// Whether old files may be copied forward and removed: not while cores recover.
    tidy: bool,
    /// Whether the log's directory may be new: its name in the data directory is synced with the
    /// first file.
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
    /// The error of a listing, read or open that failed.
    pub fn recover(disk: &mut dyn Disk, file_bytes: u64) -> Result<LogState, IoErrno> {
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
            name_dirty: false,
            waiting: Vec::new(),
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
            Err(e) => return Err(e),
        };
        paths.sort();
        s.dir_new = paths.is_empty();
        let mut expected: Option<u64> = None;
        for (seq, path) in paths {
            let data = disk.read_all(&path)?;
            let id = s.new_id();
            ok(disk.execute(IoOp::Open { file: id, path }))?;
            let size = data.len() as u64;
            s.files.insert(seq, LogFile { id, size, used: size, refs: 0, live: 0 });
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

    /// Makes file `seq` for a record of `need` bytes: created, its header written with the record
    /// by the caller, and its name made durable by the sync of the flush.
    fn new_file(&mut self, disk: &mut dyn Disk, need: u64) -> Result<u64, IoErrno> {
        let seq = self.next_file;
        let size = self.file_bytes.max(BLOCK + need);
        let id = self.new_id();
        let path = format::segment_path(seq);
        match disk.execute(IoOp::Create { file: id, path, prealloc: size }) {
            IoDone::Err { errno, .. } => Err(errno),
            _ => {
                self.next_file += 1;
                self.files.insert(seq, LogFile { id, size, used: 0, refs: 0, live: 0 });
                self.active = Some(seq);
                Ok(seq)
            }
        }
    }

    /// Writes the flush being built as one record, unsynced.
    fn write_out(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        let record = format::record(self.next_flush, &self.body);
        let need = record.len() as u64;
        let fits = self.active.is_some_and(|a| self.files[&a].used + need <= self.files[&a].size);
        let opened = !fits;
        let seq = if fits {
            self.active.expect("an active file")
        } else {
            match self.new_file(disk, need) {
                Ok(s) => s,
                Err(IoErrno::NoSpace) => {
                    self.disk_full = true;
                    return Err(IoErrno::NoSpace);
                }
                Err(e) => return Err(e),
            }
        };
        self.disk_full = false;
        let (id, at) = {
            let f = &self.files[&seq];
            (f.id, if opened { BLOCK } else { f.used })
        };
        let data = if opened {
            let mut d = format::segment_header(seq);
            d.extend_from_slice(&record);
            ok(disk.execute(IoOp::WriteAt {
                file: id,
                offset: 0,
                data: Bytes::from_vec(d),
                batch: 0,
            }))?;
            Bytes::new()
        } else {
            Bytes::from_vec(record)
        };
        if !data.is_empty() {
            ok(disk.execute(IoOp::WriteAt { file: id, offset: at, data, batch: 0 }))?;
        }
        self.files.get_mut(&seq).expect("the file written").used = at + need;
        let base = at + HEAD as u64;
        for p in std::mem::take(&mut self.pending) {
            let place = (seq, base + p.data_at as u64, p.len);
            self.apply(p.kind, (p.core, p.wal), p.arg, place);
        }
        self.body.clear();
        self.next_flush += 1;
        if !self.dirty.contains(&seq) {
            self.dirty.push(seq);
        }
        self.name_dirty |= opened;
        Ok(())
    }

    /// Writes what is waiting as one record and, if anything waits for durability, makes it
    /// durable with one sync (two when the record opened a file, whose name is synced too).
    /// Returns the answers of the operations waiting. A full disk leaves everything waiting for
    /// the next try; any other error fails every operation waiting and every later one.
    pub fn flush(&mut self, disk: &mut dyn Disk) -> Answers {
        if let Some(errno) = self.failed {
            return self.answer(Some(errno));
        }
        match self.flush_steps(disk) {
            Ok(()) => {
                let out = self.answer(None);
                if let Err(e) = self.release(disk) {
                    self.failed = Some(e);
                }
                out
            }
            Err(IoErrno::NoSpace) if self.disk_full => Vec::new(),
            Err(e) => {
                self.failed = Some(e);
                self.answer(Some(e))
            }
        }
    }

    fn flush_steps(&mut self, disk: &mut dyn Disk) -> Result<(), IoErrno> {
        if self.waiting.is_empty() && self.body.is_empty() {
            return Ok(());
        }
        if !self.waiting.is_empty() {
            self.copy_oldest(disk)?;
            if !self.body.is_empty() {
                self.write_out(disk)?;
            }
        }
        if !self.waiting.is_empty() && !self.dirty.is_empty() {
            for seq in std::mem::take(&mut self.dirty) {
                let id = self.files[&seq].id;
                ok(disk.execute(IoOp::Sync { file: id, batch: 0 }))?;
            }
            if std::mem::take(&mut self.name_dirty) {
                ok(disk.execute(IoOp::SyncDir { path: format::DIR.into() }))?;
                if std::mem::take(&mut self.dir_new) {
                    ok(disk.execute(IoOp::SyncDir { path: "".into() }))?;
                }
            }
            self.syncs += 1;
            crate::counters::record_log_sync();
        } else if self.waiting.is_empty() {
            self.write_out(disk)?;
        }
        Ok(())
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
            if Some(seq) == self.active || f.refs > 0 {
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
