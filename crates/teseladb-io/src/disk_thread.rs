//! Per-core disk thread: it executes the core's [`IoOp`]s one after another, in the
//! order they were submitted, and hands every [`IoDone`] back through the core's mailbox. The
//! core thread never blocks on disk.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use teseladb_raftlog::{Disk, IoDone, IoOp};

use crate::mailbox::MailboxSender;

/// Handle of a running disk thread.
#[derive(Debug)]
pub struct DiskThread<D> {
    ops: Sender<IoOp>,
    handle: Option<JoinHandle<D>>,
}

impl<D: Disk + Send + 'static> DiskThread<D> {
    /// Starts a thread named `name` that owns `disk`. Every result is wrapped with `wrap` and
    /// sent to `done`; the thread stops when the handle is shut down or when `done`'s receiver is
    /// gone (then the remaining operations are not executed).
    pub fn spawn<M: Send + 'static>(
        name: String,
        disk: D,
        done: MailboxSender<M>,
        wrap: fn(IoDone) -> M,
    ) -> io::Result<DiskThread<D>> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new().name(name).spawn(move || run(disk, rx, done, wrap))?;
        Ok(DiskThread { ops: tx, handle: Some(handle) })
    }

    /// Queues `op` behind the ones already submitted. Returns it if the thread has stopped.
    pub fn submit(&self, op: IoOp) -> Result<(), IoOp> {
        self.ops.send(op).map_err(|e| e.0)
    }

    /// Whether the thread still runs: a thread that panicked will complete nothing more.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Waits for the queued operations to finish and returns the disk. `None` if the thread
    /// panicked.
    pub fn shutdown(mut self) -> Option<D> {
        self.stop()
    }

    fn stop(&mut self) -> Option<D> {
        self.close();
        self.handle.take().and_then(|h| h.join().ok())
    }
}

impl<D> DiskThread<D> {
    /// Drops the real sender (replacing it with one of a dead channel), which ends the thread's
    /// loop once its queue is empty.
    fn close(&mut self) {
        self.ops = mpsc::channel().0;
    }
}

impl<D> Drop for DiskThread<D> {
    fn drop(&mut self) {
        self.close();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run<D: Disk, M: Send>(
    mut disk: D,
    ops: Receiver<IoOp>,
    done: MailboxSender<M>,
    wrap: fn(IoDone) -> M,
) -> D {
    for op in ops {
        if done.send_waiting(wrap(disk.execute(op))).is_err() {
            break;
        }
    }
    disk
}
