//! When a core flushes its memtable without being full: once its ranges applied nothing for
//! [`IDLE_FLUSH`] while one of them holds its log back, or at once while a snapshot a replica
//! waits for is held back by the release floor.

use teseladb_types::Nanos;

use crate::data::IDLE_FLUSH;

/// The state of a core's flushes that the memtable's size does not ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleFlush {
    /// The last applied index seen (summed over the ranges).
    pub seen: u64,
    /// Since when it has not moved.
    pub since: Nanos,
    /// The memtable was flushed for this quiet spell.
    pub flushed: bool,
    /// The memtable was flushed for the snapshot that is waiting now.
    pub owed: bool,
}

impl IdleFlush {
    /// The next state after a pass that saw `applied`, with a range holding its log back
    /// (`held`) and a snapshot a replica waits for held back by the floor (`awaits`), and whether
    /// to flush now.
    #[must_use]
    pub fn step(self, applied: u64, held: bool, awaits: bool, now: Nanos) -> (IdleFlush, bool) {
        // A snapshot a replica waits for is flushed for once per wait, however busy the core:
        // the replica may be what its group needs to commit anything.
        let for_snapshot = awaits && !self.owed;
        let mut next = IdleFlush { owed: awaits, ..self };
        if applied != self.seen {
            next = IdleFlush { seen: applied, since: now, flushed: false, ..next };
            return (next, for_snapshot);
        }
        let idle = !self.flushed && held && now.saturating_since(self.since) >= IDLE_FLUSH;
        if idle {
            next.flushed = true;
        }
        (next, idle || for_snapshot)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A snapshot a replica waits for flushes the memtable at once, however busy the core is,
    /// once per wait; without one the memtable waits for [`IDLE_FLUSH`] of quiet (seed 2361 of
    /// the simulator's `search`: a core that applied something every second never was idle, and a
    /// range born from a split never got its snapshot to the replica that needed it).
    #[test]
    fn a_snapshot_a_replica_waits_for_flushes_the_memtable_at_once() {
        let t0 = Nanos::from_secs(5);
        let busy = IdleFlush { seen: 7, since: t0, flushed: false, owed: false };
        // Applied moved: no idle flush, but an owed snapshot flushes now.
        assert!(!busy.step(8, true, false, t0).1);
        let (next, flush) = busy.step(8, true, true, t0);
        assert!(flush);
        // Once: the next pass waits for the flush to land and the snapshot to be saved.
        let (next, flush) = next.step(9, true, true, t0);
        assert!(!flush);
        // Saved: a later wait flushes again.
        let (next, flush) = next.step(9, true, false, t0);
        assert!(!flush);
        assert!(next.step(10, true, true, t0).1);
        // Quiet for IDLE_FLUSH with the log held: the idle flush, once.
        let quiet = IdleFlush { seen: 7, since: t0, flushed: false, owed: false };
        assert!(!quiet.step(7, true, false, t0 + Nanos::from_millis(999)).1);
        let (quiet, flush) = quiet.step(7, true, false, t0 + IDLE_FLUSH);
        assert!(flush);
        assert!(!quiet.step(7, true, false, t0 + IDLE_FLUSH + IDLE_FLUSH).1);
        // Nothing held: nothing to flush.
        let empty = IdleFlush { seen: 7, since: t0, flushed: false, owed: false };
        assert!(!empty.step(7, false, false, t0 + IDLE_FLUSH).1);
    }
}
