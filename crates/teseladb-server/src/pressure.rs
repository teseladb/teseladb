//! What a core flushes for the node's sake (012 FR-018, FR-019). Every core has its own
//! memtables and its own Raft log, so what a node holds in memory and keeps of its log grew with
//! the cores that take writes: eleven cores with a memtable half full each held more than a
//! large node's worth, and each kept the log its memtable had not flushed. The cores share one
//! count of the bytes in their memtables; a core whose own are above its share of the node's
//! budget when the node is over it flushes them, and a core whose log is past its cap flushes
//! what holds the log back and takes the snapshots that let it go. The entries of a core's log
//! live in the core's memory until a snapshot covers them (023 FR-028): a core whose log holds
//! more than its part of that memory does the same for the groups that hold the most.
//!
//! The count is an atomic every core adds its change to: no core reads another's state, and none
//! waits (012 FR-008).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_types::Nanos;

/// The most the memtables of a node hold together before the cores above their share flush.
pub const MEMTABLES_BUDGET: u64 = 512 << 20;
/// The most the Raft logs of a node take on disk together, shared between its cores.
pub const LOG_BUDGET: u64 = 256 << 20;
/// Segments of its log a core may always keep: the active one, the one made ahead and two more,
/// so that a core is not flushed for the segments any log has.
pub const LOG_SEGMENTS_KEPT: u64 = 4;
/// The part of a core's memory its log may hold in entries: an eighth. The cap of a log on disk
/// let a core of any size keep 256 MiB of entries in memory, more than a small core has.
pub const LOG_MEMORY_PART: u64 = 8;
/// The least time between two flushes a core makes for the node: a flush takes a while to
/// free what it was made for, and the count is the same meanwhile.
pub const RELIEF_EVERY: Nanos = Nanos::from_secs(1);

/// What the cores of a node share.
#[derive(Debug)]
pub struct NodePressure {
    /// Bytes in the memtables of every core.
    memtables: AtomicU64,
    memtables_budget: u64,
    /// The cap of the log of each core.
    log_cap: u64,
    /// The most of its memory the log of each core holds in entries.
    log_memory_cap: u64,
    /// The share of the memtables' budget of each core.
    memtables_share: u64,
}

impl NodePressure {
    /// The pressure of a node of `cores` cores whose logs have segments of `segment` bytes, with
    /// `memtables` bytes for its memtables (the sum of every core's part, 012 FR-013) and
    /// `core_memory` bytes of memory in each core (`u64::MAX` when the node does not know it).
    pub fn new(cores: u16, segment: u64, memtables: u64, core_memory: u64) -> NodePressure {
        let cores = u64::from(cores.max(1));
        let memtables_budget = memtables.clamp(1, MEMTABLES_BUDGET);
        NodePressure {
            memtables: AtomicU64::new(0),
            memtables_budget,
            log_cap: (LOG_BUDGET / cores).max(LOG_SEGMENTS_KEPT.saturating_mul(segment)),
            log_memory_cap: match core_memory {
                u64::MAX => u64::MAX,
                known => (known / LOG_MEMORY_PART).max(1),
            },
            memtables_share: (memtables_budget / cores).max(1),
        }
    }

    /// Bytes in the memtables of every core, as last counted.
    pub fn memtables(&self) -> u64 {
        self.memtables.load(Ordering::Relaxed)
    }

    /// The cap of each core's log on disk.
    pub fn log_cap(&self) -> u64 {
        self.log_cap
    }

    /// The most bytes of entries each core's log holds in memory.
    pub fn log_memory_cap(&self) -> u64 {
        self.log_memory_cap
    }
}

/// What a core does for the node now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Relief {
    /// Flush its memtable.
    pub flush: bool,
    /// Take the snapshots of the groups that hold its log back.
    pub snapshots: bool,
    /// Take the snapshots of the groups that hold the most of its log in memory.
    pub entries: bool,
}

/// A core's part in the node's pressure.
#[derive(Debug)]
pub struct CorePressure {
    node: Arc<NodePressure>,
    /// The bytes of its memtables it last added to the node's count.
    counted: u64,
    /// When it last flushed for the node.
    relieved_at: Option<Nanos>,
}

impl CorePressure {
    /// The part of one core in `node`.
    pub fn new(node: Arc<NodePressure>) -> CorePressure {
        CorePressure { node, counted: 0, relieved_at: None }
    }

    /// The most bytes of entries the core's log holds in memory.
    pub fn log_memory_cap(&self) -> u64 {
        self.node.log_memory_cap
    }

    /// Counts the core's memtables (`memtables` bytes, of which `active` in the one that takes
    /// writes) and its log (`log` bytes on disk, `entries` bytes of entries in memory) at `now`,
    /// and says what it does for the node.
    pub fn step(
        &mut self,
        (memtables, active): (u64, u64),
        (log, entries): (u64, u64),
        now: Nanos,
    ) -> Relief {
        // Its change joins the node's count: an add or a subtract, never a store, so that two
        // cores counting at once lose nothing.
        if memtables >= self.counted {
            self.node.memtables.fetch_add(memtables - self.counted, Ordering::Relaxed);
        } else {
            self.node.memtables.fetch_sub(self.counted - memtables, Ordering::Relaxed);
        }
        self.counted = memtables;
        if self.relieved_at.is_some_and(|at| now.saturating_since(at) < RELIEF_EVERY) {
            return Relief::default();
        }
        let over_memory = self.node.memtables() > self.node.memtables_budget
            && active > self.node.memtables_share;
        let over_log = log > self.node.log_cap;
        let over_entries = entries > self.node.log_memory_cap;
        let relieve = over_memory || over_log || over_entries;
        if relieve {
            self.relieved_at = Some(now);
        }
        Relief { flush: relieve && active > 0, snapshots: over_log, entries: over_entries }
    }
}

impl Drop for CorePressure {
    fn drop(&mut self) {
        self.node.memtables.fetch_sub(self.counted, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn at(secs: u64) -> Nanos {
        Nanos::from_secs(secs)
    }

    #[test]
    fn the_cores_above_their_share_flush_once_the_node_is_over_its_budget() {
        // Four cores and 100 MiB for their memtables: 25 MiB each.
        let node = Arc::new(NodePressure::new(4, 8 * MIB, 100 * MIB, u64::MAX));
        let mut cores: Vec<CorePressure> =
            (0..4).map(|_| CorePressure::new(node.clone())).collect();
        // 90 MiB in all: under the budget, nobody flushes, however uneven.
        let none = Relief::default();
        assert_eq!(cores[0].step((60 * MIB, 60 * MIB), (0, 0), at(1)), none);
        assert_eq!(cores[1].step((30 * MIB, 30 * MIB), (0, 0), at(1)), none);
        assert_eq!(node.memtables(), 90 * MIB);
        // 110 MiB: the node is over. The core with 20 MiB is under its share and keeps them;
        // those above theirs flush.
        let flush = Relief { flush: true, snapshots: false, entries: false };
        assert_eq!(cores[2].step((20 * MIB, 20 * MIB), (0, 0), at(2)), none);
        assert_eq!(cores[0].step((60 * MIB, 60 * MIB), (0, 0), at(2)), flush);
        assert_eq!(cores[1].step((30 * MIB, 30 * MIB), (0, 0), at(2)), flush);
        // A frozen memtable still counts while it flushes, and the core does not flush twice
        // in a second whatever it holds.
        assert_eq!(cores[0].step((60 * MIB, MIB), (0, 0), at(2)), none);
        assert_eq!(cores[0].step((60 * MIB, 40 * MIB), (0, 0), at(2)), none);
        // Flushed: the node is under its budget again.
        assert_eq!(cores[0].step((MIB, MIB), (0, 0), at(4)), none);
        assert_eq!(cores[1].step((0, 0), (0, 0), at(4)), none);
        assert_eq!(node.memtables(), 21 * MIB);
        // A core that goes takes its count with it.
        drop(cores);
        assert_eq!(node.memtables(), 0);
    }

    #[test]
    fn a_core_whose_log_is_past_its_cap_flushes_and_takes_snapshots() {
        // Eighteen cores: the node's 256 MiB give each less than the four segments any core
        // keeps, so the cap is those four.
        let node = Arc::new(NodePressure::new(18, 8 * MIB, 4096 * MIB, u64::MAX));
        assert_eq!(node.log_cap(), 32 * MIB);
        let mut core = CorePressure::new(node.clone());
        assert_eq!(core.step((5 * MIB, 5 * MIB), (32 * MIB, 0), at(1)), Relief::default());
        let both = Relief { flush: true, snapshots: true, entries: false };
        assert_eq!(core.step((5 * MIB, 5 * MIB), (40 * MIB, 0), at(1)), both);
        // Not again within a second; then again while the log is still past its cap. With an
        // empty memtable only the snapshots are left to take.
        assert_eq!(core.step((5 * MIB, 5 * MIB), (40 * MIB, 0), at(1)), Relief::default());
        let only = Relief { flush: false, snapshots: true, entries: false };
        assert_eq!(core.step((0, 0), (40 * MIB, 0), at(3)), only);
        // Two cores share the node's budget when it gives each more than four segments.
        assert_eq!(NodePressure::new(2, 8 * MIB, 1, u64::MAX).log_cap(), 128 * MIB);
    }

    #[test]
    fn a_core_whose_log_holds_more_of_its_memory_than_its_part_lets_entries_go() {
        // A core of 256 MiB: its log may hold 32 MiB of entries, with its four segments of
        // 64 MiB on disk well within their cap.
        let node = Arc::new(NodePressure::new(1, 64 * MIB, 51 * MIB, 256 * MIB));
        assert_eq!((node.log_cap(), node.log_memory_cap()), (256 * MIB, 32 * MIB));
        let mut core = CorePressure::new(node);
        assert_eq!(core.log_memory_cap(), 32 * MIB);
        let disk = 128 * MIB;
        assert_eq!(core.step((5 * MIB, 5 * MIB), (disk, 32 * MIB), at(1)), Relief::default());
        let entries = Relief { flush: true, snapshots: false, entries: true };
        assert_eq!(core.step((5 * MIB, 5 * MIB), (disk, 33 * MIB), at(1)), entries);
        // Not again within a second; then again while the entries are still past their part,
        // and with nothing in the memtable only the snapshots are left to take.
        assert_eq!(core.step((5 * MIB, 5 * MIB), (disk, 40 * MIB), at(1)), Relief::default());
        let only = Relief { flush: false, snapshots: false, entries: true };
        assert_eq!(core.step((0, 0), (disk, 40 * MIB), at(3)), only);
        // A core of 2 GiB holds as much of its log in memory as its cap on disk lets it keep.
        assert_eq!(NodePressure::new(12, 64 * MIB, 1, 2048 * MIB).log_memory_cap(), 256 * MIB);
        // A node that does not know its memory never asks for it.
        let unknown = Arc::new(NodePressure::new(1, 64 * MIB, 1, u64::MAX));
        let mut core = CorePressure::new(unknown);
        assert_eq!(core.step((0, 0), (disk, 4096 * MIB), at(1)), Relief::default());
    }
}
