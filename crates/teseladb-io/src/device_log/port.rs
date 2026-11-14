//! A core's side of the device log on the node: its [`PortBook`] (which operations are its WAL's and
//! what each becomes) with a link to the writer thread. Requests go on a lock-free SPSC ring; what
//! does not fit waits in the port and goes first on the next turn.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

use teseladb_devlog::port::{LogLink, LogReq, PortBook};
use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

use super::writer::{Req, Shared, Sink};
use crate::spsc::{SendError, SpscSender};

/// The link of a core to the writer thread.
pub struct ThreadLink {
    tx: SpscSender<Req>,
    /// What did not fit in the queue, sent before anything newer.
    overflow: VecDeque<Req>,
    /// The writer's answers before the core's loop takes them ([`LogPort::attach`]).
    answers: Receiver<IoDone>,
    shared: Arc<Shared>,
    closed: bool,
}

impl ThreadLink {
    fn queue(&mut self, req: Req) -> bool {
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
    fn pump(&mut self) -> bool {
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
}

impl LogLink for ThreadLink {
    fn send(&mut self, req: LogReq) -> bool {
        // A sync carries when it was asked: the writer counts its wait in the queue.
        let asked = matches!(req, LogReq::Sync(_)).then(Instant::now);
        self.queue(Req::Log(req, asked))
    }

    fn read(&mut self, wal: u64) -> Result<Vec<u8>, IoErrno> {
        let (reply, got) = mpsc::channel();
        if !self.queue(Req::Read { wal, reply }) {
            return Err(IoErrno::Io);
        }
        self.push_all();
        got.recv().unwrap_or(Err(IoErrno::Io))
    }

    fn disk_full(&self) -> bool {
        self.shared.disk_full()
    }

    fn wait_sync(&mut self, op: IoOp) -> IoDone {
        self.push_all();
        self.answers.recv().unwrap_or(IoDone::Err { op, errno: IoErrno::Io })
    }
}

/// What one core has of the device log.
pub struct LogPort {
    book: PortBook,
    link: ThreadLink,
}

impl std::fmt::Debug for LogPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPort").field("book", &self.book).finish()
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
        let link = ThreadLink { tx, overflow: VecDeque::new(), answers, shared, closed: false };
        (LogPort { book: PortBook::new(core, wals), link }, sink)
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

    /// Sends what waited for room in the queue. Returns whether something still waits.
    pub fn pump(&mut self) -> bool {
        self.link.pump()
    }

    /// Hands an operation the port [`takes`](LogPort::takes) to the log. Returns its answer, or
    /// `None` for a sync, whose answer comes from the writer once the log is durable.
    pub fn submit(&mut self, op: IoOp) -> Option<IoDone> {
        self.book.submit(&mut self.link, op)
    }

    /// From now on the writer hands its answers to `sink` (the core's mailbox); the core has
    /// recovered.
    pub fn attach(&mut self, sink: Sink) {
        self.link.queue(Req::Attach(sink));
        self.link.push_all();
    }

    /// A view of `inner` as the core's disk before its loop runs (recovery): its WAL comes from
    /// the log, and a sync waits for its answer.
    pub fn disk<'a, D: Disk>(&'a mut self, inner: &'a mut D) -> super::PortDisk<'a, D> {
        self.book.disk(&mut self.link, inner)
    }
}
