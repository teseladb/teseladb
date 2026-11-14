//! Group configuration and its default values.

use teseladb_types::{Nanos, NodeId};

/// Largest entry, and largest message limit, a group accepts: the inter-node
/// protocol reassembles an `AppendReq` up to this size from its pieces of 1 MiB.
pub const MAX_ENTRY_BYTES: usize = 64 << 20;

/// Raft configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaftConfig {
    /// This node.
    pub id: NodeId,
    /// Heartbeat every this many ticks.
    pub heartbeat_ticks: u32,
    /// Minimum of the randomized election timeout (ticks).
    pub election_ticks_min: u32,
    /// Maximum (exclusive) of the election timeout (ticks).
    pub election_ticks_max: u32,
    /// Lease duration from the send of the heartbeat that got a majority.
    pub lease_dur: Nanos,
    /// `AppendReq` messages in flight per follower.
    pub max_inflight_msgs: usize,
    /// Soft limit of an `AppendReq` (a larger entry travels alone).
    pub max_msg_bytes: usize,
    /// Maximum size of an entry (at most [`MAX_ENTRY_BYTES`]).
    pub max_entry_bytes: usize,
    /// Proposed but uncommitted bytes from which proposals are rejected.
    pub max_uncommitted_bytes: usize,
    /// `PreVote` enabled.
    pub pre_vote: bool,
    /// `CheckQuorum` enabled (required by the lease).
    pub check_quorum: bool,
    /// Ticks without hearing from a quorum after which a leader steps down, as a RefDb primary
    /// does after `electionTimeoutMillis` (10 s by default). Until then it takes writes, which a
    /// later leader may roll back, but it serves no lease read and lets others vote.
    pub step_down_ticks: u32,
    /// Idle ticks before quiescing.
    pub quiesce_after_ticks: u32,
}

impl RaftConfig {
    /// Default values for node `id`.
    #[must_use]
    pub fn new(id: NodeId) -> RaftConfig {
        RaftConfig {
            id,
            heartbeat_ticks: 5,
            election_ticks_min: 15,
            election_ticks_max: 30,
            lease_dur: Nanos::from_millis(110),
            max_inflight_msgs: 256,
            max_msg_bytes: 1 << 20,
            max_entry_bytes: MAX_ENTRY_BYTES,
            max_uncommitted_bytes: 32 << 20,
            pre_vote: true,
            check_quorum: true,
            step_down_ticks: 1000,
            quiesce_after_ticks: 200,
        }
    }

    /// Checks that the values are consistent.
    ///
    /// # Errors
    ///
    /// The first inconsistency, as text.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.0 == 0 {
            return Err("id 0 is reserved");
        }
        if self.heartbeat_ticks == 0 || self.heartbeat_ticks >= self.election_ticks_min {
            return Err("the heartbeat has to be more frequent than the election timeout");
        }
        if self.step_down_ticks < self.election_ticks_min {
            return Err("a leader cannot step down before one election timeout");
        }
        if self.election_ticks_max <= self.election_ticks_min {
            return Err("the maximum election timeout has to exceed the minimum");
        }
        if self.max_inflight_msgs == 0 || self.max_msg_bytes == 0 {
            return Err("zero flow limits");
        }
        if self.max_entry_bytes > MAX_ENTRY_BYTES || self.max_msg_bytes > MAX_ENTRY_BYTES {
            return Err("an entry or a message above 64 MiB");
        }
        Ok(())
    }
}
