//! The interface between a core's task and the executor of its disk operations (023 FR-010): the
//! executor takes [`IoOp`]s with their class during a turn and hands the results back in
//! submission order within each class, waking the task when they arrive.
//!
//! The executor on the engine's files, and over a disk in memory, is
//! [`ShardDisk`](super::disk::ShardDisk).

use std::rc::Rc;

use teseladb::engine::core::future::Future;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb_raftlog::{FileId, IoDone, IoOp};

use super::post::CoreIo;
use crate::IoClass;

/// What a core's executor uses to tell the core's task that results arrived.
#[derive(Clone)]
pub struct DiskWaker {
    io: Rc<CoreIo>,
}

impl DiskWaker {
    pub(crate) fn new(io: Rc<CoreIo>) -> DiskWaker {
        DiskWaker { io }
    }

    /// Wakes the core's task for results that arrived (call it on the shard).
    pub fn wake(&self) {
        self.io.wake.wake();
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
    /// Whether it reads the bytes of files for the answers that end with them
    /// ([`CoreDisk::read_for_send`]).
    fn sends_files(&self) -> bool {
        false
    }
    /// Reads `len` bytes at `offset` of the open file `file` for an answer that ends with them
    /// (012 FR-026, 023 FR-017): `None` when the executor does not read them itself (the logic
    /// answers whole); else a future of the bytes, `None` in it when the file is not open or
    /// holds fewer bytes there.
    fn read_for_send(
        &self,
        _file: FileId,
        _offset: u64,
        _len: u32,
    ) -> Option<Future<Option<TemporaryBuffer<u8>>>> {
        None
    }
}
