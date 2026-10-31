//! Raft safety: every replica applies the same entry at the same index of a
//! group (same term and contents: Log Matching and State Machine Safety), and a group has at most
//! one leader per term (Election Safety). The world feeds it with the entries each cluster core
//! applies and every election one of them wins. It also checks that a linearizable read misses
//! nothing: the index a leader registers for a read that is later served is at least every commit
//! index any node had reached when it was registered. And it checks the two rules that make it so
//! (raft-api guarantees 5 and 6b): a leader registers no read before its term's first entry is
//! committed, and no lease read is served once another node has won a later term.

use std::collections::BTreeMap;

use teseladb_types::GroupId;

use crate::checkers::Violation;

/// An entry a replica applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedEntry {
    /// Group.
    pub group: GroupId,
    /// Index.
    pub index: u64,
    /// Term.
    pub term: u64,
    /// CRC32C of its data.
    pub crc: u32,
}

/// What the replicas applied and who led each term.
#[derive(Clone, Debug, Default)]
pub struct RaftSafety {
    /// (group, index) → (term, CRC of the data, first node that applied it).
    applied: BTreeMap<(GroupId, u64), (u64, u32, u32)>,
    /// (group, term) → leader.
    leaders: BTreeMap<(GroupId, u64), u32>,
    /// group → highest commit index any node reached.
    commits: BTreeMap<GroupId, u64>,
    /// Registered reads → (group, index, highest commit index of the group at that moment).
    reads: BTreeMap<ReadKey, (GroupId, u64, u64)>,
    /// group → highest term in which some node won an election.
    max_term: BTreeMap<GroupId, u64>,
}

/// A read a leader registers (`teseladb_multiraft::Observation::ReadRegistered`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadRegistration {
    /// Group.
    pub group: GroupId,
    /// Index the read reflects (the leader's commit index).
    pub index: u64,
    /// The leader's term.
    pub term: u64,
    /// Term of the entry at `index`.
    pub index_term: Option<u64>,
    /// Served right away through the lease.
    pub lease: bool,
}

/// A read: (node, core, life of the node, read context).
pub type ReadKey = (u32, u16, u64, u64);

impl RaftSafety {
    /// `node` applied `a`.
    pub fn applied(&mut self, a: AppliedEntry, node: u32) -> Result<(), Violation> {
        match self.applied.get(&(a.group, a.index)) {
            None => {
                self.applied.insert((a.group, a.index), (a.term, a.crc, node));
                Ok(())
            }
            Some(&(term, crc, first)) if (term, crc) != (a.term, a.crc) => Err(Violation::safety(
                "raft_safety",
                format!(
                    "group {} index {}: n{first} applied term {term} crc {crc:08x}, \
                     n{node} applied term {} crc {:08x}",
                    a.group.0, a.index, a.term, a.crc
                ),
            )),
            Some(_) => Ok(()),
        }
    }

    /// `node` leads `g` in `term`.
    pub fn leader(&mut self, g: GroupId, term: u64, node: u32) -> Result<(), Violation> {
        let t = self.max_term.entry(g).or_default();
        *t = (*t).max(term);
        match *self.leaders.entry((g, term)).or_insert(node) {
            first if first != node => Err(Violation::safety(
                "raft_safety",
                format!("group {} term {term}: leaders n{first} and n{node}", g.0),
            )),
            _ => Ok(()),
        }
    }

    /// Some node's commit index of `g` reached `commit`.
    pub fn committed(&mut self, g: GroupId, commit: u64) {
        let c = self.commits.entry(g).or_default();
        *c = (*c).max(commit);
    }

    /// A leader registered read `key`.
    pub fn read_registered(&mut self, key: ReadKey, r: ReadRegistration) -> Result<(), Violation> {
        let g = r.group;
        let Some(index_term) = r.index_term else {
            return Err(Violation::safety(
                "raft_safety",
                format!(
                    "group {} n{}: commit index {} is no longer in the leader's log (it applied and \
                     compacted beyond its commit)",
                    g.0, key.0, r.index
                ),
            ));
        };
        if index_term != r.term {
            return Err(Violation::safety(
                "raft_safety",
                format!(
                    "group {} n{}: read registered in term {} before the term's first commit \
                     (commit index {} is of term {})",
                    g.0, key.0, r.term, r.index, index_term
                ),
            ));
        }
        let later = self.max_term.get(&g).copied().unwrap_or(0);
        if r.lease && later > r.term {
            return Err(Violation::safety(
                "raft_safety",
                format!(
                    "group {} n{}: lease read in term {} after a leader of term {later} was elected",
                    g.0, key.0, r.term
                ),
            ));
        }
        let required = self.commits.get(&g).copied().unwrap_or(0);
        self.reads.insert(key, (g, r.index, required));
        Ok(())
    }

    /// Read `key` is served; reads this checker did not see registered (followers') are ignored.
    pub fn read_served(&mut self, key: ReadKey) -> Result<(), Violation> {
        match self.reads.remove(&key) {
            Some((g, index, required)) if index < required => Err(Violation::safety(
                "raft_safety",
                format!(
                    "group {} n{}: a read served at index {index} although index {required} was \
                     committed before it was registered",
                    g.0, key.0
                ),
            )),
            _ => Ok(()),
        }
    }

    /// Entries seen (diagnostics).
    pub fn applied_count(&self) -> usize {
        self.applied.len()
    }
}
