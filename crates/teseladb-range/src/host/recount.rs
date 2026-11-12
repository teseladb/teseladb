//! The recount of a range's live documents (007 FR-039, H-004): while the counts of a range it
//! leads are estimated, the leader counts the documents of each collection at the entry it has
//! applied and proposes `AdjustCounts`, at most once every [`RECOUNT_EVERY`]. Each replica adds
//! the counted writes that followed and takes the counts as exact only if nothing after that
//! entry left them estimated.

use super::*;
use crate::doc_count::{DocScan, Recount};

/// The least time between two recounts of a range.
pub(crate) const RECOUNT_EVERY: Nanos = Nanos::from_millis(1_000);

/// A leader's recount: idle until a time, or counting at an entry.
#[derive(Default)]
pub(crate) enum RecountJob {
    /// Nothing until then.
    #[default]
    Idle,
    /// Waiting until it may count again.
    Until(Nanos),
    /// Counting the documents the range held at entry `base`.
    Counting { base: u64, scan: Box<DocScan> },
}

impl RangeHost {
    /// Moves the recount of range `g` on; `true` if it proposed.
    pub(super) fn recount_step<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let dirty = core.state_machine(g).is_some_and(RangeState::counts_dirty);
        let applied = core.state_machine(g).map_or(0, StateMachine::applied_index);
        let lsm = self.lsm.clone();
        let Some(r) = self.ranges.get_mut(&g) else { return false };
        let (start, end) = (r.start.clone(), r.end.clone());
        let Some(l) = r.leader.as_mut() else { return false };
        // As a write, nothing before the leader applied what earlier terms left in its log.
        if !self.counted_writes || !dirty || !l.caught_up(core, g) {
            l.recount = RecountJob::Idle;
            return false;
        }
        if let RecountJob::Until(at) = l.recount
            && now < at
        {
            return false;
        }
        if !matches!(l.recount, RecountJob::Counting { .. }) {
            // The view and the entry are taken together: nothing applies in between.
            let scan = Box::new(DocScan::new(&lsm, &start, &end));
            l.recount = RecountJob::Counting { base: applied, scan };
        }
        let RecountJob::Counting { base, scan } = &mut l.recount else { return false };
        let base = *base;
        let Some(done) = scan.step(&lsm) else { return false };
        l.recount = RecountJob::Until(now + RECOUNT_EVERY);
        let Recount::Counted(docs) = done else { return false };
        let counts = docs.into_iter().collect();
        let payload = Command::AdjustCounts { base, counts }.encode();
        let zero = Hlc { physical: 0, logical: 0 };
        core.propose_at(g, zero, now, |_| Ok::<_, std::convert::Infallible>(payload)).is_ok()
    }
}
