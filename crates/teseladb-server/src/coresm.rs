//! The state machine of a core's groups (005 FR-015, FR-016): one `MultiRaftCore` per core runs
//! one state machine type, so a core that holds Group 0 and a data range runs [`CoreSm`], with a
//! variant for each; core 0 also runs the `Meta2` ranges (007 FR-014).

use std::fmt;

use teseladb_hlc::Hlc;
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_meta::meta2::Meta2Sm;
use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_range::sm::{RangeOutput, RangeSm, RangeState};

/// The mark of a group that has no GC threshold.
const ZERO: Hlc = Hlc { physical: 0, logical: 0 };

/// What gives Group 0's state, if the state machine is Group 0.
pub trait Group0Holder {
    /// Group 0's state.
    fn group0(&self) -> Option<&Group0Sm>;
}

impl Group0Holder for Group0Sm {
    fn group0(&self) -> Option<&Group0Sm> {
        Some(self)
    }
}

/// The state machine of a group of this node.
pub enum CoreSm {
    /// Group 0.
    Group0(Group0Sm),
    /// A data range over its core's LSM.
    Range(RangeSm),
    /// A `Meta2` range: descriptors Group 0 handed over (core 0, 007 FR-014).
    Meta2(Box<Meta2Sm>),
}

impl fmt::Debug for CoreSm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreSm::Group0(_) => f.write_str("Group0"),
            CoreSm::Range(_) => f.write_str("Range"),
            CoreSm::Meta2(_) => f.write_str("Meta2"),
        }
    }
}

impl CoreSm {
    /// The range's state, if this is a range.
    pub fn range(&self) -> Option<&RangeSm> {
        match self {
            CoreSm::Range(sm) => Some(sm),
            CoreSm::Group0(_) | CoreSm::Meta2(_) => None,
        }
    }

    /// The range's state to change, if this is a range.
    pub fn range_mut(&mut self) -> Option<&mut RangeSm> {
        match self {
            CoreSm::Range(sm) => Some(sm),
            CoreSm::Group0(_) | CoreSm::Meta2(_) => None,
        }
    }

    /// The `Meta2` range's state, if this is one.
    pub fn meta2(&self) -> Option<&Meta2Sm> {
        match self {
            CoreSm::Meta2(sm) => Some(sm),
            CoreSm::Group0(_) | CoreSm::Range(_) => None,
        }
    }
}

impl Group0Holder for CoreSm {
    fn group0(&self) -> Option<&Group0Sm> {
        match self {
            CoreSm::Group0(sm) => Some(sm),
            CoreSm::Range(_) | CoreSm::Meta2(_) => None,
        }
    }
}

/// What applying an entry gives whoever proposed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreOutput {
    /// Of Group 0.
    Group0(Group0Result),
    /// Of a range.
    Range(RangeOutput),
}

impl RangeState for CoreSm {
    fn gc_threshold(&self) -> Hlc {
        self.range().map_or(ZERO, RangeState::gc_threshold)
    }

    fn counts_dirty(&self) -> bool {
        self.range().is_some_and(RangeState::counts_dirty)
    }

    fn txn_mark(&self) -> u64 {
        self.range().map_or(0, RangeState::txn_mark)
    }

    fn shared_counts(&self) -> Option<std::sync::Arc<teseladb_range::doc_count::Counts>> {
        self.range().and_then(RangeState::shared_counts)
    }

    fn checksum_request(&self) -> Option<(u64, Hlc)> {
        self.range().and_then(RangeState::checksum_request)
    }

    fn end(&self) -> Option<&[u8]> {
        self.range().and_then(RangeState::end)
    }

    fn frozen(&self) -> bool {
        self.range().is_some_and(RangeState::frozen)
    }

    fn fenced(&self, key: &[u8]) -> bool {
        self.range().is_some_and(|r| r.fenced(key))
    }

    fn closed_mark(&self) -> Option<(Hlc, u64)> {
        self.range().and_then(RangeState::closed_mark)
    }
}

impl teseladb_range::host::FenceOutcome for CoreOutput {
    fn range_output(&self) -> Option<&RangeOutput> {
        match self {
            CoreOutput::Range(o) => Some(o),
            CoreOutput::Group0(_) => None,
        }
    }
}

impl StateMachine for CoreSm {
    type Output = CoreOutput;

    // A range holds the log until its LSM flushes; Group 0 answers `None`.
    const HOLDS_LOG: bool = true;

    fn release_floor(&self) -> Option<u64> {
        self.range().and_then(StateMachine::release_floor)
    }

    fn read_cache_margin_ms(&self) -> Option<u64> {
        self.range().and_then(StateMachine::read_cache_margin_ms)
    }

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<CoreOutput, ApplyError> {
        match self {
            CoreSm::Group0(sm) => sm.apply(payload, ctx).map(CoreOutput::Group0),
            CoreSm::Range(sm) => sm.apply(payload, ctx).map(CoreOutput::Range),
            CoreSm::Meta2(sm) => sm.apply(payload, ctx).map(CoreOutput::Group0),
        }
    }

    fn skip(&mut self, index: u64) {
        match self {
            CoreSm::Group0(sm) => sm.skip(index),
            CoreSm::Range(sm) => sm.skip(index),
            CoreSm::Meta2(sm) => sm.skip(index),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        match self {
            CoreSm::Group0(sm) => sm.snapshot(),
            CoreSm::Range(sm) => sm.snapshot(),
            CoreSm::Meta2(sm) => sm.snapshot(),
        }
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        match self {
            CoreSm::Group0(sm) => sm.restore(snap),
            CoreSm::Range(sm) => sm.restore(snap),
            CoreSm::Meta2(sm) => sm.restore(snap),
        }
    }

    fn has_data_for(&self, snap: &[u8]) -> bool {
        self.range().is_none_or(|sm| sm.has_data_for(snap))
    }

    fn snapshot_floor(&self) -> u64 {
        self.range().map_or(0, StateMachine::snapshot_floor)
    }

    fn log_floor(&self) -> u64 {
        self.range().map_or(0, StateMachine::log_floor)
    }

    fn snapshot_state_index(&self) -> u64 {
        match self {
            CoreSm::Group0(sm) => sm.snapshot_state_index(),
            CoreSm::Range(sm) => sm.snapshot_state_index(),
            CoreSm::Meta2(sm) => sm.snapshot_state_index(),
        }
    }

    fn needs_snapshot(&self) -> bool {
        self.range().is_some_and(StateMachine::needs_snapshot)
    }

    fn applied_index(&self) -> u64 {
        match self {
            CoreSm::Group0(sm) => sm.applied_index(),
            CoreSm::Range(sm) => sm.applied_index(),
            CoreSm::Meta2(sm) => sm.applied_index(),
        }
    }

    fn halted(&self) -> bool {
        match self {
            CoreSm::Group0(_) | CoreSm::Meta2(_) => false,
            CoreSm::Range(sm) => StateMachine::halted(sm),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use teseladb_lsm::{Lsm, LsmConfig};
    use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START};
    use teseladb_raftlog::MemDisk;

    use super::*;

    fn range() -> CoreSm {
        let mut disk = MemDisk::new();
        let (lsm, _) = Lsm::recover(&LsmConfig::default(), &mut disk).unwrap();
        let lsm = Rc::new(RefCell::new(lsm));
        let sm = RangeSm::open(DATA_RANGE, DATA_START.to_vec(), DATA_END.to_vec(), lsm, &mut disk);
        CoreSm::Range(sm.unwrap())
    }

    #[test]
    fn group0_has_no_range_state() {
        let mut g = CoreSm::Group0(Group0Sm::default());
        assert_eq!(format!("{g:?}"), "Group0");
        assert!(g.group0().is_some() && g.range().is_none());
        assert_eq!((g.gc_threshold(), g.checksum_request(), g.release_floor()), (ZERO, None, None));
        assert!(g.has_data_for(&[]));
        let snap = g.snapshot();
        g.restore(&snap).unwrap();
        g.skip(4);
        assert_eq!((g.applied_index(), g.snapshot_state_index()), (4, 4));
    }

    #[test]
    fn a_meta2_range_answers_with_its_own_state_and_no_range_state() {
        let mut m = CoreSm::Meta2(Box::new(Meta2Sm::new(teseladb_types::GroupId(9))));
        assert_eq!(format!("{m:?}"), "Meta2");
        assert!(m.meta2().is_some() && m.group0().is_none() && m.range().is_none());
        assert!(!m.halted() && m.has_data_for(&[]));
        let snap = m.snapshot();
        m.restore(&snap).unwrap();
        m.skip(5);
        assert_eq!((m.applied_index(), m.snapshot_state_index()), (5, 5));
        let seed = teseladb_meta::meta2::Meta2Command::Seed(snap).encode();
        let ctx = ApplyCtx { group: teseladb_types::GroupId(9), index: 6, term: 1, ts: ZERO };
        assert!(matches!(m.apply(&seed, &ctx), Ok(CoreOutput::Group0(_))));
        assert!(CoreSm::Group0(Group0Sm::default()).meta2().is_none() && range().meta2().is_none());
    }

    #[test]
    fn a_range_answers_with_its_own_state() {
        let mut r = range();
        assert_eq!(format!("{r:?}"), "Range");
        assert!(r.group0().is_none() && r.range().is_some());
        assert_eq!((r.gc_threshold(), r.checksum_request(), r.release_floor()), (ZERO, None, None));
        let snap = r.snapshot();
        assert!(r.has_data_for(&snap));
        r.restore(&snap).unwrap();
        r.skip(3);
        assert_eq!((r.applied_index(), r.snapshot_state_index()), (3, 3));
        // An entry it cannot decode stops the group.
        let ctx = ApplyCtx { group: DATA_RANGE, index: 4, term: 1, ts: ZERO };
        assert!(r.apply(&[0xff, 0xff], &ctx).is_err());
    }

    #[test]
    fn a_range_that_moved_to_another_core_is_halted_for_the_multi_raft_core() {
        let mut r = range();
        assert!(!r.halted() && !CoreSm::Group0(Group0Sm::default()).halted());
        let CoreSm::Range(sm) = &mut r else { unreachable!() };
        sm.set_core(0);
        let ctx = ApplyCtx { group: DATA_RANGE, index: 1, term: 1, ts: ZERO };
        let relocate = teseladb_range::command::Command::Relocate { core: 1, intent: 7 };
        assert!(r.apply(&relocate.encode(), &ctx).is_ok());
        assert!(r.halted());
    }
}
