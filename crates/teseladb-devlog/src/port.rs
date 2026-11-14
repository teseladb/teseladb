//! A core's side of the device log: which of its disk operations are its WAL's, and what each
//! becomes for the log. A core keeps writing its WAL segments as files (`core-NN/wal/<seq>.wal`);
//! their operations come here instead of to the core's disk. Writes, creations and deletions are
//! handed to the log and answered on the spot, in the order the core asked; a sync waits for the
//! log's next sync, which covers everything handed to it before.
//!
//! How the requests reach the log is the [`LogLink`]'s: a lock-free queue to the writer thread on
//! the node, the log's state itself in the simulator.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use teseladb_raftlog::paths;
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::state::LogState;

/// `EXDEV`: a WAL segment is never renamed or linked.
const CROSS_DEVICE: i32 = 18;

/// What a core asks of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogReq {
    /// WAL segment `wal` exists, with at least `len` bytes.
    Create {
        /// The segment.
        wal: u64,
        /// Its reserved length.
        len: u64,
    },
    /// `data` at `offset` of WAL segment `wal`.
    Write {
        /// The segment.
        wal: u64,
        /// Where the data goes.
        offset: u64,
        /// The bytes.
        data: Bytes,
    },
    /// WAL segment `wal` ends at `len`.
    Truncate {
        /// The segment.
        wal: u64,
        /// Its new length.
        len: u64,
    },
    /// WAL segment `wal` is gone.
    Remove {
        /// The segment.
        wal: u64,
    },
    /// Answered once everything before it is durable.
    Sync(IoOp),
}

impl LogReq {
    /// Hands it to the log `state` as core `core`'s.
    pub fn apply(self, state: &mut LogState, core: u16) {
        match self {
            LogReq::Create { wal, len } => state.create(core, wal, len),
            LogReq::Write { wal, offset, data } => state.write(core, wal, offset, &data),
            LogReq::Truncate { wal, len } => state.truncate(core, wal, len),
            LogReq::Remove { wal } => state.remove(core, wal),
            LogReq::Sync(op) => state.sync(core, op),
        }
    }
}

/// How a core's requests reach the log.
pub trait LogLink {
    /// Hands `req` to the log; `false` if the log is gone.
    fn send(&mut self, req: LogReq) -> bool;

    /// The whole contents of WAL segment `wal`, with what was sent before it. Blocks.
    ///
    /// # Errors
    ///
    /// The error of the read, or `Io` if the log is gone.
    fn read(&mut self, wal: u64) -> Result<Vec<u8>, IoErrno>;

    /// Whether the log found the disk full the last time it tried to make a file.
    fn disk_full(&self) -> bool;

    /// The answer of `op`, the sync just sent, once the log is durable. Blocks: for the
    /// synchronous callers, such as a core's recovery.
    fn wait_sync(&mut self, op: IoOp) -> IoDone;
}

/// What one core has of the device log: its WAL segments and the files it has open on them.
#[derive(Debug)]
pub struct PortBook {
    core: u16,
    /// The core's open WAL segments.
    files: HashMap<FileId, u64>,
    /// The core's WAL segments.
    wals: BTreeSet<u64>,
}

impl PortBook {
    /// Core `core`'s, which has `wals` in the log.
    #[must_use]
    pub fn new(core: u16, wals: BTreeSet<u64>) -> PortBook {
        PortBook { core, files: HashMap::new(), wals }
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

    /// Hands an operation the book [`takes`](PortBook::takes) to the log through `link`. Returns
    /// its answer, or `None` for a sync, whose answer comes from the log once it is durable.
    pub fn submit(&mut self, link: &mut dyn LogLink, op: IoOp) -> Option<IoDone> {
        let err = |op: &IoOp, errno| Some(IoDone::Err { op: op.without_data(), errno });
        let done = Some(IoDone::Ok(op.without_data()));
        match &op {
            IoOp::Create { file, path, prealloc } => {
                let wal = self.wal_of(path)?;
                if self.wals.contains(&wal) {
                    return err(&op, IoErrno::AlreadyExists);
                }
                if link.disk_full() {
                    return err(&op, IoErrno::NoSpace);
                }
                if !link.send(LogReq::Create { wal, len: *prealloc }) {
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
                let sent = link.send(LogReq::Write { wal, offset: *offset, data: data.clone() });
                if sent { done } else { err(&op, IoErrno::Io) }
            }
            IoOp::Truncate { file, len } => {
                let wal = self.files[file];
                if link.send(LogReq::Truncate { wal, len: *len }) {
                    done
                } else {
                    err(&op, IoErrno::Io)
                }
            }
            IoOp::Sync { .. } | IoOp::SyncDir { .. } => {
                if link.send(LogReq::Sync(op.without_data())) {
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
                if link.send(LogReq::Remove { wal }) { done } else { err(&op, IoErrno::Io) }
            }
            IoOp::RemoveDir { .. } if !self.wals.is_empty() => err(&op, IoErrno::NotEmpty),
            IoOp::RemoveDir { .. } => done,
            IoOp::ReadAt { file, offset, len, token } => {
                let wal = self.files[file];
                let (start, len) = (*offset, *len as usize);
                let data = link.read(wal).map(|all| {
                    let from = usize::try_from(start).unwrap_or(usize::MAX).min(all.len());
                    let to = from.saturating_add(len).min(all.len());
                    Bytes::from_vec(all[from..to].to_vec())
                });
                Some(IoDone::Read { token: *token, data })
            }
            IoOp::Rename { .. } | IoOp::Link { .. } => err(&op, IoErrno::Other(CROSS_DEVICE)),
        }
    }

    /// A view of `inner` as the core's disk before its loop runs (recovery): its WAL comes from
    /// the log through `link`, and a sync waits for its answer.
    pub fn disk<'a, L: LogLink, D: Disk>(
        &'a mut self,
        link: &'a mut L,
        inner: &'a mut D,
    ) -> PortDisk<'a, L, D> {
        PortDisk { book: self, link, inner }
    }
}

/// A core's disk with its WAL in the device log, synchronous ([`PortBook::disk`]).
pub struct PortDisk<'a, L, D> {
    book: &'a mut PortBook,
    link: &'a mut L,
    inner: &'a mut D,
}

impl<L: LogLink, D: Disk> PortDisk<'_, L, D> {
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

impl<L: LogLink, D: Disk> Disk for PortDisk<'_, L, D> {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        if self.book.is_wal_dir(dir) {
            let core = self.book.core;
            return Ok(self.book.wals.iter().map(|w| paths::wal_segment(core, *w)).collect());
        }
        self.inner.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        match self.book.wal_of(path) {
            Some(wal) if self.book.wals.contains(&wal) => self.link.read(wal),
            Some(_) => Err(IoErrno::NotFound),
            None => self.inner.read_all(path),
        }
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        if !self.book.takes(&op) {
            return self.inner.execute(op);
        }
        let sync = op.without_data();
        match self.book.submit(self.link, op) {
            Some(done) => done,
            None => self.link.wait_sync(sync),
        }
    }
}

/// A [`LogLink`] straight to the log's state on its disk, for a caller that is the log's only
/// user while it runs: the simulator, and the tests.
pub struct DirectLink<'a> {
    /// The log.
    pub state: &'a mut LogState,
    /// Its disk.
    pub disk: &'a mut dyn Disk,
    /// The core whose requests these are.
    pub core: u16,
}

impl LogLink for DirectLink<'_> {
    fn send(&mut self, req: LogReq) -> bool {
        req.apply(self.state, self.core);
        true
    }

    fn read(&mut self, wal: u64) -> Result<Vec<u8>, IoErrno> {
        self.state.read(self.disk, self.core, wal)
    }

    fn disk_full(&self) -> bool {
        self.state.disk_full()
    }

    fn wait_sync(&mut self, op: IoOp) -> IoDone {
        // Nobody else uses the log meanwhile: every answer of this flush is this core's sync.
        let answers = self.state.flush(self.disk);
        let done = answers.into_iter().rev().find(|(core, _)| *core == self.core);
        // A full disk keeps the sync waiting; this caller cannot wait, so its sync fails as the
        // disk does, and it starts again once there is room.
        let errno = if self.state.disk_full() { IoErrno::NoSpace } else { IoErrno::Io };
        done.map_or(IoDone::Err { op, errno }, |(_, d)| d)
    }
}
