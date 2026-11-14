//! The node's device log in the simulated world (012 FR-035, D-154): as on the node, every core's
//! WAL goes through its [`PortBook`](teseladb_devlog::port::PortBook) to one log per node, on its
//! own simulated disk, which writes what the cores asked as one record and makes it durable with
//! one sync. Its writer is two events of the world: a sync a core asks for waits for the next
//! flush, whose record is written after the disk's write latency and synced after its sync
//! latency, and its answer reaches the core as an event of its own. Crashes tear the log's last
//! unsynced write, syncs fail and the disk fills as the plan says; a full disk is tried again after
//! [`DISK_FULL_RETRY`], or sooner when a core asks again.

use teseladb_raftlog::{Completions, FileId, IoDone, IoOp};
use teseladb_types::Nanos;

use crate::world::{Event, World};

/// Size of each file of the simulated device log: small, so that files rotate, are copied forward
/// and go often.
pub const SIM_LOG_FILE_BYTES: u64 = 256 << 10;
/// How long the writer waits before trying again on a full disk (the node's writer waits 1 s).
pub const DISK_FULL_RETRY: Nanos = Nanos::from_secs(1);

impl World {
    /// Schedules the device log's next flush of `node` after the latency of a write of its disk,
    /// unless one comes sooner.
    pub(crate) fn schedule_log_flush(&mut self, node: u32) {
        let write = IoOp::Truncate { file: FileId(0), len: 0 };
        let lat = self.nodes[node as usize].log_disk.latency(&write, &mut self.rng);
        let now = self.now();
        let n = &mut self.nodes[node as usize];
        let at = now + Nanos(lat.0 * n.disk_slow);
        if n.log_flush_at.is_some_and(|t| t <= at) {
            return;
        }
        n.log_flush_at = Some(at);
        let life = n.life;
        self.schedule(at, Event::LogFlush { node, life, sync: false });
    }

    /// A step of the device log's flush of `node`: its write, then (`sync`) the sync that makes it
    /// durable and sends each core the answers of its syncs. An event of a flush that was moved or
    /// of an earlier life does nothing.
    pub fn on_log_flush(&mut self, node: u32, life: u64, sync: bool) {
        let now = self.now();
        let n = &mut self.nodes[node as usize];
        if !n.alive || n.life != life || n.log_flush_at != Some(now) {
            return;
        }
        n.log_flush_at = None;
        let log = n.log.as_mut().expect("a running node has its device log");
        let answers = if sync {
            log.flush(&mut n.log_disk)
        } else {
            // The record is on the disk and not durable yet: a crash now may tear it.
            log.write_ahead(&mut n.log_disk)
        };
        let waiting = log.has_waiting();
        for (core, done) in answers {
            self.schedule(now, Event::LogDone { node, core, life, done });
        }
        if !waiting {
            return;
        }
        let (at, sync) = if sync {
            // What waits on a full disk is tried again later.
            (now + DISK_FULL_RETRY, false)
        } else {
            let probe = IoOp::Sync { file: FileId(0), batch: 0 };
            let lat = self.nodes[node as usize].log_disk.latency(&probe, &mut self.rng);
            (now + Nanos(lat.0 * self.nodes[node as usize].disk_slow), true)
        };
        self.nodes[node as usize].log_flush_at = Some(at);
        self.schedule(at, Event::LogFlush { node, life, sync });
    }

    /// A core takes the answer of its sync from the device log (`None` if the event is from an
    /// earlier life of the node).
    pub fn on_log_done(
        &mut self,
        node: u32,
        core: u16,
        life: u64,
        done: IoDone,
    ) -> Option<Completions> {
        let n = &self.nodes[node as usize];
        if !n.alive || n.life != life {
            return None;
        }
        let local = self.local(node);
        Some(self.core_io_done(node, core, done, local))
    }
}
