//! Per-core disk thread: it executes the core's [`IoOp`]s one after another, in the
//! order they were submitted, and hands every [`IoDone`] back through the core's mailbox. The
//! core thread never blocks on disk. The sync of a store's batch on a real disk (`IoOp::Sync` with
//! `batch > 0`) goes to the core's sync thread, which waits for the drive's flush and hands its
//! result back on its own: the disk thread writes the next batches meanwhile, and each of them
//! goes in the first flush of the drive that starts after its write (012 FR-014, FR-020, D-159).
//! Its result may come after those of later operations; the store makes its batches durable in
//! order.

use std::any::Any;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use teseladb_raftlog::{Disk, IoDone, IoOp};

use crate::file_disk::{DetachedSync, FileDisk};
use crate::mailbox::MailboxSender;
use crate::node_disk::NodeDisk;

/// Handle of a running disk thread.
#[derive(Debug)]
pub struct DiskThread<D> {
    /// Each operation with the instant it was queued.
    ops: Sender<(Instant, IoOp)>,
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
        let handle = thread::Builder::new().name(name).spawn(move || {
            // A write waits on this thread: on the fast cores (H-049 of the native tests).
            let _ = teseladb_sys::favor_current_thread();
            run(disk, rx, (done, wrap), &sync_name)
        })?;
        Ok(DiskThread { ops: tx, handle: Some(handle) })
    }

    /// Queues `op` behind the ones already submitted. Returns it if the thread has stopped.
    pub fn submit(&self, op: IoOp) -> Result<(), IoOp> {
        self.ops.send((Instant::now(), op)).map_err(|e| e.0.1)
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
    ops: Receiver<(Instant, IoOp)>,
    (done, wrap): (MailboxSender<M>, fn(IoDone) -> M),
    sync_name: &str,
) -> D {
    let mut syncs: Option<Syncs> = None;
    let mut chores: Option<Chores> = None;
    for (queued, op) in ops {
        // How long it waited behind the operations before it (012 FR-015): a batch of the log
        // waits here while the thread syncs a file of the LSM.
        let waited = u64::try_from(queued.elapsed().as_nanos()).unwrap_or(u64::MAX);
        teseladb_stages::record(teseladb_stages::Stage::DiskQueueWait, waited);
        if let Some(job) = detach_chore(&disk, &op) {
            let helper = match &mut chores {
                Some(h) => h,
                None => match Chores::spawn(&format!("{sync_name}-chores"), done.clone(), wrap) {
                    Ok(h) => chores.insert(h),
                    // Without a third thread the chore runs here, in order.
                    Err(_) => {
                        if done.send_waiting(wrap(job.run())).is_err() {
                            break;
                        }
                        continue;
                    }
                },
            };
            helper.jobs.send(job).ok();
            continue;
        }
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
    // The syncs and chores handed over finish before the disk goes back.
    if let Some(h) = syncs {
        h.join();
    }
    if let Some(h) = chores {
        drop(h.jobs);
        let _ = h.handle.join();
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

/// A maintenance operation of a log as a job for the chores thread, when `disk` is a real one.
fn detach_chore<D: 'static>(disk: &D, op: &IoOp) -> Option<crate::log_chore::DetachedChore> {
    let any = disk as &dyn Any;
    if let Some(d) = any.downcast_ref::<NodeDisk>() {
        return d.detach_chore(op);
    }
    any.downcast_ref::<FileDisk>().and_then(|d| d.detach_chore(op))
}

/// The thread that runs the maintenance of a core's log, in order and away from its disk thread
/// ([`crate::log_chore`], H-049 of the native tests).
struct Chores {
    jobs: Sender<crate::log_chore::DetachedChore>,
    handle: JoinHandle<()>,
}

impl Chores {
    fn spawn<M: Send + 'static>(
        name: &str,
        done: MailboxSender<M>,
        wrap: fn(IoDone) -> M,
    ) -> io::Result<Chores> {
        let (jobs, rx) = mpsc::channel::<crate::log_chore::DetachedChore>();
        let handle = thread::Builder::new().name(name.to_owned()).spawn(move || {
            for job in rx {
                if done.send_waiting(wrap(job.run())).is_err() {
                    return;
                }
            }
        })?;
        Ok(Chores { jobs, handle })
    }
}

/// The thread that runs the syncs of a core's batches away from its disk thread. Each turn it
/// takes every sync waiting, waits for the flush of the drive that took each batch (or, for the
/// batches without a place in one, makes their files durable with one sync per file) and answers
/// each as it becomes durable (012 FR-014, FR-020, D-159).
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
            // Every confirmed write waits for a sync of this thread.
            let _ = teseladb_sys::favor_current_thread();
            while let Ok(first) = rx.recv() {
                let mut turn = vec![first];
                turn.extend(rx.try_iter());
                // Each answer leaves when its batch is durable: the ones after it in the turn
                // may be in a later flush of the drive (012 FR-020).
                let mut listened = true;
                DetachedSync::run_each(turn, |d| {
                    listened = done.send_waiting(wrap(d)).is_ok();
                    listened
                });
                if !listened {
                    return;
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
