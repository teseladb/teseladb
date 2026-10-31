//! The routing table: each range's bounds, core, replicas and generation by first key, built
//! from the node's copy of Group 0. A range that answered `OutOfRange` with its current
//! descriptor teaches the router before that copy catches up: the hint replaces the entries it
//! overlaps and stays until the copy has that range at its generation or later.

use std::collections::BTreeMap;
use std::ops::Bound;

use teseladb_meta::group0::{Group0Sm, RangeDesc, ReplicaRole};
use teseladb_types::{GroupId, NodeId};

/// What the router knows of a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    /// Raft group.
    pub group: GroupId,
    /// First key.
    pub start: Vec<u8>,
    /// First key past the range (empty: no end).
    pub end: Vec<u8>,
    /// Core on every replica.
    pub core: u16,
    /// Voters, in node order: the nodes that may lead it.
    pub voters: Vec<NodeId>,
    /// Generation of its bounds.
    pub generation: u64,
    /// Where Group 0 wants its leader.
    pub leader_hint: Option<NodeId>,
}

impl RouteEntry {
    /// The entry of a descriptor of Group 0.
    pub fn from_desc(d: &RangeDesc) -> RouteEntry {
        let voters = d.replicas.iter().filter(|(_, r)| *r == ReplicaRole::Voter).map(|(n, _)| *n);
        RouteEntry {
            group: d.group,
            start: d.start.clone(),
            end: d.end.clone(),
            core: d.core,
            voters: voters.collect(),
            generation: d.generation,
            leader_hint: d.leader_hint,
        }
    }

    /// Whether `key` falls in the range.
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && (self.end.is_empty() || key < self.end.as_slice())
    }

    fn overlaps(&self, o: &RouteEntry) -> bool {
        (self.end.is_empty() || o.start < self.end) && (o.end.is_empty() || self.start < o.end)
    }
}

/// The ranges by first key, with the hints not yet in the copy of Group 0.
#[derive(Clone, Debug, Default)]
pub struct RouteTable {
    by_start: BTreeMap<Vec<u8>, RouteEntry>,
    hints: BTreeMap<GroupId, RouteEntry>,
}

impl RouteTable {
    /// Rebuilds the table from a copy of Group 0, then applies the hints it does not have yet.
    pub fn refresh(&mut self, sm: &Group0Sm) {
        self.refresh_ranges(sm.ranges.values());
    }

    /// Rebuilds the table from Group 0's descriptors (a core without Group 0 receives them from
    /// the core that holds it), then applies the hints they do not have yet.
    pub fn refresh_ranges<'a>(&mut self, ranges: impl IntoIterator<Item = &'a RangeDesc>) {
        let ranges: BTreeMap<GroupId, &RangeDesc> =
            ranges.into_iter().map(|d| (d.group, d)).collect();
        self.by_start =
            ranges.values().map(|d| (d.start.clone(), RouteEntry::from_desc(d))).collect();
        self.hints.retain(|g, h| ranges.get(g).is_none_or(|d| d.generation < h.generation));
        let hints: Vec<RouteEntry> = self.hints.values().cloned().collect();
        for h in hints {
            self.place(h);
        }
    }

    /// Learns a range's current descriptor from its answer; an older generation than the one
    /// known is ignored.
    pub fn learn(&mut self, e: RouteEntry) {
        let known = self.by_start.values().find(|x| x.group == e.group);
        if known.is_some_and(|x| x.generation >= e.generation) {
            return;
        }
        self.hints.insert(e.group, e.clone());
        self.place(e);
    }

    fn place(&mut self, e: RouteEntry) {
        self.by_start.retain(|_, x| !x.overlaps(&e) && x.group != e.group);
        self.by_start.insert(e.start.clone(), e);
    }

    /// Forgets the range `g` until the next refresh.
    pub fn forget(&mut self, g: GroupId) {
        self.by_start.retain(|_, x| x.group != g);
        self.hints.remove(&g);
    }

    /// The range that holds `key`.
    pub fn lookup(&self, key: &[u8]) -> Option<&RouteEntry> {
        let below = (Bound::Unbounded, Bound::Included(key));
        self.by_start
            .range::<[u8], _>(below)
            .next_back()
            .map(|(_, e)| e)
            .filter(|e| e.contains(key))
    }

    /// The entry of range `g`.
    pub fn entry(&self, g: GroupId) -> Option<&RouteEntry> {
        self.by_start.values().find(|e| e.group == g)
    }

    /// The ranges in key order.
    pub fn entries(&self) -> impl Iterator<Item = &RouteEntry> {
        self.by_start.values()
    }

    /// Number of ranges.
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    /// Whether it knows no range.
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }
}
