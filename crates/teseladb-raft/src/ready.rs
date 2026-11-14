//! Batch of output actions of a `RawNode` and its queryable state.

use teseladb_types::{Nanos, NodeId};

use crate::message::{Entry, HardState, Message, Snapshot};
use crate::raft::Role;

/// Role and leader (they change rarely; they are reported when they change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftState {
    /// Known leader (0 = none).
    pub leader: NodeId,
    /// Role.
    pub role: Role,
}

/// `ReadIndex` read ready: answer when `applied ≥ index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadState {
    /// Context passed to `read_index`.
    pub ctx: u64,
    /// Index to reach.
    pub index: u64,
}

/// Actions the caller has to execute.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    /// Number for `on_persisted`.
    pub number: u64,
    /// Hard state to store (if it changed).
    pub hard_state: Option<HardState>,
    /// Entries to store (after the truncation, if any).
    pub entries_to_persist: Vec<Entry>,
    /// Truncate the stored log from this index before storing the entries.
    pub truncate_from: Option<u64>,
    /// Snapshot to install (before applying anything else).
    pub snapshot_to_apply: Option<Snapshot>,
    /// Messages that can be sent right away.
    pub messages: Vec<Message>,
    /// Votes and responses: only after persisting this `Ready`.
    pub messages_after_persist: Vec<Message>,
    /// Committed entries to apply, in order.
    pub committed: Vec<Entry>,
    /// `ReadIndex` reads that are ready.
    pub read_states: Vec<ReadState>,
    /// Change of role or leader.
    pub soft_state: Option<SoftState>,
}

/// State of a group (for metrics, routing and tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// This node.
    pub id: NodeId,
    /// Role.
    pub role: Role,
    /// Known leader.
    pub leader: NodeId,
    /// Hard state.
    pub hard_state: HardState,
    /// Last index of the log.
    pub last_index: u64,
    /// Last index of the log on this node's disk.
    pub persisted: u64,
    /// Last applied index.
    pub applied: u64,
    /// `(node, matched)` of each follower if it is the leader.
    pub progress: Vec<(NodeId, u64)>,
    /// `(node, round trip)` of each follower that answered a heartbeat round of this leader.
    pub rtts: Vec<(NodeId, Nanos)>,
}

impl Status {
    /// Replicas with entry `index` on their disk, this node included, as far as this leader knows
    ///.
    #[must_use]
    pub fn durable_replicas(&self, index: u64) -> usize {
        let others = self.progress.iter().filter(|(n, m)| *n != self.id && *m >= index).count();
        others + usize::from(self.persisted >= index)
    }
}
