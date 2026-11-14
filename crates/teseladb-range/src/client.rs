//! The client of the ranges (FR-017, contract `range-client-api.md`), sans-I/O: it starts from the
//! range's descriptor in Group 0 (its replicas and leader hint, [`RangeClient::learn_range`]),
//! sends each request to the leader it knows for the range, follows the hint of a `NotLeader` or
//! tries the range's next replica, and gives up after its attempts. A read is retried always; a
//! write only when it is retryable (it carries its idempotence record, FR-011), because an answer
//! after its proposal does not say whether the new leader committed the entry, as in the reference
//! server's retryable writes.

use std::collections::BTreeMap;

use teseladb_types::{GroupId, NodeId};

use crate::host::{RangeError, Reply};

/// What to do with a request after an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Send it (again) to this node.
    Send(NodeId),
    /// It ended with this result.
    Done(Result<Reply, RangeError>),
}

/// The client: the nodes it may ask and the leader it knows for each range.
#[derive(Clone, Debug)]
pub struct RangeClient {
    nodes: Vec<NodeId>,
    /// Replicas of each range, from its descriptor in Group 0.
    replicas: BTreeMap<GroupId, Vec<NodeId>>,
    leaders: BTreeMap<GroupId, NodeId>,
    max_attempts: u32,
}

impl RangeClient {
    /// A client of the cluster `nodes` (at least one) that tries a request `max_attempts` times.
    pub fn new(nodes: Vec<NodeId>, max_attempts: u32) -> RangeClient {
        assert!(!nodes.is_empty(), "a client needs a node to ask");
        RangeClient { nodes, replicas: BTreeMap::new(), leaders: BTreeMap::new(), max_attempts }
    }

    /// Range `g`'s descriptor in Group 0: its replicas (at least one) and the leader it names,
    /// which replaces the leader the client knew.
    pub fn learn_range(&mut self, g: GroupId, replicas: Vec<NodeId>, leader_hint: Option<NodeId>) {
        assert!(!replicas.is_empty(), "a range has a replica");
        match leader_hint {
            Some(l) => self.leaders.insert(g, l),
            None => self.leaders.remove(&g),
        };
        self.replicas.insert(g, replicas);
    }

    /// The nodes that may lead range `g`: its replicas if Group 0 described it, or every node.
    fn candidates(&self, g: GroupId) -> &[NodeId] {
        self.replicas.get(&g).map_or(&self.nodes, |r| r)
    }

    /// Where a request for range `g` goes: its known leader, or its first candidate.
    pub fn target(&self, g: GroupId) -> NodeId {
        self.leaders.get(&g).copied().unwrap_or(self.candidates(g)[0])
    }

    /// The candidate of `g` after `from` (the first one if `from` is not one).
    fn next_after(&self, g: GroupId, from: NodeId) -> NodeId {
        let c = self.candidates(g);
        let i = c.iter().position(|&n| n == from).map_or(0, |i| i + 1);
        c[i % c.len()]
    }

    /// The answer of node `from` to attempt `attempt` (from 1) of a read.
    pub fn on_reply(
        &mut self,
        g: GroupId,
        from: NodeId,
        attempt: u32,
        r: Result<Reply, RangeError>,
    ) -> Route {
        self.on_reply_write(g, from, attempt, true, r)
    }

    /// The answer of node `from` to attempt `attempt` of a write, `retryable` if it carries its
    /// idempotence record.
    pub fn on_reply_write(
        &mut self,
        g: GroupId,
        from: NodeId,
        attempt: u32,
        retryable: bool,
        r: Result<Reply, RangeError>,
    ) -> Route {
        let next = match &r {
            Ok(_) => {
                self.leaders.insert(g, from);
                return Route::Done(r);
            }
            Err(RangeError::NotLeader { hint }) if hint.0 != 0 => {
                self.leaders.insert(g, *hint);
                *hint
            }
            Err(RangeError::NotLeader { .. } | RangeError::Overloaded | RangeError::Timeout) => {
                let next = self.next_after(g, from);
                self.leaders.insert(g, next);
                next
            }
            Err(
                RangeError::OutOfRange
                | RangeError::EntryLost
                | RangeError::Unsatisfiable
                | RangeError::TooLarge
                | RangeError::Unavailable
                | RangeError::SnapshotTooOld
                | RangeError::ReadUncertain { .. }
                | RangeError::Mutation(_)
                | RangeError::WriteConflict
                | RangeError::IntentConflict(_)
                | RangeError::ReadIntent { .. }
                | RangeError::NoSuchTransaction
                | RangeError::Fenced
                | RangeError::KeyLocked,
            ) => return Route::Done(r),
        };
        if attempt >= self.max_attempts || !retryable {
            return Route::Done(r);
        }
        Route::Send(next)
    }
}
