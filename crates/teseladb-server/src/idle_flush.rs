//! When a core flushes its memtable without being full: once its ranges applied nothing for
//! [`IDLE_FLUSH`] while one of them holds its log back; at once, once per snapshot, while a
//! snapshot someone waits for (a replica, or the split or replica change of its range) is held
//! back by the release floor; and every [`CHECKPOINT_EVERY`] while a range holds its log back,
//! however busy the core is (a core that never goes quiet kept its whole WAL, 001 FR-005).

use teseladb_types::Nanos;

use crate::data::{CHECKPOINT_EVERY, IDLE_FLUSH};

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
    /// When it last flushed.
    pub flushed_at: Nanos,
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
        // What the memtable holds back of the log is flushed at least this often, as the reference
        // storage engine checkpoints every 60 s so that the reference server can remove its old
        // journal files: a core that always applies something is never idle.
        let checkpoint = held && now.saturating_since(self.flushed_at) >= CHECKPOINT_EVERY;
        let mut next = IdleFlush { served: awaits, ..self };
        let flush = if applied == self.seen {
            let idle = !self.flushed && held && now.saturating_since(self.since) >= IDLE_FLUSH;
            if idle {
                next.flushed = true;
            }
            idle || for_snapshot || checkpoint
        } else {
            next = IdleFlush { seen: applied, since: now, flushed: false, ..next };
            for_snapshot || checkpoint
        };
        if flush {
            next.flushed_at = now;
        }
        (next, flush)
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
        let busy = IdleFlush { seen: 7, since: t0, flushed: false, served: None, flushed_at: t0 };
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
        let quiet = IdleFlush { seen: 7, since: t0, flushed: false, served: None, flushed_at: t0 };
        assert!(!quiet.step(7, true, None, t0 + Nanos::from_millis(999)).1);
        let (quiet, flush) = quiet.step(7, true, None, t0 + IDLE_FLUSH);
        assert!(flush);
        assert!(!quiet.step(7, true, None, t0 + IDLE_FLUSH + IDLE_FLUSH).1);
        // Nothing held: nothing to flush.
        let empty = IdleFlush { seen: 7, since: t0, flushed: false, served: None, flushed_at: t0 };
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
        let busy = IdleFlush { seen: 7, since: t0, flushed: false, served: None, flushed_at: t0 };
        let (next, flush) = busy.step(8, true, Some(4), t0);
        assert!(flush);
        let (next, flush) = next.step(9, true, Some(4), t0);
        assert!(!flush, "the same wait");
        let (next, flush) = next.step(10, true, Some(5), t0);
        assert!(flush, "another snapshot waits now, with no pass in between without one");
        assert!(!next.step(11, true, Some(5), t0).1);
    }

    /// A core that applies something every few seconds is never idle: what its memtable holds
    /// back of the log is flushed every `CHECKPOINT_EVERY` (production, 10-11-2026: 6,4 GB of WAL
    /// never released under a job queue that wrote all day).
    #[test]
    fn a_busy_core_flushes_what_holds_its_log_every_checkpoint() {
        let t0 = Nanos::from_secs(5);
        let mut s = IdleFlush { seen: 0, since: t0, flushed: false, served: None, flushed_at: t0 };
        let mut flushes = Vec::new();
        for second in 1..=130u64 {
            let now = t0 + Nanos::from_secs(second);
            let (next, flush) = s.step(second, true, None, now);
            if flush {
                flushes.push(second);
            }
            s = next;
        }
        assert_eq!(flushes, [60, 120], "once a minute while it holds the log back");
        // Nothing held back: no flush, however long.
        let free = IdleFlush { seen: 0, since: t0, flushed: false, served: None, flushed_at: t0 };
        assert!(!free.step(1, false, None, t0 + Nanos::from_secs(600)).1);
    }
}
