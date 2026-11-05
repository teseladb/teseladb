//! A core's part in its ranges' checksums (005 FR-014, research R5). The host computes this
//! replica's sums and, where it leads, tallies the replicas' sums and marks the ones that differ
//! from the majority; this module carries both across nodes and rebuilds a marked replica:
//!
//! - Each sum goes to the range's leader: to the host itself if this replica leads, or in a
//!   forwarded request to the leader the core knows. Without a known leader it is dropped, as a
//!   lost frame would be; the next checksum asks again.
//! - Each replica the leader marks gets the order: at once if it is this node's, or in a forwarded
//!   request to its node.
//! - A rebuild closes the replica (its log stays), takes it out of the host, clears its intervals
//!   in the LSM, local keys included, and opens it again. It then starts as after a restart that
//!   finds its data missing: from its own snapshot, whose data it asks another replica for
//!   (FR-012), and its log after it; a range that never took a snapshot replays its whole log. A
//!   replica with something of its group in flight with the disk waits for it first.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_lsm::Lsm;
use teseladb_multiraft::{MultiRaftCore, OpError, OpId};
use teseladb_range::checksum::Report;
use teseladb_range::host::RangeHost;
use teseladb_range::sm::RangeSm;
use teseladb_raft::Role;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

use crate::coresm::CoreSm;
use crate::forward::Request;
use crate::ranges::Replicas;

/// Where a rebuild of a replica is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rebuild {
    /// Waiting to close the replica.
    Close,
    /// The LSM clears its intervals (the clearing's identifier).
    Clearing(u64),
}

/// The checksums of a core's ranges between nodes, and the rebuilds of its replicas.
#[derive(Debug)]
pub struct Checksums {
    me: NodeId,
    /// Forwarded sums and orders: their answers carry nothing.
    sent: BTreeSet<OpId>,
    rebuilding: BTreeMap<GroupId, Rebuild>,
    /// Rebuilds finished since the core started.
    rebuilt: u64,
}

impl Checksums {
    /// The checksums of a core of node `me`.
    pub fn new(me: NodeId) -> Checksums {
        Checksums { me, sent: BTreeSet::new(), rebuilding: BTreeMap::new(), rebuilt: 0 }
    }

    /// Replicas whose data was rebuilt since the core started.
    pub fn rebuilt(&self) -> u64 {
        self.rebuilt
    }

    /// Whether replica `g` is being rebuilt.
    pub fn rebuilding(&self, g: GroupId) -> bool {
        self.rebuilding.contains_key(&g)
    }

    /// After the host's poll: this replica's sums go to their leaders and the leader's marks to
    /// their replicas.
    pub fn route(&mut self, core: &mut MultiRaftCore<CoreSm>, host: &mut RangeHost, now: Nanos) {
        for r in host.take_checksum_reports() {
            let Some(st) = core.status(r.range) else { continue };
            if st.role == Role::Leader {
                host.on_checksum_report(&r);
            } else if st.leader != NodeId(0) {
                self.send(core, st.leader, Request::Checksum(r), now);
            }
        }
        for m in host.take_rebuilds() {
            if m.node == self.me {
                self.order(m.range);
            } else {
                self.send(core, m.node, Request::Rebuild(m.range), now);
            }
        }
    }

    fn send(&mut self, core: &mut MultiRaftCore<CoreSm>, to: NodeId, r: Request, now: Nanos) {
        let op = core.forward(to, Bytes::from_vec(r.encode()), now);
        self.sent.insert(op);
    }

    /// Another replica's sum for a range this replica leads.
    pub fn on_report(host: &mut RangeHost, r: &Report) {
        host.on_checksum_report(r);
    }

    /// The leader marked this node's replica of `g` for rebuild.
    pub fn order(&mut self, g: GroupId) {
        self.rebuilding.entry(g).or_insert(Rebuild::Close);
    }

    /// Moves the rebuilds: closes each replica that has nothing in flight with the disk and asks
    /// the LSM to clear its intervals. A replica this core does not have is forgotten.
    pub fn step(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &mut Replicas,
        lsm: &mut Lsm,
    ) {
        let waiting: Vec<GroupId> = self
            .rebuilding
            .iter()
            .filter(|(_, r)| **r == Rebuild::Close)
            .map(|(g, _)| *g)
            .collect();
        for g in waiting {
            let intervals = core.state_machine(g).and_then(CoreSm::range).map(RangeSm::intervals);
            match (intervals, core.close_group(g)) {
                (Some(intervals), Ok(())) => {
                    host.remove_range(g);
                    replicas.hold(g);
                    let id = lsm.clear_range(g, intervals);
                    self.rebuilding.insert(g, Rebuild::Clearing(id));
                }
                (_, Err(OpError::Busy)) => {}
                // Not a range of this core.
                _ => {
                    self.rebuilding.remove(&g);
                }
            }
        }
    }

    /// The LSM finished clearing `id`: if it was a rebuild's, its replica opens again.
    pub fn on_cleared(&mut self, replicas: &mut Replicas, id: u64) {
        let done = self.rebuilding.iter().find(|(_, r)| **r == Rebuild::Clearing(id));
        let Some((&g, _)) = done else { return };
        self.rebuilding.remove(&g);
        replicas.release(g);
        self.rebuilt += 1;
    }

    /// The end of a forwarded sum or order; `false` if it is not this module's.
    pub fn on_done(&mut self, op: OpId) -> bool {
        self.sent.remove(&op)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_clearing_of_no_rebuild_changes_nothing() {
        let mut c = Checksums::new(NodeId(1));
        let mut r = Replicas::new(NodeId(1), 0, 1 << 20);
        c.order(GroupId(4));
        c.on_cleared(&mut r, 99);
        assert!(c.rebuilding(GroupId(4)));
        assert_eq!(c.rebuilt(), 0);
        assert!(!c.on_done(OpId(5)));
    }
}
