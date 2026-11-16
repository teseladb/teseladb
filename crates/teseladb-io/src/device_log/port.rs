//! A core's side of the device log on the node: its [`PortBook`] (which operations are its WAL's
//! and what each becomes) with a link to the writer, a task of shard 0 (023 FR-013). What a core
//! asks during a turn waits in its port and reaches the writer in one message when the turn ends
//! ([`LogPort::pump`]), with `smp` from a shard or `alien` from another thread.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use teseladb_devlog::port::{LogLink, LogReq, PortBook};
use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

use teseladb::engine::core::thread::Thread;

use super::bound::Sink;
use super::writer::{Req, Shared, deliver};
use crate::engine::post::ShardPost;
use crate::engine::wait;

/// The next value of `rx`, `None` once its sender is gone. In an engine thread (a core's recovery
/// on its shard) the thread yields to its shard while it waits, so that the writer, which may run
/// on the same shard, goes on; a thread that is not a shard blocks. A shard's task cannot wait:
/// its own messages to the writer would never leave, so it gets `None` at once.
fn wait_for<T>(rx: &Receiver<T>) -> Option<T> {
    // `Thread::running_in_thread` is only defined on a shard: asked first.
    if wait::this_shard().is_none() {
        return rx.recv().ok();
    }
    if !Thread::running_in_thread() {
        return rx.try_recv().ok();
    }
    loop {
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(TryRecvError::Empty) => Thread::yield_(),
            Err(TryRecvError::Disconnected) => return None,
        }
    }
}

/// The link of a core to the writer.
pub struct EngineLink {
    /// Shard 0, where the writer runs.
    post: ShardPost,
    /// The writer's identifier on its shard.
    log: u64,
    core: u16,
    /// What waits for the end of the turn.
    batch: Vec<Req>,
    /// The writer's answers before the core's task takes them ([`LogPort::attach`]).
    answers: Receiver<IoDone>,
    shared: Arc<Shared>,
    closed: bool,
}

impl EngineLink {
    fn queue(&mut self, req: Req) -> bool {
        if self.closed {
            return false;
        }
        self.batch.push(req);
        true
    }

    /// Sends what waits to the writer, in one message.
    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let (log, core) = (self.log, self.core);
        let batch = std::mem::take(&mut self.batch);
        if !self.post.post(move || deliver(log, core, batch)) {
            self.closed = true;
        }
    }
}

impl LogLink for EngineLink {
    fn send(&mut self, req: LogReq) -> bool {
        // A sync carries when it was asked: the writer counts its wait in the queue.
        let asked = matches!(req, LogReq::Sync(_)).then(Instant::now);
        self.queue(Req::Log(req, asked))
    }

    fn read(&mut self, wal: u64) -> Result<Vec<u8>, IoErrno> {
        let (reply, got) = mpsc::sync_channel(1);
        if !self.queue(Req::Read { wal, reply }) {
            return Err(IoErrno::Io);
        }
        self.flush();
        wait_for(&got).unwrap_or(Err(IoErrno::Io))
    }

    fn disk_full(&self) -> bool {
        self.shared.disk_full()
    }

    fn wait_sync(&mut self, op: IoOp) -> IoDone {
        self.flush();
        wait_for(&self.answers).unwrap_or(IoDone::Err { op, errno: IoErrno::Io })
    }
}

/// What one core has of the device log.
pub struct LogPort {
    book: PortBook,
    link: EngineLink,
}

impl std::fmt::Debug for LogPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPort").field("book", &self.book).finish()
    }
}

impl LogPort {
    pub(crate) fn new(
        core: u16,
        (post, log): (ShardPost, u64),
        wals: BTreeSet<u64>,
        answers: Receiver<IoDone>,
        shared: Arc<Shared>,
    ) -> LogPort {
        let link =
            EngineLink { post, log, core, batch: Vec::new(), answers, shared, closed: false };
        LogPort { book: PortBook::new(core, wals), link }
    }

    /// The core it serves.
    #[must_use]
    pub fn core(&self) -> u16 {
        self.book.core()
    }

    /// Whether `op` is an operation on this core's WAL, which goes to the log.
    #[must_use]
    pub fn takes(&self, op: &IoOp) -> bool {
        self.book.takes(op)
    }

    /// Sends what the turn asked of the log to the writer. Returns whether something still waits
    /// (never: a message to shard 0 always leaves).
    pub fn pump(&mut self) -> bool {
        self.link.flush();
        false
    }

    /// Hands an operation the port [`takes`](LogPort::takes) to the log. Returns its answer, or
    /// `None` for a sync, whose answer comes from the writer once the log is durable.
    pub fn submit(&mut self, op: IoOp) -> Option<IoDone> {
        self.book.submit(&mut self.link, op)
    }

    /// From now on the writer hands its answers to `sink` (the core's task); the core has
    /// recovered.
    pub fn attach(&mut self, sink: Sink) {
        self.link.queue(Req::Attach(sink));
        self.link.flush();
    }

    /// A view of `inner` as the core's disk before its task runs (recovery): its WAL comes from
    /// the log, and a sync waits for its answer: on a thread that is not a shard, or in an engine
    /// thread of a shard (as a core recovers in `NodeEngine::start_core`).
    pub fn disk<'a, D: Disk>(&'a mut self, inner: &'a mut D) -> super::PortDisk<'a, D> {
        self.book.disk(&mut self.link, inner)
    }
}
