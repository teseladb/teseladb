//! The leader known for each range: the last node that answered for it or that a `NotLeader`
//! named, as long as it is still one of its voters. Without one, Group 0's leader hint, and
//! without that, the first voter.

use std::collections::BTreeMap;

use teseladb_types::{GroupId, NodeId};

use crate::table::RouteEntry;

/// Leaders by range.
#[derive(Clone, Debug, Default)]
pub struct LeaderCache {
    leaders: BTreeMap<GroupId, NodeId>,
}

impl LeaderCache {
    /// The node to send a request for range `e` to.
    pub fn leader(&self, e: &RouteEntry) -> Option<NodeId> {
        let cached = self.leaders.get(&e.group).copied().filter(|n| e.voters.contains(n));
        cached.or(e.leader_hint.filter(|n| e.voters.contains(n))).or(e.voters.first().copied())
    }

    /// `node` leads range `g`.
    pub fn set(&mut self, g: GroupId, node: NodeId) {
        self.leaders.insert(g, node);
    }

    /// The voter of `e` after `from`, in node order, which becomes the known leader.
    pub fn rotate(&mut self, e: &RouteEntry, from: NodeId) -> Option<NodeId> {
        let i = e.voters.iter().position(|n| *n == from).map_or(0, |i| i + 1);
        let next = *e.voters.get(i % e.voters.len().max(1))?;
        self.leaders.insert(e.group, next);
        Some(next)
    }
}
