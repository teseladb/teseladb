//! Draining and removing a node (007 FR-015). `node drain` marks the node `Draining`: the balancer
//! moves its replicas and leaderships to the others and its Group 0 vote to an active node, and
//! the node shows a new `topologyVersion` so that drivers look again. Once it holds nothing, it
//! closes its clients' port and the balancer removes it (D-046); `node remove` removes at once a
//! node that holds nothing, a dead one whose replicas were replaced among them.

use teseladb_types::NodeId;

use crate::command::{Group0Command, NodeState};
use crate::group0::Group0Sm;

/// Why a drain or a removal is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrainError {
    /// No such node in Group 0.
    UnknownNode(NodeId),
    /// The node is already removed.
    Removed(NodeId),
    /// The only active node: nothing would take its replicas.
    LastActive(NodeId),
    /// It still holds replicas or votes in Group 0.
    StillHolds {
        /// The node.
        node: NodeId,
        /// Replicas it holds.
        replicas: usize,
        /// It votes in Group 0.
        group0_voter: bool,
    },
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DrainError::UnknownNode(n) => write!(f, "node {} is not in the cluster", n.0),
            DrainError::Removed(n) => write!(f, "node {} is already removed", n.0),
            DrainError::LastActive(n) => {
                write!(f, "node {} is the last active node: nothing would take its data", n.0)
            }
            DrainError::StillHolds { node, replicas, group0_voter } => {
                let plural = if *replicas == 1 { "" } else { "s" };
                let vote = if *group0_voter { " and votes in Group 0" } else { "" };
                write!(
                    f,
                    "node {} still holds {replicas} replica{plural}{vote}: drain it first",
                    node.0
                )
            }
        }
    }
}

/// How far a drain is: what the node still holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Replicas left on it.
    pub replicas: usize,
    /// Ranges whose published leader it is.
    pub leaders: usize,
    /// It votes in Group 0.
    pub group0_voter: bool,
}

impl Progress {
    /// It holds no replica and does not vote in Group 0.
    pub fn drained(&self) -> bool {
        self.replicas == 0 && !self.group0_voter
    }
}

/// The state of `node`, refused if it is unknown or removed.
fn state(sm: &Group0Sm, node: NodeId) -> Result<NodeState, DrainError> {
    match sm.nodes.get(&node).map(|i| i.state) {
        None => Err(DrainError::UnknownNode(node)),
        Some(NodeState::Removed) => Err(DrainError::Removed(node)),
        Some(s) => Ok(s),
    }
}

/// The command that drains `node`: refused for an unknown or removed node and for the last
/// active one. A node already draining gets the same command, which changes nothing.
pub fn drain(sm: &Group0Sm, node: NodeId) -> Result<Group0Command, DrainError> {
    if state(sm, node)? == NodeState::Active {
        let others = sm.nodes.values().any(|i| i.id != node && i.state == NodeState::Active);
        if !others {
            return Err(DrainError::LastActive(node));
        }
    }
    Ok(Group0Command::SetNodeState { node, state: NodeState::Draining })
}

/// What `node` still holds, with Group 0's `voters`.
pub fn progress(sm: &Group0Sm, voters: &[NodeId], node: NodeId) -> Progress {
    let ranges = sm.ranges.values();
    Progress {
        replicas: ranges.clone().filter(|r| r.nodes().contains(&node)).count(),
        leaders: ranges.filter(|r| r.leader_hint == Some(node)).count(),
        group0_voter: voters.contains(&node),
    }
}

/// The command that removes `node`, which has to hold nothing.
pub fn remove(sm: &Group0Sm, voters: &[NodeId], node: NodeId) -> Result<Group0Command, DrainError> {
    state(sm, node)?;
    let p = progress(sm, voters, node);
    if !p.drained() {
        return Err(DrainError::StillHolds {
            node,
            replicas: p.replicas,
            group0_voter: p.group0_voter,
        });
    }
    Ok(Group0Command::SetNodeState { node, state: NodeState::Removed })
}

/// The `topologyVersion` counter a node in `state` shows: it grows when the node starts draining
/// and when it is removed, so that drivers waiting on `hello` look at it again.
pub fn topology_counter(state: NodeState) -> i64 {
    match state {
        NodeState::Active => 0,
        NodeState::Draining => 1,
        NodeState::Removed => 2,
    }
}
