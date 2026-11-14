//! A core's side of the device log: which of its disk operations are its WAL's, and how they go to
//! the writer. A core keeps writing its WAL segments as files (`core-NN/wal/<seq>.wal`); their
//! operations come here instead of to the core's disk. Writes, creations and deletions are queued
//! and answered on the spot, in the order the core asked; a sync waits for the writer's next sync
//! of the log, which covers everything queued before it.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use teseladb_raftlog::paths;
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};

use super::writer::{Req, Shared, Sink};
use crate::spsc::{SendError, SpscSender};

/// `EXDEV`: a WAL segment is never renamed or linked.
const CROSS_DEVICE: i32 = 18;

/// What one core has of the device log.
pub struct LogPort {
    core: u16,
    tx: SpscSender<Req>,
    /// What did not fit in the queue, sent before anything newer.
    overflow: VecDeque<Req>,
    /// The core's open WAL segments.
    files: HashMap<FileId, u64>,
    /// The core's WAL segments.
    wals: BTreeSet<u64>,
    /// The writer's answers before the core's loop takes them ([`LogPort::attach`]).
    answers: Receiver<IoDone>,
    shared: Arc<Shared>,
    closed: bool,
}

impl std::fmt::Debug for LogPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPort").field("core", &self.core).field("wals", &self.wals).finish()
    }
}

impl LogPort {
    pub(crate) fn new(
        core: u16,
        tx: SpscSender<Req>,
        wals: BTreeSet<u64>,
        shared: Arc<Shared>,
    ) -> (LogPort, Sink) {
        let (answer, answers) = mpsc::channel();
        let sink: Sink = Box::new(move |d| answer.send(d).is_ok());
        let port = LogPort {
            core,
            tx,
            overflow: VecDeque::new(),
            files: HashMap::new(),
            wals,
            answers,
            shared,
            closed: false,
        };
        (port, sink)
    }

    /// The core it serves.
    #[must_use]
    pub fn core(&self) -> u16 {
        self.core
    }

    /// The WAL segment `path` names, if it is one of this core's.
    fn wal_of(&self, path: &Path) -> Option<u64> {
        (path.parent() == Some(paths::wal_dir(self.core).as_path()))
            .then(|| paths::classify_wal(path))
            .flatten()
    }

    fn is_wal_dir(&self, path: &Path) -> bool {
        path == paths::wal_dir(self.core)
    }

    /// Whether `op` is an operation on this core's WAL, which goes to the log.
    #[must_use]
    pub fn takes(&self, op: &IoOp) -> bool {
        match op {
            IoOp::Create { path, .. } | IoOp::Open { path, .. } | IoOp::Remove { path } => {
                self.wal_of(path).is_some()
            }
            IoOp::SyncDir { path } | IoOp::RemoveDir { path } => self.is_wal_dir(path),
            IoOp::WriteAt { file, .. }
            | IoOp::ReadAt { file, .. }
            | IoOp::Sync { file, .. }
            | IoOp::Truncate { file, .. }
            | IoOp::Close { file } => self.files.contains_key(file),
            IoOp::Rename { from, to } | IoOp::Link { from, to } => {
                self.wal_of(from).is_some() || self.wal_of(to).is_some()
            }
        }
    }

    fn send(&mut self, req: Req) -> bool {
        if self.closed {
            return false;
        }
        if !self.overflow.is_empty() {
            self.overflow.push_back(req);
            return true;
        }
        match self.tx.send(req) {
            Ok(()) => true,
            Err(SendError::Full(r)) => {
                self.overflow.push_back(r);
                true
            }
            Err(SendError::Closed(_)) => {
                self.closed = true;
                false
            }
        }
    }

    /// Sends what waited for room in the queue. Returns whether something still waits.
    pub fn pump(&mut self) -> bool {
        while let Some(r) = self.overflow.pop_front() {
            match self.tx.send(r) {
                Ok(()) => {}
                Err(SendError::Full(r)) => {
                    self.overflow.push_front(r);
                    return true;
                }
                Err(SendError::Closed(_)) => {
                    self.closed = true;
                    self.overflow.clear();
                }
            }
        }
        false
    }

    /// Sends everything waiting, yielding while the queue is full: for the synchronous callers,
    /// which then wait for the writer.
    fn push_all(&mut self) {
        while self.pump() {
            std::thread::yield_now();
        }
    }

    /// Hands an operation the port [`takes`](LogPort::takes) to the log. Returns its answer, or
    /// `None` for a sync, whose answer comes from the writer once the log is durable.
    pub fn submit(&mut self, op: IoOp) -> Option<IoDone> {
        let err = |op: &IoOp, errno| Some(IoDone::Err { op: op.without_data(), errno });
        let done = Some(IoDone::Ok(op.without_data()));
        match &op {
            IoOp::Create { file, path, prealloc } => {
                let wal = self.wal_of(path)?;
                if self.wals.contains(&wal) {
                    return err(&op, IoErrno::AlreadyExists);
                }
                if self.shared.disk_full() {
                    return err(&op, IoErrno::NoSpace);
                }
                if !self.send(Req::Create { wal, len: *prealloc }) {
                    return err(&op, IoErrno::Io);
                }
                self.wals.insert(wal);
                self.files.insert(*file, wal);
                done
            }
            IoOp::Open { file, path } => match self.wal_of(path) {
                Some(wal) if self.wals.contains(&wal) => {
                    self.files.insert(*file, wal);
                    done
                }
                _ => err(&op, IoErrno::NotFound),
            },
            IoOp::WriteAt { file, offset, data, .. } => {
                let wal = self.files[file];
                let sent = self.send(Req::Write { wal, offset: *offset, data: data.clone() });
                if sent { done } else { err(&op, IoErrno::Io) }
            }
            IoOp::Truncate { file, len } => {
                let wal = self.files[file];
                if self.send(Req::Truncate { wal, len: *len }) {
                    done
                } else {
                    err(&op, IoErrno::Io)
                }
            }
            IoOp::Sync { .. } | IoOp::SyncDir { .. } => {
                if self.send(Req::Sync(op.without_data())) {
                    None
                } else {
                    err(&op, IoErrno::Io)
                }
            }
            IoOp::Close { file } => {
                self.files.remove(file);
                done
            }
            IoOp::Remove { path } => {
                let wal = self.wal_of(path)?;
                if !self.wals.remove(&wal) {
                    return err(&op, IoErrno::NotFound);
                }
                self.files.retain(|_, w| *w != wal);
                if self.send(Req::Remove { wal }) { done } else { err(&op, IoErrno::Io) }
            }
            IoOp::RemoveDir { .. } if !self.wals.is_empty() => err(&op, IoErrno::NotEmpty),
            IoOp::RemoveDir { .. } => done,
            IoOp::ReadAt { file, offset, len, token } => {
                let wal = self.files[file];
                let (start, len) = (*offset, *len as usize);
                let data = self.read(wal).map(|all| {
                    let from = usize::try_from(start).unwrap_or(usize::MAX).min(all.len());
                    let to = from.saturating_add(len).min(all.len());
                    teseladb_types::Bytes::from_vec(all[from..to].to_vec())
                });
                Some(IoDone::Read { token: *token, data })
            }
            IoOp::Rename { .. } | IoOp::Link { .. } => err(&op, IoErrno::Other(CROSS_DEVICE)),
        }
    }

    /// The whole contents of WAL segment `wal`, read by the writer. Blocks.
    fn read(&mut self, wal: u64) -> Result<Vec<u8>, IoErrno> {
        let (reply, got) = mpsc::channel();
        if !self.send(Req::Read { wal, reply }) {
            return Err(IoErrno::Io);
        }
        self.push_all();
        got.recv().unwrap_or(Err(IoErrno::Io))
    }

    /// From now on the writer hands its answers to `sink` (the core's mailbox); the core has
    /// recovered.
    pub fn attach(&mut self, sink: Sink) {
        self.send(Req::Attach(sink));
        self.push_all();
    }

    /// A view of `inner` as the core's disk before its loop runs (recovery): its WAL comes from
    /// the log, and a sync waits for its answer.
    pub fn disk<'a, D: Disk>(&'a mut self, inner: &'a mut D) -> PortDisk<'a, D> {
        PortDisk { port: self, inner }
    }
}

/// A core's disk with its WAL in the device log, synchronous ([`LogPort::disk`]).
pub struct PortDisk<'a, D> {
    port: &'a mut LogPort,
    inner: &'a mut D,
}

impl<D: Disk> PortDisk<'_, D> {
    /// Reads `path` from the core's own disk, also a WAL segment's path: what a directory of an
    /// earlier format kept there.
    ///
    /// # Errors
    ///
    /// The error of the read.
    pub fn inner_read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.inner.read_all(path)
    }
}

impl<D: Disk> Disk for PortDisk<'_, D> {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        if self.port.is_wal_dir(dir) {
            let core = self.port.core;
            return Ok(self.port.wals.iter().map(|w| paths::wal_segment(core, *w)).collect());
        }
        self.inner.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        match self.port.wal_of(path) {
            Some(wal) if self.port.wals.contains(&wal) => self.port.read(wal),
            Some(_) => Err(IoErrno::NotFound),
            None => self.inner.read_all(path),
        }
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        if !self.port.takes(&op) {
            return self.inner.execute(op);
        }
        let fallback = op.without_data();
        if let Some(done) = self.port.submit(op) {
            return done;
        }
        self.port.push_all();
        self.port.answers.recv().unwrap_or(IoDone::Err { op: fallback, errno: IoErrno::Io })
    }
}
