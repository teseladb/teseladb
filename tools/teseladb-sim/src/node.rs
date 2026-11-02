//! The simulated node's state machines (002 T002, research R1b): one `MultiRaftCore` per core, whose
//! state machine is [`NodeSm`] with a variant per machine: Group 0, the `testkv` groups, the ranges
//! of 005 (over their core's LSM, which the world opens) and those that later specifications
//! register by name in a [`Registry`].

use std::collections::BTreeMap;
use std::fmt;

use teseladb_keys::Hlc;
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_range::sm::{RangeOutput, RangeSm, RangeState};
use teseladb_testkv::{KvOutput, TestKv};
use teseladb_types::Bytes;

/// A state machine that a specification registers (005 to 015): its output is bytes for the
/// workload that proposed the entry.
pub trait RegisteredSm {
    /// Applies a committed entry.
    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Bytes, ApplyError>;
    /// An entry with nothing for the state machine was committed at `index`.
    fn skip(&mut self, index: u64);
    /// The whole state.
    fn snapshot(&self) -> Vec<u8>;
    /// Replaces the state with a snapshot.
    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError>;
    /// Index of the last entry applied or skipped.
    fn applied_index(&self) -> u64;
    /// Its state, for a workload that reads it (007: a Meta2 range).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

/// Builds an empty registered state machine.
pub type Factory = fn() -> Box<dyn RegisteredSm>;

/// What runs a group of the simulated cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmKind {
    /// Group 0.
    Group0,
    /// A `testkv` group.
    Kv,
    /// A range of 005 over `[start, end)`; the world builds it over its core's LSM.
    Range(&'static [u8], &'static [u8]),
    /// A state machine registered under this name.
    Registered(&'static str),
}

/// State machines registered by name.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    kinds: BTreeMap<&'static str, Factory>,
}

impl Registry {
    /// Registers `factory` under `name`; a name registers once.
    pub fn register(&mut self, name: &'static str, factory: Factory) -> Result<(), String> {
        if self.kinds.contains_key(name) {
            return Err(format!("state machine {name} is already registered"));
        }
        self.kinds.insert(name, factory);
        Ok(())
    }

    /// Registered names, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.kinds.keys().copied().collect()
    }

    /// An empty state machine of `kind`; `None` for a name nobody registered, and for a range,
    /// which needs its core's LSM (`World` builds it).
    pub fn make(&self, kind: SmKind) -> Option<NodeSm> {
        Some(match kind {
            SmKind::Group0 => NodeSm::Group0(Group0Sm::default()),
            SmKind::Kv => NodeSm::Kv(TestKv::default()),
            SmKind::Range(..) => return None,
            SmKind::Registered(name) => NodeSm::Registered(self.kinds.get(name)?()),
        })
    }
}

/// The state machine of a group of the simulated node.
pub enum NodeSm {
    /// Group 0.
    Group0(Group0Sm),
    /// A `testkv` group.
    Kv(TestKv),
    /// A range of 005.
    Range(RangeSm),
    /// A registered state machine.
    Registered(Box<dyn RegisteredSm>),
}

impl fmt::Debug for NodeSm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeSm::Group0(_) => f.write_str("Group0"),
            NodeSm::Kv(_) => f.write_str("Kv"),
            NodeSm::Range(_) => f.write_str("Range"),
            NodeSm::Registered(_) => f.write_str("Registered"),
        }
    }
}

impl NodeSm {
    /// Group 0's state, if this is Group 0.
    pub fn group0(&self) -> Option<&Group0Sm> {
        match self {
            NodeSm::Group0(sm) => Some(sm),
            _ => None,
        }
    }

    /// The `testkv` state, if this is a `testkv` group.
    pub fn kv(&self) -> Option<&TestKv> {
        match self {
            NodeSm::Kv(sm) => Some(sm),
            _ => None,
        }
    }

    /// A registered state machine's state of type `T`, if it is one and shows it.
    pub fn registered<T: 'static>(&self) -> Option<&T> {
        match self {
            NodeSm::Registered(sm) => sm.as_any()?.downcast_ref(),
            _ => None,
        }
    }
}

/// What applying an entry gives to whoever proposed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeOutput {
    /// Of Group 0.
    Group0(Group0Result),
    /// Of a `testkv` group.
    Kv(KvOutput),
    /// Of a range.
    Range(RangeOutput),
    /// Of a registered state machine.
    Bytes(Bytes),
}

impl StateMachine for NodeSm {
    type Output = NodeOutput;

    // The ranges hold the log until their LSM flushes; the other machines answer `None`.
    const HOLDS_LOG: bool = true;

    fn release_floor(&self) -> Option<u64> {
        match self {
            NodeSm::Range(sm) => sm.release_floor(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => None,
        }
    }

    fn read_cache_margin_ms(&self) -> Option<u64> {
        match self {
            NodeSm::Range(sm) => sm.read_cache_margin_ms(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => None,
        }
    }

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<NodeOutput, ApplyError> {
        match self {
            NodeSm::Group0(sm) => sm.apply(payload, ctx).map(NodeOutput::Group0),
            NodeSm::Kv(sm) => sm.apply(payload, ctx).map(NodeOutput::Kv),
            NodeSm::Range(sm) => sm.apply(payload, ctx).map(NodeOutput::Range),
            NodeSm::Registered(sm) => sm.apply(payload, ctx).map(NodeOutput::Bytes),
        }
    }

    fn skip(&mut self, index: u64) {
        match self {
            NodeSm::Group0(sm) => sm.skip(index),
            NodeSm::Kv(sm) => sm.skip(index),
            NodeSm::Range(sm) => sm.skip(index),
            NodeSm::Registered(sm) => sm.skip(index),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        match self {
            NodeSm::Group0(sm) => sm.snapshot(),
            NodeSm::Kv(sm) => sm.snapshot(),
            NodeSm::Range(sm) => sm.snapshot(),
            NodeSm::Registered(sm) => sm.snapshot(),
        }
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        match self {
            NodeSm::Group0(sm) => sm.restore(snap),
            NodeSm::Kv(sm) => sm.restore(snap),
            NodeSm::Range(sm) => sm.restore(snap),
            NodeSm::Registered(sm) => sm.restore(snap),
        }
    }

    fn applied_index(&self) -> u64 {
        match self {
            NodeSm::Group0(sm) => sm.applied_index(),
            NodeSm::Kv(sm) => sm.applied_index(),
            NodeSm::Range(sm) => sm.applied_index(),
            NodeSm::Registered(sm) => sm.applied_index(),
        }
    }

    fn halted(&self) -> bool {
        match self {
            NodeSm::Range(sm) => StateMachine::halted(sm),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => false,
        }
    }

    fn has_data_for(&self, snap: &[u8]) -> bool {
        match self {
            NodeSm::Range(sm) => sm.has_data_for(snap),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => true,
        }
    }

    fn snapshot_floor(&self) -> u64 {
        match self {
            NodeSm::Range(sm) => sm.snapshot_floor(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => 0,
        }
    }

    fn snapshot_state_index(&self) -> u64 {
        match self {
            NodeSm::Range(sm) => sm.snapshot_state_index(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => self.applied_index(),
        }
    }

    fn needs_snapshot(&self) -> bool {
        match self {
            NodeSm::Range(sm) => sm.needs_snapshot(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => false,
        }
    }
}

/// What a range's host follows (005 FR-013, FR-014); the other machines are no range of a host and
/// have no threshold or checksum.
impl RangeState for NodeSm {
    fn gc_threshold(&self) -> Hlc {
        match self {
            NodeSm::Range(sm) => sm.gc_threshold(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => {
                Hlc { physical: 0, logical: 0 }
            }
        }
    }

    fn checksum_request(&self) -> Option<(u64, Hlc)> {
        match self {
            NodeSm::Range(sm) => sm.checksum_request(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => None,
        }
    }

    fn end(&self) -> Option<&[u8]> {
        match self {
            NodeSm::Range(sm) => sm.end(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => None,
        }
    }

    fn frozen(&self) -> bool {
        match self {
            NodeSm::Range(sm) => sm.frozen(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => false,
        }
    }

    fn fenced(&self, key: &[u8]) -> bool {
        match self {
            NodeSm::Range(sm) => sm.fenced(key),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => false,
        }
    }

    fn closed_mark(&self) -> Option<(Hlc, u64)> {
        match self {
            NodeSm::Range(sm) => sm.closed_mark(),
            NodeSm::Group0(_) | NodeSm::Kv(_) | NodeSm::Registered(_) => None,
        }
    }
}

impl teseladb_range::host::FenceOutcome for NodeOutput {
    fn range_output(&self) -> Option<&RangeOutput> {
        match self {
            NodeOutput::Range(o) => Some(o),
            NodeOutput::Group0(_) | NodeOutput::Kv(_) | NodeOutput::Bytes(_) => None,
        }
    }
}
