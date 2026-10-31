//! The router's decision after each answer (FR-003): a success caches the leader; `NotLeader`
//! follows its hint when it names a voter, or moves to the next voter, as a busy or silent
//! replica does; `OutOfRange` learns the current descriptors the answer carries and routes again,
//! or asks the caller to refresh the table from its copy of Group 0. A request
//! gives up when its attempts run out, and a write that cannot be retried gives up at its first
//! doubtful answer, because it may have been applied.

use teseladb_meta::group0::{Group0Sm, RangeDesc};
use teseladb_types::{GroupId, NodeId};

use crate::leader_cache::LeaderCache;
use crate::table::{RouteEntry, RouteTable};

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Range.
    pub group: GroupId,
    /// Node it is sent to.
    pub node: NodeId,
    /// Core of the range there.
    pub core: u16,
    /// Generation of the range the router knew.
    pub generation: u64,
}

/// The answer of a replica, as the router reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The request ended there.
    Done,
    /// That replica does not lead; `hint`: the leader it knows.
    NotLeader {
        /// Known leader.
        hint: Option<NodeId>,
    },
    /// The range does not hold the key; `hints`: the current descriptors the answering node
    /// holds of that range and of the range that holds the key (after a split both halves have
    /// the same replicas, so the node that answers has the new one).
    OutOfRange {
        /// Current descriptors.
        hints: Vec<RouteEntry>,
    },
    /// No answer in time, or the replica is overloaded.
    Busy,
    /// The request failed for a reason that another attempt does not change.
    Failed,
}

/// What to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// The request ended with the answer given.
    Done,
    /// Send it to this target.
    Send(Target),
    /// Refresh the table from the copy of Group 0 and route again.
    Refresh,
    /// Stop: the attempts ran out, or a write that cannot be retried got a doubtful answer.
    GiveUp,
}

/// The router of a node.
#[derive(Clone, Debug)]
pub struct Router {
    table: RouteTable,
    leaders: LeaderCache,
    max_attempts: u32,
}

impl Router {
    /// A router that tries each request at most `max_attempts` times.
    pub fn new(max_attempts: u32) -> Router {
        Router { table: RouteTable::default(), leaders: LeaderCache::default(), max_attempts }
    }

    /// Its table.
    pub fn table(&self) -> &RouteTable {
        &self.table
    }

    /// Rebuilds the table from the node's copy of Group 0.
    pub fn refresh(&mut self, sm: &Group0Sm) {
        self.table.refresh(sm);
    }

    /// Rebuilds the table from Group 0's descriptors.
    pub fn refresh_ranges<'a>(&mut self, ranges: impl IntoIterator<Item = &'a RangeDesc>) {
        self.table.refresh_ranges(ranges);
    }

    /// Where a request for range `g` goes first.
    pub fn route_group(&self, g: GroupId) -> Option<Target> {
        self.table.entry(g).and_then(|e| self.target(e))
    }

    fn target(&self, e: &RouteEntry) -> Option<Target> {
        let node = self.leaders.leader(e)?;
        Some(Target { group: e.group, node, core: e.core, generation: e.generation })
    }

    /// Where a request for `key` goes first.
    pub fn route(&self, key: &[u8]) -> Option<Target> {
        self.table.lookup(key).and_then(|e| self.target(e))
    }

    /// The answer to attempt `attempt` (from 1) of a request for `key` sent to `sent`.
    pub fn on_answer(
        &mut self,
        key: &[u8],
        sent: &Target,
        attempt: u32,
        retryable: bool,
        answer: Answer,
    ) -> Next {
        let next = match answer {
            Answer::Done => {
                self.leaders.set(sent.group, sent.node);
                return Next::Done;
            }
            Answer::Failed => return Next::Done,
            Answer::OutOfRange { hints } => {
                if hints.is_empty() {
                    self.table.forget(sent.group);
                }
                for h in hints {
                    self.table.learn(h);
                }
                match self.route(key) {
                    Some(t) if attempt < self.max_attempts => return Next::Send(t),
                    Some(_) => return Next::GiveUp,
                    None => return Next::Refresh,
                }
            }
            Answer::NotLeader { hint } => self.table.entry(sent.group).cloned().and_then(|e| {
                match hint.filter(|n| e.voters.contains(n)) {
                    Some(n) => {
                        self.leaders.set(e.group, n);
                        Some(n)
                    }
                    None => self.leaders.rotate(&e, sent.node),
                }
            }),
            Answer::Busy => self
                .table
                .entry(sent.group)
                .cloned()
                .and_then(|e| self.leaders.rotate(&e, sent.node)),
        };
        let Some(node) = next else { return Next::Refresh };
        if attempt >= self.max_attempts || !retryable {
            return Next::GiveUp;
        }
        Next::Send(Target { node, ..sent.clone() })
    }
}
