//! Per-core disk thread: it executes the core's [`IoOp`]s one after another, in the
//! order they were submitted, and hands every [`IoDone`] back through the core's mailbox. The
//! core thread never blocks on disk. The sync of a store's batch on a real disk (`IoOp::Sync` with
//! `batch > 0`) goes to the core's sync thread, which waits for the drive's flush and hands its
//! result back on its own: the disk thread writes the next batches meanwhile, and the sync
//! thread makes all the batches that waited durable with one sync of their file in the drive's
//! next flush (012 FR-014, D-159). Its result may come after those of later operations; the store
//! makes its batches durable in order.

use std::any::Any;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use teseladb_raftlog::{Disk, IoDone, IoOp};

use crate::file_disk::{DetachedSync, FileDisk};
use crate::mailbox::MailboxSender;
use crate::node_disk::NodeDisk;

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
        let sync_name = format!("{name}-sync");
        let handle = thread::Builder::new()
            .name(name)
            .spawn(move || run(disk, rx, (done, wrap), &sync_name))?;
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

fn run<D: Disk + 'static, M: Send + 'static>(
    mut disk: D,
    ops: Receiver<IoOp>,
    (done, wrap): (MailboxSender<M>, fn(IoDone) -> M),
    sync_name: &str,
) -> D {
    let mut syncs: Option<Syncs> = None;
    for op in ops {
        let detached = detach(&disk, &op);
        if let Some(job) = detached {
            let helper = match &mut syncs {
                Some(h) => h,
                None => match Syncs::spawn(sync_name, done.clone(), wrap) {
                    Ok(h) => syncs.insert(h),
                    // Without a second thread the sync runs here, in order.
                    Err(_) => {
                        let answers = DetachedSync::run_all(vec![job]);
                        if answers.into_iter().any(|d| done.send_waiting(wrap(d)).is_err()) {
                            break;
                        }
                        continue;
                    }
                },
            };
            helper.submit(job);
            continue;
        }
        if done.send_waiting(wrap(disk.execute(op))).is_err() {
            break;
        }
    }
    // The syncs handed over finish before the disk goes back.
    if let Some(h) = syncs {
        h.join();
    }
    disk
}

/// The sync of a store's batch as a job for the second thread, when `disk` is a real one: the
/// node's disk or a bare `FileDisk`. Any other disk (in memory, simulated) runs it in order.
fn detach<D: 'static>(disk: &D, op: &IoOp) -> Option<DetachedSync> {
    let any = disk as &dyn Any;
    if let Some(d) = any.downcast_ref::<NodeDisk>() {
        return d.detach_sync(op);
    }
    any.downcast_ref::<FileDisk>().and_then(|d| d.detach_sync(op))
}

/// The thread that runs the syncs of a core's batches away from its disk thread. Each turn it
/// takes every sync waiting and makes their files durable with one sync per file: the batches
/// that arrived while the drive flushed go together in its next flush (012 FR-014, D-159).
struct Syncs {
    jobs: Sender<DetachedSync>,
    handle: JoinHandle<()>,
}

impl Syncs {
    fn spawn<M: Send + 'static>(
        name: &str,
        done: MailboxSender<M>,
        wrap: fn(IoDone) -> M,
    ) -> io::Result<Syncs> {
        let (jobs, rx) = mpsc::channel::<DetachedSync>();
        let handle = thread::Builder::new().name(name.to_owned()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut turn = vec![first];
                turn.extend(rx.try_iter());
                for d in DetachedSync::run_all(turn) {
                    if done.send_waiting(wrap(d)).is_err() {
                        return;
                    }
                }
            }
        })?;
        Ok(Syncs { jobs, handle })
    }

    /// Queues `job`; a thread that stopped (its core is gone) has nobody to answer.
    fn submit(&mut self, job: DetachedSync) {
        let _ = self.jobs.send(job);
    }

    fn join(self) {
        drop(self.jobs);
        let _ = self.handle.join();
    }
}
