//! The interface between a core's task and the executor of its disk operations (023 FR-010): the
//! executor takes [`IoOp`]s with their class during a turn, sends them to the device once per turn
//! and hands the results back in submission order within each class.
//!
//! The executor on the engine's files implements [`CoreDisk`] (`engine/disk.rs`). Until it is
//! there, [`ThreadDisk`] runs any [`Disk`] on the core's disk thread and brings its results back
//! to the shard.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use teseladb_raftlog::{Disk, IoDone, IoOp};

use super::post::{CoreIo, ShardPost};
use crate::IoClass;
use crate::disk_thread::{DiskThread, DoneSink};

/// What a core's executor uses to bring results back to the core's task.
#[derive(Clone)]
pub struct DiskWaker {
    io: Rc<CoreIo>,
    post: ShardPost,
}

impl DiskWaker {
    pub(crate) fn new(io: Rc<CoreIo>, post: ShardPost) -> DiskWaker {
        DiskWaker { io, post }
    }

    /// Wakes the core's task for results that arrived on the shard (call it on the shard).
    pub fn wake(&self) {
        self.io.wake.wake();
    }

    /// A sink for results produced on another thread: each one reaches the core's shard, where
    /// [`DiskWaker::take_posted`] takes it.
    pub fn sink(&self) -> DoneSink {
        let post = self.post.clone();
        Arc::new(move |d| post.post_disk(d))
    }

    /// Moves the results that [`DiskWaker::sink`] brought to the shard to `out`.
    pub fn take_posted(&self, out: &mut Vec<IoDone>) {
        self.io.take_disk(out);
    }
}

/// The executor of one core's disk operations, on the core's shard.
pub trait CoreDisk: 'static {
    /// Called once, on the shard, before the first operation.
    fn attach(&mut self, waker: DiskWaker);
    /// Takes `op` of `class`. `false` when the executor can take nothing more (its disk is gone,
    /// so nothing can be made durable any more).
    fn submit(&mut self, op: IoOp, class: IoClass) -> bool;
    /// Sends what this turn submitted to the device. `false` as for [`CoreDisk::submit`].
    fn flush(&mut self) -> bool;
    /// Moves the results ready to `out`, in submission order within each class.
    fn reap(&mut self, out: &mut Vec<(IoDone, IoClass)>);
    /// Whether results wait that no wake-up will announce (the task does not sleep then).
    fn ready(&self) -> bool {
        false
    }
    /// Whether it still runs operations: one that died completes nothing more.
    fn alive(&self) -> bool;
}

/// Runs a [`Disk`] on the core's disk thread (the executor until the engine's files take over).
pub struct ThreadDisk<D: Disk + Send + 'static> {
    name: String,
    disk: Option<D>,
    thread: Option<DiskThread<D>>,
    waker: Option<DiskWaker>,
    /// The classes of the operations handed to the thread, in order, but those it answers apart,
    /// which are all the log's ([`crate::answered_apart`]).
    classes: VecDeque<IoClass>,
    posted: Vec<IoDone>,
    /// The thread could not start.
    failed: bool,
}

impl<D: Disk + Send + 'static> ThreadDisk<D> {
    /// `disk` on a thread named `name`, started when the core attaches it.
    pub fn new(name: String, disk: D) -> ThreadDisk<D> {
        ThreadDisk {
            name,
            disk: Some(disk),
            thread: None,
            waker: None,
            classes: VecDeque::new(),
            posted: Vec::new(),
            failed: false,
        }
    }

    /// Waits for the operations queued and gives the disk back (`None` if its thread panicked).
    pub fn into_disk(mut self) -> Option<D> {
        match self.thread.take() {
            Some(t) => t.shutdown(),
            None => self.disk.take(),
        }
    }

    /// The class of a result: the log's for one answered apart, else the class of the oldest
    /// operation handed to the thread, since it answers the rest in order.
    fn class_of(&mut self, done: &IoDone) -> IoClass {
        let apart = match done {
            IoDone::Ok(op) | IoDone::Err { op, .. } => crate::answered_apart(op),
            IoDone::Read { .. } => false,
        };
        if apart {
            return IoClass::Log;
        }
        self.classes.pop_front().unwrap_or(IoClass::Data)
    }
}

impl<D: Disk + Send + 'static> CoreDisk for ThreadDisk<D> {
    fn attach(&mut self, waker: DiskWaker) {
        let Some(disk) = self.disk.take() else { return };
        match DiskThread::spawn(self.name.clone(), disk, waker.sink()) {
            Ok(t) => self.thread = Some(t),
            Err(_) => self.failed = true,
        }
        self.waker = Some(waker);
    }

    fn submit(&mut self, op: IoOp, class: IoClass) -> bool {
        let Some(t) = &self.thread else { return false };
        let apart = crate::answered_apart(&op);
        if t.submit(op).is_err() {
            return false;
        }
        if !apart {
            self.classes.push_back(class);
        }
        true
    }

    fn flush(&mut self) -> bool {
        self.alive()
    }

    fn reap(&mut self, out: &mut Vec<(IoDone, IoClass)>) {
        let Some(w) = &self.waker else { return };
        w.take_posted(&mut self.posted);
        for done in std::mem::take(&mut self.posted) {
            let class = self.class_of(&done);
            out.push((done, class));
        }
    }

    fn alive(&self) -> bool {
        !self.failed && self.thread.as_ref().is_some_and(DiskThread::is_running)
    }
}
