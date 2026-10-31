//! Node liveness on Group 0's leader: every node's `NodeHeartbeat` (its wall clock
//! and its load) every second, `Suspect` after 5 s without one, `Dead` after `dead_after`, and the
//! clock samples the leader turns into `ClockReport`s. The table lives in memory
//! only, while the core leads Group 0.

use std::collections::BTreeMap;

use teseladb_types::{DecodeError, Decoder, Encoder, Nanos, NodeId};

use crate::clock::{ClockSample, offsets};

/// How often each node sends its `NodeHeartbeat` (and the leader its `ClockReport`s, 005 FR-006).
pub const NODE_HEARTBEAT: Nanos = Nanos::from_millis(500);

/// Without a heartbeat for this long a node is `Suspect`.
pub const SUSPECT_AFTER: Nanos = Nanos::from_secs(5);

/// Default `dead_after` (`--dead-after`).
pub const DEAD_AFTER: Nanos = Nanos::from_secs(60);

/// Cluster setting that overrides the leader's `dead_after` (milliseconds as text).
pub const DEAD_AFTER_SETTING: &[u8] = b"dead_after_ms";

/// Liveness state of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LiveState {
    /// Heard within [`SUSPECT_AFTER`].
    Alive,
    /// Not heard for [`SUSPECT_AFTER`].
    Suspect,
    /// Not heard for `dead_after`: the balancer replaces its replicas.
    Dead,
}

/// What a node carries: the sum over its cores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeLoad {
    /// Raft groups.
    pub groups: u64,
    /// Groups it leads.
    pub leaders: u64,
    /// Bytes stored.
    pub bytes: u64,
    /// Free bytes of the data directory's disk (007 FR-010).
    pub free_bytes: u64,
    /// Size of that disk; 0 while unknown.
    pub total_bytes: u64,
    /// Operations per second the ranges it leads served since the previous heartbeat.
    pub ops: u64,
}

impl NodeLoad {
    /// The sum of two cores' loads: the disk is the node's, so its space is the one either knows.
    pub fn plus(self, o: NodeLoad) -> NodeLoad {
        let (free_bytes, total_bytes) = if self.total_bytes != 0 {
            (self.free_bytes, self.total_bytes)
        } else {
            (o.free_bytes, o.total_bytes)
        };
        NodeLoad {
            groups: self.groups + o.groups,
            leaders: self.leaders + o.leaders,
            bytes: self.bytes + o.bytes,
            free_bytes,
            total_bytes,
            ops: self.ops + o.ops,
        }
    }
}

/// A node's entry in the leader's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeLiveness {
    /// When the latest heartbeat arrived (or the table started).
    pub last_heartbeat: Nanos,
    /// State.
    pub state: LiveState,
    /// NodeLoad of its latest heartbeat.
    pub load: NodeLoad,
    /// Clock sample of its latest heartbeat.
    pub clock: Option<ClockSample>,
}

/// Messages between nodes about liveness (peer protocol `0x40`, `0x44`, `0x45`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeFrame {
    /// `NodeHeartbeat`: the sender's wall clock and load, to Group 0's leader.
    Heartbeat {
        /// Wall clock (Unix ms).
        now_ms: u64,
        /// NodeLoad.
        load: NodeLoad,
    },
    /// `ClockReport`: the receiver's offset from the median of Group 0's voters.
    ClockReport {
        /// Offset (ms).
        offset_ms: i64,
    },
    /// `NodeSuspect`: `node` moved to `Suspect` (or `Dead`), or back to `Alive`.
    Suspect {
        /// Node.
        node: NodeId,
        /// It is suspect.
        suspect: bool,
    },
}

impl NodeFrame {
    /// Encoding (the simulator's frames; the node uses the peer protocol's).
    pub fn encode(&self, e: &mut Encoder) {
        match *self {
            NodeFrame::Heartbeat { now_ms, load } => {
                e.u8(0).u64(now_ms).u64(load.groups).u64(load.leaders).u64(load.bytes);
                e.u64(load.free_bytes).u64(load.total_bytes).u64(load.ops);
            }
            NodeFrame::ClockReport { offset_ms } => {
                e.u8(1).u64(offset_ms as u64);
            }
            NodeFrame::Suspect { node, suspect } => {
                e.u8(2).u64(node.0).bool(suspect);
            }
        }
    }

    /// Decoding.
    pub fn decode(d: &mut Decoder<'_>) -> Result<NodeFrame, DecodeError> {
        let at = d.position();
        Ok(match d.u8()? {
            0 => {
                let now_ms = d.u64()?;
                let load = NodeLoad {
                    groups: d.u64()?,
                    leaders: d.u64()?,
                    bytes: d.u64()?,
                    free_bytes: d.u64()?,
                    total_bytes: d.u64()?,
                    ops: d.u64()?,
                };
                NodeFrame::Heartbeat { now_ms, load }
            }
            1 => NodeFrame::ClockReport { offset_ms: d.u64()? as i64 },
            2 => NodeFrame::Suspect { node: NodeId(d.u64()?), suspect: d.bool()? },
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        })
    }
}

/// The leader's table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Liveness {
    nodes: BTreeMap<NodeId, NodeLiveness>,
}

impl Liveness {
    /// A new leader's table: every node alive as of `now`, so nobody is suspect or dead until it
    /// misses the intervals counted from then.
    pub fn start(nodes: impl IntoIterator<Item = NodeId>, now: Nanos) -> Liveness {
        let mut l = Liveness::default();
        for n in nodes {
            l.nodes.insert(n, fresh(now));
        }
        l
    }

    /// A heartbeat of `node`: it is alive. Returns `true` if it was suspect or dead.
    pub fn heartbeat(
        &mut self,
        node: NodeId,
        now: Nanos,
        load: NodeLoad,
        clock: ClockSample,
    ) -> bool {
        let e = self.nodes.entry(node).or_insert_with(|| fresh(now));
        let back = e.state != LiveState::Alive;
        *e =
            NodeLiveness { last_heartbeat: now, state: LiveState::Alive, load, clock: Some(clock) };
        back
    }

    /// Brings the table in line with `nodes` (those not `Removed`: new ones start alive, removed
    /// ones leave) and applies the timeouts. Returns the nodes whose state changed, with the old
    /// and the new one.
    pub fn check(
        &mut self,
        nodes: &[NodeId],
        now: Nanos,
        dead_after: Nanos,
    ) -> Vec<(NodeId, LiveState, LiveState)> {
        self.nodes.retain(|n, _| nodes.contains(n));
        let mut changed = Vec::new();
        for &n in nodes {
            let e = self.nodes.entry(n).or_insert_with(|| fresh(now));
            let silent = now.saturating_since(e.last_heartbeat);
            let state = if silent >= dead_after.max(SUSPECT_AFTER) {
                LiveState::Dead
            } else if silent >= SUSPECT_AFTER {
                LiveState::Suspect
            } else {
                LiveState::Alive
            };
            if state != e.state {
                changed.push((n, e.state, state));
                e.state = state;
            }
        }
        changed
    }

    /// A node's entry.
    pub fn get(&self, node: NodeId) -> Option<&NodeLiveness> {
        self.nodes.get(&node)
    }

    /// Every node's state.
    pub fn states(&self) -> Vec<(NodeId, LiveState)> {
        self.nodes.iter().map(|(n, e)| (*n, e.state)).collect()
    }

    /// Each sampled node's offset from the median of `voters`' clocks, with the
    /// leader's own clock as one more voter.
    pub fn clock_reports(&self, leader: NodeId, voters: &[NodeId]) -> Vec<(NodeId, i64)> {
        let samples: Vec<ClockSample> = self.nodes.values().filter_map(|e| e.clock).collect();
        offsets(leader, voters, &samples)
    }
}

fn fresh(now: Nanos) -> NodeLiveness {
    NodeLiveness {
        last_heartbeat: now,
        state: LiveState::Alive,
        load: NodeLoad::default(),
        clock: None,
    }
}

/// `dead_after` from the cluster setting, or `default` when it is unset or not a number.
pub fn dead_after(setting: Option<&[u8]>, default: Nanos) -> Nanos {
    setting
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(default, Nanos::from_millis)
}
