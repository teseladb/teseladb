//! Configuration of a core, description of a group and read modes.

use std::sync::Arc;

use teseladb_hlc::{HlcClock, MAX_NODE};
use teseladb_raft::RaftConfig;
use teseladb_types::{GroupId, Nanos, NodeId};

/// Default deadline of an operation.
pub const DEFAULT_TIMEOUT: Nanos = Nanos::from_secs(5);
/// Default entries applied past the latest snapshot that trigger a new one.
pub const SNAPSHOT_ENTRIES: u64 = 4096;
/// Default age past which a group with entries applied since its latest snapshot takes a new one
/// (001 FR-005): the core's WAL releases a segment only once every group with entries in it has
/// them under its snapshot, and a group far from `snapshot_entries` would hold it forever.
pub const SNAPSHOT_AGE: Nanos = Nanos::from_secs(10);
/// Default proposals a group keeps waiting for their entry.
pub const MAX_PENDING_PROPOSALS: usize = 4096;
/// Part of a snapshot transfer's deadline that does not depend on its size.
pub const SNAPSHOT_TIMEOUT_BASE: Nanos = Nanos::from_secs(2);
/// Slowest transfer rate a snapshot's deadline allows for (bytes per second).
pub const SNAPSHOT_MIN_RATE: u64 = 8 << 20;

/// A snapshot transfer of `size` bytes without an answer for this long is given up and started
/// again: [`SNAPSHOT_TIMEOUT_BASE`] plus the time to send it at [`SNAPSHOT_MIN_RATE`], so that a
/// small one lost on the way is not waited for long and a large one still moving is not cut.
pub fn snapshot_timeout(size: u64) -> Nanos {
    let send = u128::from(size) * 1_000_000_000 / u128::from(SNAPSHOT_MIN_RATE);
    Nanos(SNAPSHOT_TIMEOUT_BASE.0.saturating_add(u64::try_from(send).unwrap_or(u64::MAX)))
}

/// The node's HLC, shared by its cores (005 FR-003); two configurations are equal when they share it.
#[derive(Clone, Debug)]
pub struct NodeClock(pub Arc<HlcClock>);

impl NodeClock {
    /// A new clock for `node`: its node number is the identifier modulo 1 024 (D-003).
    pub fn for_node(node: NodeId) -> NodeClock {
        NodeClock::with_bound(node, u64::MAX)
    }

    /// The clock of `node` whose `node.meta` holds the bound `bound` (005 FR-003); the node
    /// shares it among its cores.
    pub fn with_bound(node: NodeId, bound: u64) -> NodeClock {
        let number = (node.0 % (u64::from(MAX_NODE) + 1)) as u32;
        NodeClock(Arc::new(HlcClock::with_bound(number, bound).expect("a number below the limit")))
    }
}

impl PartialEq for NodeClock {
    fn eq(&self, other: &NodeClock) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for NodeClock {}

/// Configuration of a core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    /// The node's HLC: every core of a node gets a clone of the same one.
    pub clock: NodeClock,
    /// This node.
    pub node_id: NodeId,
    /// Core.
    pub core: u16,
    /// Raft configuration of every group (`id` is set to `node_id`).
    pub raft: RaftConfig,
    /// Deadline of proposals, reads, configuration changes and forwarded requests.
    pub op_timeout: Nanos,
    /// Entries applied past the latest snapshot that trigger a new one.
    pub snapshot_entries: u64,
    /// Age of the latest snapshot past which a group with entries applied since takes a new one.
    pub snapshot_age: Nanos,
    /// Proposals a group keeps waiting for their entry before answering `Overloaded`.
    pub max_pending_proposals: usize,
    /// Record what the simulator's Raft checker needs ([`crate::Observation`]).
    pub observe: bool,
    /// Tells this start of the core from its earlier ones: read contexts and forwarded requests
    /// are numbered from a value drawn from it and the seed. The server draws it at random on
    /// every start.
    pub instance: u64,
}

impl CoreConfig {
    /// Default values.
    pub fn new(node_id: NodeId, core: u16) -> CoreConfig {
        CoreConfig {
            clock: NodeClock::for_node(node_id),
            node_id,
            core,
            raft: RaftConfig::new(node_id),
            op_timeout: DEFAULT_TIMEOUT,
            snapshot_entries: SNAPSHOT_ENTRIES,
            snapshot_age: SNAPSHOT_AGE,
            max_pending_proposals: MAX_PENDING_PROPOSALS,
            observe: false,
            instance: 0,
        }
    }
}

/// A group to create on this core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupDesc {
    /// Group.
    pub group: GroupId,
    /// Voters (this node among them or among the learners).
    pub voters: Vec<NodeId>,
    /// Learners.
    pub learners: Vec<NodeId>,
}

/// How a read confirms that this replica is still the leader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// With the lease when it is valid; otherwise like `ReadIndex`.
    Lease,
    /// With a heartbeat round answered by a majority.
    ReadIndex,
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_deadline_grows_with_its_size() {
        assert_eq!(snapshot_timeout(0), SNAPSHOT_TIMEOUT_BASE);
        assert_eq!(snapshot_timeout(8 << 20), SNAPSHOT_TIMEOUT_BASE + Nanos::from_secs(1));
        assert_eq!(snapshot_timeout(1 << 30), SNAPSHOT_TIMEOUT_BASE + Nanos::from_secs(128));
        assert_eq!(snapshot_timeout(u64::MAX), Nanos(u64::MAX), "no overflow");
    }
}
