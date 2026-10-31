//! The clock rule (005 FR-006, D-004): Group 0's leader estimates every node's wall clock from
//! its `NodeHeartbeat`s and reports to each node its offset from the median of Group 0's voters
//! (`ClockReport`, every 500 ms). A node more than [`MAX_CLOCK_OFFSET_MS`] away cannot lead any
//! range, so a single skewed clock, even the leader's, only excludes itself. Each node also
//! compares its clock with the Raft heartbeats of its peers ([`PeerClocks`]) and sets itself
//! aside without waiting for Group 0.

use teseladb_types::NodeId;

pub use teseladb_hlc::{MAX_CLOCK_OFFSET_MS, PEER_WINDOW, PeerClocks, may_lead};

/// A node's clock as Group 0's leader sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSample {
    /// Node.
    pub node: NodeId,
    /// The node's wall clock in its heartbeat (ms).
    pub sent_ms: i64,
    /// The leader's wall clock when the heartbeat arrived (ms).
    pub received_ms: i64,
    /// Round trip to that node (ms): half of it is the heartbeat's flight.
    pub rtt_ms: i64,
}

impl ClockSample {
    /// The node's clock minus the leader's, discounting half the round trip (saturating: a
    /// heartbeat with an absurd clock only makes its own node's offset absurd).
    pub fn skew_ms(&self) -> i64 {
        self.sent_ms.saturating_add(self.rtt_ms / 2).saturating_sub(self.received_ms)
    }
}

/// Every sampled node's offset from the median of the voters' clocks (the leader's own included,
/// with skew 0). Voters without a sample do not count towards the median.
pub fn offsets(leader: NodeId, voters: &[NodeId], samples: &[ClockSample]) -> Vec<(NodeId, i64)> {
    let skew = |n: NodeId| -> Option<i64> {
        if n == leader {
            return Some(0);
        }
        samples.iter().find(|s| s.node == n).map(ClockSample::skew_ms)
    };
    let mut known: Vec<i64> = voters.iter().filter_map(|n| skew(*n)).collect();
    if known.is_empty() {
        return Vec::new();
    }
    known.sort_unstable();
    // The lower median: with an even count no clock is averaged into one nobody has.
    let median = known[(known.len() - 1) / 2];
    let mut out: Vec<(NodeId, i64)> =
        samples.iter().map(|s| (s.node, s.skew_ms().saturating_sub(median))).collect();
    if !samples.iter().any(|s| s.node == leader) {
        out.push((leader, median.saturating_neg()));
    }
    out.sort_unstable();
    out
}
