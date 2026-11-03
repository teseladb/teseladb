//! When a core flushes its memtable without being full: once its ranges applied nothing for
//! [`IDLE_FLUSH`] while one of them holds its log back, or at once, once per snapshot, while a
//! snapshot someone waits for (a replica, or the split or replica change of its range) is held
//! back by the release floor.

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
    /// The mark of the waiting snapshots the memtable was flushed for
    /// ([`teseladb_multiraft::MultiRaftCore::snapshot_flush_mark`]); `None` while none waits.
    pub served: Option<u64>,
}

impl IdleFlush {
    /// The next state after a pass that saw `applied`, with a range holding its log back
    /// (`held`) and the mark of the snapshots waited for that the floor holds back (`awaits`),
    /// and whether to flush now.
    #[must_use]
    pub fn step(
        self,
        applied: u64,
        held: bool,
        awaits: Option<u64>,
        now: Nanos,
    ) -> (IdleFlush, bool) {
        // A snapshot waited for is flushed for once per mark, however busy the core: the replica
        // that needs it, or the split or replica change of its range, waits for it.
        let for_snapshot = awaits.is_some() && awaits != self.served;
        let mut next = IdleFlush { served: awaits, ..self };
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

    /// A snapshot waited for flushes the memtable at once, however busy the core is, once per
    /// mark; without one the memtable waits for [`IDLE_FLUSH`] of quiet (seed 2361 of the
    /// simulator's `search`: a core that applied something every second never was idle, and a
    /// range born from a split never got its snapshot to the replica that needed it).
    #[test]
    fn a_snapshot_waited_for_flushes_the_memtable_at_once() {
        let t0 = Nanos::from_secs(5);
        let busy = IdleFlush { seen: 7, since: t0, flushed: false, served: None };
        // Applied moved: no idle flush, but a waited snapshot flushes now.
        assert!(!busy.step(8, true, None, t0).1);
        let (next, flush) = busy.step(8, true, Some(1), t0);
        assert!(flush);
        // Once: the next pass waits for the flush to land and the snapshot to be saved.
        let (next, flush) = next.step(9, true, Some(1), t0);
        assert!(!flush);
        // Saved: a later wait flushes again.
        let (next, flush) = next.step(9, true, None, t0);
        assert!(!flush);
        assert!(next.step(10, true, Some(2), t0).1);
        // Quiet for IDLE_FLUSH with the log held: the idle flush, once.
        let quiet = IdleFlush { seen: 7, since: t0, flushed: false, served: None };
        assert!(!quiet.step(7, true, None, t0 + Nanos::from_millis(999)).1);
        let (quiet, flush) = quiet.step(7, true, None, t0 + IDLE_FLUSH);
        assert!(flush);
        assert!(!quiet.step(7, true, None, t0 + IDLE_FLUSH + IDLE_FLUSH).1);
        // Nothing held: nothing to flush.
        let empty = IdleFlush { seen: 7, since: t0, flushed: false, served: None };
        assert!(!empty.step(7, false, None, t0 + IDLE_FLUSH).1);
    }

    /// A snapshot that begins to wait while another still waits gets its own flush: on a loaded
    /// machine the core applies something every few hundred milliseconds and is never idle, and
    /// the flush made for the first one may not reach the second (`admin_port`'s
    /// `range_leaders_publish_themselves_and_spread_over_the_nodes`: a range born from a split
    /// that never split again).
    #[test]
    fn each_snapshot_that_begins_to_wait_gets_its_flush() {
        let t0 = Nanos::from_secs(5);
        let busy = IdleFlush { seen: 7, since: t0, flushed: false, served: None };
        let (next, flush) = busy.step(8, true, Some(4), t0);
        assert!(flush);
        let (next, flush) = next.step(9, true, Some(4), t0);
        assert!(!flush, "the same wait");
        let (next, flush) = next.step(10, true, Some(5), t0);
        assert!(flush, "another snapshot waits now, with no pass in between without one");
        assert!(!next.step(11, true, Some(5), t0).1);
    }
}
