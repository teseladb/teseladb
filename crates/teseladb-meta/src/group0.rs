//! Group 0's state machine: the cluster's nodes, the ranges with their opaque bounds and replicas,
//! the cluster settings and the cluster time. Deterministic: every replica applies the same
//! entries to the same state and gets the same results.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::catalog::CatalogCommand;
use crate::catalog::{Catalog, CatalogError, CollectionEntry};
use crate::clustertime_keys::ClusterTimeKeys;
use crate::command::snapshot_codec::{
    get_addrs, get_bytes_array, get_node_list, get_state, put_addrs, put_node_list, state_code,
};
use crate::command::{Group0Command, NodeAddrs, NodeState};
use crate::descriptors::RangeIndex;
use crate::enroll_tokens::{EnrollTokens, TokenError};
use crate::indexes::{IndexEntry, Indexes};
use crate::request::Requests;
use crate::security::{Principal, Security, SecurityError};

/// Version of the snapshot format (12: the core changes in progress, 007 FR-011).
pub const SNAPSHOT_VERSION: u8 = 13;
/// The previous version, still read: its users and roles are kept by database and name, in name
/// order, and the order becomes the insertion order when read.
pub const SNAPSHOT_VERSION_WITH_NAMED_SECURITY: u8 = 12;
/// The version before, still read: it has no core changes either.
pub const SNAPSHOT_VERSION_WITHOUT_RELOCATIONS: u8 = 11;
/// The version before, still read: its databases have no mark either.
pub const SNAPSHOT_VERSION_WITHOUT_DB_MARKS: u8 = 10;
/// Group of Group 0.
pub const GROUP0: GroupId = GroupId(0);

/// A node of the cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// Identifier.
    pub id: NodeId,
    /// Addresses, name and cores.
    pub addrs: NodeAddrs,
    /// Administrative state.
    pub state: NodeState,
    /// Group 0 index at which it joined.
    pub joined_index: u64,
    /// Fingerprint of its certificate (zeros with `--insecure`).
    pub cert_fingerprint: [u8; 32],
    /// Last `boot_epoch` it reported (0 = it never started).
    pub boot_epoch: u32,
    /// Its tags (`--tags`), sorted by name (007 FR-013).
    pub tags: Vec<(String, String)>,
}

/// Role of a replica in its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaRole {
    /// Votes.
    Voter,
    /// Receives the log without voting, until its change settles.
    Learner,
}

/// A range: opaque bounds, the core that holds it on every node and its replicas (data model §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeDesc {
    /// Raft group.
    pub group: GroupId,
    /// First key.
    pub start: Vec<u8>,
    /// First key past the range (empty = no end).
    pub end: Vec<u8>,
    /// Core of every node that holds it.
    pub core: u16,
    /// Replicas, in node order.
    pub replicas: Vec<(NodeId, ReplicaRole)>,
    /// Grows with every replica change.
    pub replica_epoch: u64,
    /// Grows with every change of its bounds.
    pub generation: u64,
    /// Where its leader should be.
    pub leader_hint: Option<NodeId>,
    /// The term in which the leader named by `leader_hint` published itself; 0 if the balancer
    /// asked the range to move its leadership there (007 FR-010).
    pub leader_term: u64,
    /// A recorded split not completed yet.
    pub pending_split: Option<Vec<u8>>,
    /// A replica change waits for `ReplicasSettled`.
    pub settling: bool,
}

impl RangeDesc {
    /// Whether `key` falls in the range.
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && (self.end.is_empty() || key < self.end.as_slice())
    }

    /// Nodes that hold it.
    pub fn nodes(&self) -> Vec<NodeId> {
        self.replicas.iter().map(|(n, _)| *n).collect()
    }

    /// Nodes that vote in it.
    pub fn voters(&self) -> Vec<NodeId> {
        self.replicas.iter().filter(|(_, r)| *r == ReplicaRole::Voter).map(|(n, _)| *n).collect()
    }
}

/// Why a command did not change anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group0Error {
    /// The cluster was already created.
    AlreadyBootstrapped,
    /// No `Bootstrap` yet.
    NotBootstrapped,
    /// The name or an address already belongs to another node.
    NameOrAddrTaken,
    /// Unknown node.
    NoSuchNode,
    /// A node's tags out of order by name, or a name twice.
    InvalidTags,
    /// Unknown range.
    NoSuchGroup,
    /// Empty replicas, unknown or inactive nodes, or nodes repeated.
    BadReplicas,
    /// Bounds that are empty or overlap another range.
    BadBounds,
    /// Another change of the range is in progress, or it does not match the current epoch.
    RangeBusy,
    /// A split key outside the range, on one of its bounds, or not the recorded one.
    BadSplit,
    /// No merge of those two ranges is recorded.
    NoSuchMerge,
    /// A core change to the range's own core, or to a core some node of the range lacks.
    BadCore,
    /// No core change of the range to that core is recorded.
    NoSuchRelocation,
    /// The namespace already has a collection.
    NamespaceExists,
    /// The namespace has no collection.
    NamespaceNotFound,
    /// A user or role change found its name taken (013).
    PrincipalExists(Principal),
    /// A user or role change found its name free (013).
    PrincipalNotFound(Principal),
    /// A user or role change computed from a document that changed since (013).
    SecurityConflict,
    /// A signing key proposed on a state that changed since, or one to drop that is gone (013).
    ClusterTimeKeyRaced,
    /// An enrollment token that does not exist, whose secret does not match, or whose id is
    /// taken (013).
    EnrollTokenUnknown,
    /// An enrollment token already used.
    EnrollTokenUsed,
    /// An enrollment token past its expiry.
    EnrollTokenExpired,
    /// The collection already has an index with that name.
    IndexExists,
    /// No such index.
    IndexNotFound,
    /// A database whose name differs only in case from one of the catalog (D-077).
    DatabaseDifferCase,
}

/// Result of applying a Group 0 command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Group0Result {
    /// Done.
    Ok,
    /// The node created (or the one that already had that fingerprint).
    Node(NodeId),
    /// The range created.
    Range(GroupId),
    /// The collection created, or found by an implicit creation.
    Collection(CollectionEntry),
    /// The index created (kind 41).
    Index(IndexEntry),
    /// Nothing changed.
    Err(Group0Error),
}

/// Group 0's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group0Sm {
    /// Cluster identifier (zeros until `Bootstrap`).
    pub cluster_id: [u8; 16],
    /// Nodes.
    pub nodes: BTreeMap<NodeId, NodeInfo>,
    /// Ranges.
    pub ranges: BTreeMap<GroupId, RangeDesc>,
    /// Cluster settings.
    pub settings: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Cluster time: the highest of every entry's.
    pub cluster_time_ms: u64,
    /// Last node number given.
    pub last_node: u64,
    /// Last range number given (Group 0 is 0: ranges start at 1).
    pub last_group: u64,
    /// Last applied index.
    pub applied: u64,
    /// The catalog (006 FR-027), shared with the operations that read it: a change copies it
    /// only while one of them still holds it.
    pub catalog: Arc<Catalog>,
    /// Index definitions of the catalog (007 FR-001).
    pub indexes: Indexes,
    /// Results of the idempotent commands (007 contract `catalog.md`).
    pub requests: Requests,
    /// The ranges by first key (007 FR-002), derived from `ranges`.
    pub range_index: RangeIndex,
    /// The number each recorded split gives its new range, by the range being split (007).
    pub split_ids: BTreeMap<GroupId, GroupId>,
    /// The merges recorded and not completed nor aborted (007 FR-009), by the range that absorbs:
    /// the range absorbed and the index of the intent, which names the merge in the ranges' logs.
    pub merges: BTreeMap<GroupId, (GroupId, u64)>,
    /// The core changes recorded and not completed (007 FR-011), by range: the target core and
    /// the index of the intent, which names the change in the range's log.
    pub relocations: BTreeMap<GroupId, (u16, u64)>,
    /// Users and roles (013 FR-009).
    pub security: Security,
    /// The `$clusterTime` signing keys, sealed (013 FR-014); boxed, so that Group 0's state
    /// grows by a pointer.
    pub clustertime_keys: Box<ClusterTimeKeys>,
    /// The node enrollment tokens (013 FR-005), boxed for the same reason.
    pub enroll_tokens: Box<EnrollTokens>,
}

impl Group0Sm {
    /// Whether the cluster exists.
    pub fn bootstrapped(&self) -> bool {
        self.cluster_id != [0; 16]
    }

    /// Active nodes.
    pub fn active_nodes(&self) -> Vec<NodeId> {
        self.nodes.values().filter(|n| n.state == NodeState::Active).map(|n| n.id).collect()
    }

    /// Whether a node other than `except` that is not removed has the name or inter-node address
    /// of `a`.
    fn addr_taken(&self, a: &NodeAddrs, except: Option<NodeId>) -> bool {
        self.nodes.values().any(|n| {
            Some(n.id) != except
                && n.state != NodeState::Removed
                && (n.addrs.name == a.name || n.addrs.peer_addr == a.peer_addr)
        })
    }

    /// New name and addresses of a node that is not removed; the cores are those it joined with.
    fn set_node_addrs(
        &mut self,
        node: NodeId,
        addrs: &NodeAddrs,
    ) -> Result<Group0Result, Group0Error> {
        if !self.nodes.get(&node).is_some_and(|n| n.state != NodeState::Removed) {
            return Err(Group0Error::NoSuchNode);
        }
        if self.addr_taken(addrs, Some(node)) {
            return Err(Group0Error::NameOrAddrTaken);
        }
        let n = self.nodes.get_mut(&node).expect("checked above");
        n.addrs = NodeAddrs { cores: n.addrs.cores, ..addrs.clone() };
        Ok(Group0Result::Ok)
    }

    /// The tags of a node that is not removed.
    fn set_node_tags(
        &mut self,
        node: NodeId,
        tags: &[(String, String)],
    ) -> Result<Group0Result, Group0Error> {
        if !self.nodes.get(&node).is_some_and(|n| n.state != NodeState::Removed) {
            return Err(Group0Error::NoSuchNode);
        }
        if !tags.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(Group0Error::InvalidTags);
        }
        self.nodes.get_mut(&node).expect("checked above").tags = tags.to_vec();
        Ok(Group0Result::Ok)
    }

    fn add_node(&mut self, addrs: &NodeAddrs, fingerprint: [u8; 32], index: u64) -> NodeId {
        self.last_node += 1;
        let id = NodeId(self.last_node);
        let info = NodeInfo {
            id,
            addrs: addrs.clone(),
            state: NodeState::Active,
            joined_index: index,
            cert_fingerprint: fingerprint,
            boot_epoch: 0,
            tags: Vec::new(),
        };
        self.nodes.insert(id, info);
        id
    }

    fn good_replicas(&self, nodes: &[NodeId]) -> bool {
        let mut sorted = nodes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        !nodes.is_empty()
            && sorted.len() == nodes.len()
            && nodes.iter().all(|n| self.nodes.get(n).is_some_and(|i| i.state == NodeState::Active))
    }

    fn overlaps(&self, start: &[u8], end: &[u8]) -> bool {
        self.ranges.values().any(|r| {
            let before_end = r.end.is_empty() || start < r.end.as_slice();
            let after_start = end.is_empty() || end > r.start.as_slice();
            before_end && after_start
        })
    }

    fn range_mut(&mut self, g: GroupId) -> Result<&mut RangeDesc, Group0Error> {
        self.ranges.get_mut(&g).ok_or(Group0Error::NoSuchGroup)
    }

    fn create_range(
        &mut self,
        start: &[u8],
        end: &[u8],
        replicas: &[NodeId],
    ) -> Result<GroupId, Group0Error> {
        if !self.good_replicas(replicas) {
            return Err(Group0Error::BadReplicas);
        }
        if (!end.is_empty() && end <= start) || self.overlaps(start, end) {
            return Err(Group0Error::BadBounds);
        }
        self.last_group += 1;
        let group = GroupId(self.last_group);
        // Every replica runs it on the same core: the smallest core count of its nodes bounds it.
        let cores = replicas.iter().map(|n| self.nodes[n].addrs.cores.max(1)).min().unwrap_or(1);
        let mut nodes = replicas.to_vec();
        nodes.sort_unstable();
        let core = self.least_loaded_core(&nodes, cores);
        let desc = RangeDesc {
            group,
            start: start.to_vec(),
            end: end.to_vec(),
            core,
            replicas: nodes.into_iter().map(|n| (n, ReplicaRole::Voter)).collect(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
        };
        self.insert_range(desc);
        Ok(group)
    }

    fn set_replicas(
        &mut self,
        g: GroupId,
        epoch: u64,
        add: &[NodeId],
        remove: &[NodeId],
    ) -> Result<(), Group0Error> {
        let known = |n: &NodeId| self.nodes.get(n).is_some_and(|i| i.state == NodeState::Active);
        let all_known = add.iter().all(known);
        let merging = self.in_change(g);
        let r = self.range_mut(g)?;
        if merging || r.settling || r.replica_epoch != epoch || r.pending_split.is_some() {
            return Err(Group0Error::RangeBusy);
        }
        let held = r.nodes();
        let bad_add = !all_known || add.iter().any(|n| held.contains(n));
        let bad_remove = remove.iter().any(|n| !held.contains(n));
        let left = (held.len() + add.len()).saturating_sub(remove.len());
        if bad_add || bad_remove || left == 0 || (add.is_empty() && remove.is_empty()) {
            return Err(Group0Error::BadReplicas);
        }
        r.replicas.retain(|(n, _)| !remove.contains(n));
        r.replicas.extend(add.iter().map(|n| (*n, ReplicaRole::Learner)));
        r.replicas.sort_unstable_by_key(|(n, _)| *n);
        r.replica_epoch += 1;
        r.settling = true;
        Ok(())
    }

    fn settle(&mut self, g: GroupId, epoch: u64) -> Result<(), Group0Error> {
        let r = self.range_mut(g)?;
        if !r.settling || r.replica_epoch != epoch {
            return Err(Group0Error::RangeBusy);
        }
        r.settling = false;
        for (_, role) in &mut r.replicas {
            *role = ReplicaRole::Voter;
        }
        Ok(())
    }

    /// A leader that published itself in `term`, or with term 0 the balancer's request; a
    /// publication of a term older than the one recorded changes nothing (007 FR-010).
    fn leader_hint(&mut self, g: GroupId, node: NodeId, term: u64) -> Result<(), Group0Error> {
        let r = self.range_mut(g)?;
        if !r.nodes().contains(&node) {
            return Err(Group0Error::BadReplicas);
        }
        if term == 0 || term >= r.leader_term {
            r.leader_hint = Some(node);
            r.leader_term = term;
        }
        Ok(())
    }

    /// Records the intent of splitting `g` at `at` and gives the new range its number now: the
    /// range's `Split` entry writes the new range's state under it (007 FR-007).
    fn split(&mut self, g: GroupId, at: &[u8]) -> Result<GroupId, Group0Error> {
        let merging = self.in_change(g);
        let capped = self.capped_collections();
        let r = self.range_mut(g)?;
        if merging || r.pending_split.is_some() || r.settling {
            return Err(Group0Error::RangeBusy);
        }
        if at <= r.start.as_slice() || !r.contains(at) {
            return Err(Group0Error::BadSplit);
        }
        // A capped collection stays in one range (015 research R1): the cut moves to its edge.
        let at = teseladb_keys::space::cut_outside(at, (&r.start, &r.end), |t, c| {
            t == 0 && capped.contains(&c)
        })
        .ok_or(Group0Error::BadSplit)?;
        r.pending_split = Some(at);
        self.last_group += 1;
        let new = GroupId(self.last_group);
        self.split_ids.insert(g, new);
        Ok(new)
    }

    /// The numbers of the capped collections in the catalog.
    fn capped_collections(&self) -> BTreeSet<u32> {
        self.catalog
            .collections
            .values()
            .filter(|e| {
                matches!(
                    crate::collection_types::CollectionKind::of(&e.options),
                    Ok(crate::collection_types::CollectionKind::Capped { .. })
                )
            })
            .map(|e| e.id)
            .collect()
    }

    fn complete_split(&mut self, g: GroupId, at: &[u8]) -> Result<GroupId, Group0Error> {
        let id = self.split_ids.get(&g).copied();
        let r = self.range_mut(g)?;
        let Some(id) = id.filter(|_| r.pending_split.as_deref() == Some(at)) else {
            return Err(Group0Error::BadSplit);
        };
        let mut new = r.clone();
        r.end = at.to_vec();
        r.generation += 1;
        r.pending_split = None;
        self.split_ids.remove(&g);
        new.group = id;
        new.start = at.to_vec();
        new.generation = 1;
        new.pending_split = None;
        new.leader_hint = None;
        let group = new.group;
        self.insert_range(new);
        Ok(group)
    }

    /// The merge that absorbs range `g`, if one is recorded: the range that absorbs it and the
    /// index of the intent.
    pub fn merging(&self, g: GroupId) -> Option<(GroupId, u64)> {
        self.merges.iter().find(|(_, (r, _))| *r == g).map(|(l, (_, at))| (*l, *at))
    }

    /// Whether range `g` is either side of a recorded merge, or moves to another core: neither
    /// lets it split, merge or change replicas.
    fn in_change(&self, g: GroupId) -> bool {
        self.merges.contains_key(&g)
            || self.merging(g).is_some()
            || self.relocations.contains_key(&g)
    }

    /// Records the intent of moving range `g` to core `core` of every node (007 FR-011): a core
    /// other than its own that every node of the range has, with no split, merge, replica change
    /// or other core change in progress.
    fn relocate(&mut self, index: u64, g: GroupId, core: u16) -> Result<(), Group0Error> {
        let r = self.ranges.get(&g).ok_or(Group0Error::NoSuchGroup)?;
        if r.settling || r.pending_split.is_some() || self.in_change(g) {
            return Err(Group0Error::RangeBusy);
        }
        let cores = r.nodes().iter().map(|n| self.nodes.get(n).map_or(0, |i| i.addrs.cores)).min();
        if core == r.core || cores.is_none_or(|c| core >= c.max(1)) {
            return Err(Group0Error::BadCore);
        }
        self.relocations.insert(g, (core, index));
        Ok(())
    }

    /// Every replica of `g` runs on `core`, the target of its recorded core change.
    fn complete_relocate(&mut self, g: GroupId, core: u16) -> Result<(), Group0Error> {
        if self.relocations.get(&g).is_none_or(|(c, _)| *c != core) {
            return Err(Group0Error::NoSuchRelocation);
        }
        self.relocations.remove(&g);
        self.range_mut(g)?.core = core;
        Ok(())
    }

    /// Records the intent of merging `right` into its left neighbour `left` (007 FR-009): both on
    /// the same core and replicas, with no split, replica change or other merge in progress.
    fn merge(&mut self, index: u64, left: GroupId, right: GroupId) -> Result<(), Group0Error> {
        let (l, r) = match (self.ranges.get(&left), self.ranges.get(&right)) {
            (Some(l), Some(r)) => (l, r),
            _ => return Err(Group0Error::NoSuchGroup),
        };
        if l.end.is_empty() || l.end != r.start {
            return Err(Group0Error::BadBounds);
        }
        if l.core != r.core || l.replicas != r.replicas {
            return Err(Group0Error::BadReplicas);
        }
        let busy = |d: &RangeDesc| d.settling || d.pending_split.is_some();
        if busy(l) || busy(r) || self.in_change(left) || self.in_change(right) {
            return Err(Group0Error::RangeBusy);
        }
        self.merges.insert(left, (right, index));
        Ok(())
    }

    /// The merge of `right` into `left` as recorded, which `CompleteMerge` and `AbortMerge` end.
    fn end_merge(&mut self, left: GroupId, right: GroupId) -> Result<(), Group0Error> {
        if self.merges.get(&left).is_none_or(|(r, _)| *r != right) {
            return Err(Group0Error::NoSuchMerge);
        }
        self.merges.remove(&left);
        Ok(())
    }

    fn complete_merge(&mut self, left: GroupId, right: GroupId) -> Result<(), Group0Error> {
        self.end_merge(left, right)?;
        let gone = self.ranges.remove(&right).ok_or(Group0Error::NoSuchGroup)?;
        self.range_index.remove(&gone.start);
        let l = self.range_mut(left)?;
        l.end = gone.end;
        l.generation += 1;
        Ok(())
    }

    /// Applies a decoded command at `index`.
    pub fn apply_command(&mut self, index: u64, cmd: &Group0Command) -> Group0Result {
        self.apply_command_at(index, None, cmd)
    }

    /// [`Group0Sm::apply_command`] of the entry with HLC mark `mark` (the catalog keeps the mark
    /// that added each database).
    pub fn apply_command_at(
        &mut self,
        index: u64,
        mark: Option<teseladb_hlc::Hlc>,
        cmd: &Group0Command,
    ) -> Group0Result {
        if !self.bootstrapped() && !matches!(cmd, Group0Command::Bootstrap { .. }) {
            return Group0Result::Err(Group0Error::NotBootstrapped);
        }
        let r = match cmd {
            Group0Command::Bootstrap { cluster_id, first_node, cert_fingerprint } => {
                if self.bootstrapped() {
                    return Group0Result::Err(Group0Error::AlreadyBootstrapped);
                }
                self.cluster_id = *cluster_id;
                Ok(Group0Result::Node(self.add_node(first_node, *cert_fingerprint, index)))
            }
            Group0Command::AddNode { node, cert_fingerprint } => {
                // Idempotent by fingerprint (except the `--insecure` one, all zeros).
                let again = self.nodes.values().find(|n| {
                    *cert_fingerprint != [0; 32]
                        && n.state != NodeState::Removed
                        && n.cert_fingerprint == *cert_fingerprint
                });
                match again {
                    Some(n) => Ok(Group0Result::Node(n.id)),
                    None if self.addr_taken(node, None) => Err(Group0Error::NameOrAddrTaken),
                    None => Ok(Group0Result::Node(self.add_node(node, *cert_fingerprint, index))),
                }
            }
            Group0Command::SetNodeState { node, state } => match self.nodes.get_mut(node) {
                Some(n) => {
                    n.state = *state;
                    Ok(Group0Result::Ok)
                }
                None => Err(Group0Error::NoSuchNode),
            },
            Group0Command::SetNodeAddrs { node, addrs } => self.set_node_addrs(*node, addrs),
            Group0Command::SetNodeTags { node, tags } => self.set_node_tags(*node, tags),
            Group0Command::NodeBoot { node, boot_epoch } => match self.nodes.get_mut(node) {
                Some(n) => {
                    n.boot_epoch = n.boot_epoch.max(*boot_epoch);
                    Ok(Group0Result::Ok)
                }
                None => Err(Group0Error::NoSuchNode),
            },
            Group0Command::ClusterTick { cluster_time_ms } => {
                self.cluster_time_ms = self.cluster_time_ms.max(*cluster_time_ms);
                Ok(Group0Result::Ok)
            }
            Group0Command::SetSetting { key, value } => {
                self.settings.insert(key.clone(), value.clone());
                Ok(Group0Result::Ok)
            }
            Group0Command::CreateRange { start, end, replicas } => {
                self.create_range(start, end, replicas).map(Group0Result::Range)
            }
            Group0Command::SetReplicas { group, epoch, add, remove } => {
                self.set_replicas(*group, *epoch, add, remove).map(|()| Group0Result::Ok)
            }
            Group0Command::ReplicasSettled { group, epoch } => {
                self.settle(*group, *epoch).map(|()| Group0Result::Ok)
            }
            Group0Command::SetLeaderHint { group, node, term } => {
                self.leader_hint(*group, *node, *term).map(|()| Group0Result::Ok)
            }
            Group0Command::SplitRange { group, at } => {
                self.split(*group, at).map(Group0Result::Range)
            }
            Group0Command::CompleteSplit { group, at, .. } => {
                self.complete_split(*group, at).map(Group0Result::Range)
            }
            Group0Command::MergeRanges { left, right } => {
                self.merge(index, *left, *right).map(|()| Group0Result::Ok)
            }
            Group0Command::CompleteMerge { left, right } => {
                self.complete_merge(*left, *right).map(|()| Group0Result::Ok)
            }
            Group0Command::AbortMerge { left, right } => {
                self.end_merge(*left, *right).map(|()| Group0Result::Ok)
            }
            Group0Command::RelocateRange { group, core } => {
                self.relocate(index, *group, *core).map(|()| Group0Result::Ok)
            }
            Group0Command::CompleteRelocate { group, core } => {
                self.complete_relocate(*group, *core).map(|()| Group0Result::Ok)
            }
            Group0Command::Catalog(c) => {
                if let CatalogCommand::Purged { id } = c {
                    self.indexes.forget_collection(*id);
                }
                Arc::make_mut(&mut self.catalog)
                    .apply_at(index, mark, c)
                    .map_err(catalog_error)
                    .map(|entry| entry.map_or(Group0Result::Ok, Group0Result::Collection))
            }
            Group0Command::Indexes(c) => {
                let live: BTreeSet<u32> = self.catalog.collections.values().map(|e| e.id).collect();
                self.indexes
                    .apply(c, |id| live.contains(&id))
                    .map_err(catalog_error)
                    .map(|entry| entry.map_or(Group0Result::Ok, Group0Result::Index))
            }
            Group0Command::Security(ops) => {
                self.security.apply(ops).map(|()| Group0Result::Ok).map_err(|e| match e {
                    SecurityError::Exists(p) => Group0Error::PrincipalExists(p),
                    SecurityError::NotFound(p) => Group0Error::PrincipalNotFound(p),
                    SecurityError::Conflict => Group0Error::SecurityConflict,
                })
            }
            Group0Command::AddClusterTimeKey { key, after } => {
                let added = self.clustertime_keys.add(key, *after);
                added.then_some(Group0Result::Ok).ok_or(Group0Error::ClusterTimeKeyRaced)
            }
            Group0Command::ExpireClusterTimeKey { id } => {
                let gone = self.clustertime_keys.expire(*id);
                gone.then_some(Group0Result::Ok).ok_or(Group0Error::ClusterTimeKeyRaced)
            }
            Group0Command::CreateEnrollToken(t) => {
                let created = self.enroll_tokens.create(t, self.cluster_time_ms);
                created.then_some(Group0Result::Ok).ok_or(Group0Error::EnrollTokenUnknown)
            }
            Group0Command::ConsumeEnrollToken { id, secret_hash } => self
                .enroll_tokens
                .consume(id, secret_hash, self.cluster_time_ms)
                .map(|()| Group0Result::Ok)
                .map_err(|e| match e {
                    TokenError::Unknown => Group0Error::EnrollTokenUnknown,
                    TokenError::Used => Group0Error::EnrollTokenUsed,
                    TokenError::Expired => Group0Error::EnrollTokenExpired,
                }),
            Group0Command::Idempotent { req, cmd } => {
                if let Some(done) = self.requests.get(*req) {
                    return done.clone();
                }
                let done = self.apply_command(index, cmd);
                self.requests.record(*req, done.clone());
                return done;
            }
        };
        r.unwrap_or_else(Group0Result::Err)
    }

    fn encode_snapshot(&self) -> Vec<u8> {
        self.encode_snapshot_as(SNAPSHOT_VERSION)
    }

    /// The snapshot in format `version`: older ones leave out what they did not have (core changes,
    /// databases' marks) and keep the users and roles by name.
    pub fn encode_snapshot_as(&self, version: u8) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u8(version).raw(&self.cluster_id);
        e.u64(self.cluster_time_ms).u64(self.last_node).u64(self.last_group).u64(self.applied);
        e.var(self.nodes.len() as u64);
        for n in self.nodes.values() {
            e.u64(n.id.0);
            put_addrs(&mut e, &n.addrs);
            e.u8(state_code(n.state))
                .u64(n.joined_index)
                .raw(&n.cert_fingerprint)
                .u32(n.boot_epoch);
            crate::command::put_tags(&mut e, &n.tags);
        }
        e.var(self.ranges.len() as u64);
        for r in self.ranges.values() {
            e.u64(r.group.0).bytes(&r.start).bytes(&r.end).u16(r.core);
            put_node_list(&mut e, &r.nodes());
            let learners: Vec<NodeId> = r
                .replicas
                .iter()
                .filter(|(_, role)| *role == ReplicaRole::Learner)
                .map(|(n, _)| *n)
                .collect();
            put_node_list(&mut e, &learners);
            e.u64(r.replica_epoch).u64(r.generation).u64(r.leader_hint.map_or(0, |n| n.0));
            e.u64(r.leader_term);
            e.bool(r.pending_split.is_some()).bytes(r.pending_split.as_deref().unwrap_or(&[]));
            e.bool(r.settling);
        }
        e.var(self.settings.len() as u64);
        for (k, v) in &self.settings {
            e.bytes(k).bytes(v);
        }
        self.catalog.encode_with(&mut e, version != SNAPSHOT_VERSION_WITHOUT_DB_MARKS);
        self.indexes.encode(&mut e);
        self.requests.encode(&mut e);
        e.var(self.split_ids.len() as u64);
        for (g, new) in &self.split_ids {
            e.u64(g.0).u64(new.0);
        }
        e.var(self.merges.len() as u64);
        for (l, (r, at)) in &self.merges {
            e.u64(l.0).u64(r.0).u64(*at);
        }
        if version > SNAPSHOT_VERSION_WITH_NAMED_SECURITY {
            self.security.encode(&mut e);
        } else {
            self.security.encode_named(&mut e);
        }
        self.clustertime_keys.encode(&mut e);
        self.enroll_tokens.encode(&mut e);
        if version > SNAPSHOT_VERSION_WITHOUT_RELOCATIONS {
            e.var(self.relocations.len() as u64);
            for (g, (core, at)) in &self.relocations {
                e.u64(g.0).u16(*core).u64(*at);
            }
        }
        e.finish()
    }

    fn decode_snapshot(b: &[u8]) -> Result<Group0Sm, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let version = d.u8()?;
        if !(SNAPSHOT_VERSION_WITHOUT_DB_MARKS..=SNAPSHOT_VERSION).contains(&version) {
            return Err(DecodeError::InvalidValue { offset: at });
        }
        let mut sm = Group0Sm {
            cluster_id: get_bytes_array(&mut d)?,
            cluster_time_ms: d.u64()?,
            last_node: d.u64()?,
            last_group: d.u64()?,
            applied: d.u64()?,
            ..Group0Sm::default()
        };
        for _ in 0..d.var()? {
            let id = NodeId(d.u64()?);
            let info = NodeInfo {
                id,
                addrs: get_addrs(&mut d)?,
                state: get_state(&mut d)?,
                joined_index: d.u64()?,
                cert_fingerprint: get_bytes_array(&mut d)?,
                boot_epoch: d.u32()?,
                tags: crate::command::get_tags(&mut d)?,
            };
            sm.nodes.insert(id, info);
        }
        for _ in 0..d.var()? {
            let group = GroupId(d.u64()?);
            let (start, end, core) = (d.bytes()?.to_vec(), d.bytes()?.to_vec(), d.u16()?);
            let (nodes, learners) = (get_node_list(&mut d)?, get_node_list(&mut d)?);
            let replicas = nodes
                .into_iter()
                .map(|n| {
                    let role = if learners.contains(&n) {
                        ReplicaRole::Learner
                    } else {
                        ReplicaRole::Voter
                    };
                    (n, role)
                })
                .collect();
            let (replica_epoch, generation, hint) = (d.u64()?, d.u64()?, d.u64()?);
            let leader_term = d.u64()?;
            let split = d.bool()?;
            let at = d.bytes()?.to_vec();
            let desc = RangeDesc {
                group,
                start,
                end,
                core,
                replicas,
                replica_epoch,
                generation,
                leader_hint: (hint != 0).then_some(NodeId(hint)),
                leader_term,
                pending_split: split.then_some(at),
                settling: d.bool()?,
            };
            sm.ranges.insert(group, desc);
        }
        sm.reindex();
        for _ in 0..d.var()? {
            let k = d.bytes()?.to_vec();
            sm.settings.insert(k, d.bytes()?.to_vec());
        }
        sm.catalog =
            Arc::new(Catalog::decode_with(&mut d, version != SNAPSHOT_VERSION_WITHOUT_DB_MARKS)?);
        sm.indexes = Indexes::decode(&mut d)?;
        sm.requests = Requests::decode(&mut d)?;
        for _ in 0..d.var()? {
            sm.split_ids.insert(GroupId(d.u64()?), GroupId(d.u64()?));
        }
        for _ in 0..d.var()? {
            let (l, r, at) = (GroupId(d.u64()?), GroupId(d.u64()?), d.u64()?);
            sm.merges.insert(l, (r, at));
        }
        sm.security = if version > SNAPSHOT_VERSION_WITH_NAMED_SECURITY {
            Security::decode(&mut d)?
        } else {
            Security::decode_named(&mut d)?
        };
        sm.clustertime_keys = Box::new(ClusterTimeKeys::decode(&mut d)?);
        sm.enroll_tokens = Box::new(EnrollTokens::decode(&mut d)?);
        if version > SNAPSHOT_VERSION_WITHOUT_RELOCATIONS {
            for _ in 0..d.var()? {
                let (g, core, at) = (GroupId(d.u64()?), d.u16()?, d.u64()?);
                sm.relocations.insert(g, (core, at));
            }
        }
        d.finish()?;
        Ok(sm)
    }
}

fn catalog_error(e: CatalogError) -> Group0Error {
    match e {
        CatalogError::NamespaceExists => Group0Error::NamespaceExists,
        CatalogError::NamespaceNotFound => Group0Error::NamespaceNotFound,
        CatalogError::IndexExists => Group0Error::IndexExists,
        CatalogError::IndexNotFound => Group0Error::IndexNotFound,
        CatalogError::DatabaseDifferCase => Group0Error::DatabaseDifferCase,
    }
}

impl StateMachine for Group0Sm {
    type Output = Group0Result;

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Group0Result, ApplyError> {
        let cmd = Group0Command::decode(payload).map_err(|e| ApplyError(format!("{e:?}")))?;
        self.applied = ctx.index;
        self.cluster_time_ms = self.cluster_time_ms.max(ctx.ts.physical);
        Ok(self.apply_command_at(ctx.index, Some(ctx.ts), &cmd))
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        self.encode_snapshot()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        *self = Group0Sm::decode_snapshot(snap).map_err(|e| RestoreError(format!("{e:?}")))?;
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}
