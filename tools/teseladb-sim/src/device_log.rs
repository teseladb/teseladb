//! The node's device log in the simulated world (012 FR-035, D-154): as on the node, every core's
//! WAL goes through its [`PortBook`](teseladb_devlog::port::PortBook) to one log per node, on its
//! own simulated disk, and its writer works in the same pipeline. A sync a core asks for makes the
//! writer take what waits after the disk's write latency and write it; the group's sync ends after
//! the disk's sync latency, and runs on the disk then. What the cores ask while it runs is written
//! at once and goes in the next group's sync, which starts as that one ends. Each answer reaches
//! its core as an event of its own. Crashes tear the log's unsynced writes, syncs fail and the disk
//! fills as the plan says; a full disk is tried again after [`DISK_FULL_RETRY`], or sooner when a
//! core asks again.

use teseladb_raftlog::{Completions, Disk, FileId, IoDone, IoOp};
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

    /// A step of the device log's writer of `node`: with `sync`, the group's sync ends and runs on
    /// the disk; then the writer takes its next step and sends each core the answers of its
    /// syncs. An event that was moved or of an earlier life does nothing.
    pub fn on_log_flush(&mut self, node: u32, life: u64, sync: bool) {
        let now = self.now();
        let n = &mut self.nodes[node as usize];
        let slot = if sync { &mut n.log_sync_at } else { &mut n.log_flush_at };
        if !n.alive || n.life != life || *slot != Some(now) {
            return;
        }
        *slot = None;
        let log = n.log.as_mut().expect("a running node has its device log");
        let mut answers = Vec::new();
        if sync {
            for op in std::mem::take(&mut n.log_syncs) {
                let done = n.log_disk.execute(op);
                answers.extend(log.on_done(&done));
            }
        }
        let mut ops = Vec::new();
        answers.extend(log.next(&mut n.log_disk, &mut ops));
        for op in ops {
            if matches!(op, IoOp::Sync { .. }) {
                n.log_syncs.push(op);
                continue;
            }
            // The record is on the disk and not durable yet: a crash now may tear it.
            let done = n.log_disk.execute(op);
            answers.extend(log.on_done(&done));
        }
        let (syncing, waiting, full) = (log.syncing(), log.has_waiting(), log.disk_full());
        for (core, done) in answers {
            self.schedule(now, Event::LogDone { node, core, life, done });
        }
        let n = &self.nodes[node as usize];
        if syncing && n.log_sync_at.is_none() {
            let probe = IoOp::Sync { file: FileId(0), batch: 0 };
            let lat = n.log_disk.latency(&probe, &mut self.rng);
            let at = now + Nanos(lat.0 * n.disk_slow);
            self.nodes[node as usize].log_sync_at = Some(at);
            self.schedule(at, Event::LogFlush { node, life, sync: true });
        } else if !syncing && waiting && full && n.log_flush_at.is_none() {
            // What waits on a full disk is tried again later.
            let at = now + DISK_FULL_RETRY;
            self.nodes[node as usize].log_flush_at = Some(at);
            self.schedule(at, Event::LogFlush { node, life, sync: false });
        }
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
