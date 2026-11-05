//! Group 0's state machine: the cluster's nodes, the ranges with their opaque bounds and replicas,
//! the cluster settings and the cluster time. Deterministic: every replica applies the same
//! entries to the same state and gets the same results.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::catalog::CatalogCommand;
use crate::catalog::{Catalog, CatalogError, CollectionEntry};
use crate::changelog::Changelog;
use crate::clustertime_keys::ClusterTimeKeys;
use crate::command::snapshot_codec::{
    get_addrs, get_bytes_array, get_node_list, get_state, put_addrs, put_node_list, state_code,
};
use crate::command::{Group0Command, NodeAddrs, NodeState};
use crate::ddl_log::{DdlChange, DdlLog};
use crate::descriptors::RangeIndex;
use crate::enroll_tokens::{EnrollTokens, TokenError};
use crate::indexes::{IndexEntry, Indexes};
use crate::meta2::{Meta1, Meta2Range};
use crate::request::Requests;
use crate::security::{Principal, Security, SecurityError};
use crate::tenants::Tenants;

/// Version of the snapshot format (20: when each range was last split or moved, 018 FR-017).
pub const SNAPSHOT_VERSION: u8 = 21;
/// The snapshot before the limits, the access list and the suspension of each tenant (05-11-2026),
/// still read and written while some node runs it: its tenants have none (007 FR-030).
pub const SNAPSHOT_VERSION_WITHOUT_TENANT_STATE: u8 = 20;
/// The snapshot before the ranges' marks (03-11-2026), still read: every range as never moved.
pub const SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS: u8 = 19;
/// The snapshot before the places of the indexes (02-11-2026), still read: each index at its
/// identifier.
pub const SNAPSHOT_VERSION_WITHOUT_INDEX_PLACES: u8 = 18;
/// The snapshot before the API keys (02-11-2026), still read: none.
pub const SNAPSHOT_VERSION_WITHOUT_API_KEYS: u8 = 17;
/// The snapshot before the catalog changes (01-11-2026), still read: none recorded.
pub const SNAPSHOT_VERSION_WITHOUT_DDL: u8 = 16;
/// The snapshot before tenants (01-11-2026), still read: no tenant but the default one.
pub const SNAPSHOT_VERSION_WITHOUT_TENANTS: u8 = 15;
/// The snapshot before `config.changelog` (01-11-2026), still read: no change recorded.
pub const SNAPSHOT_VERSION_WITHOUT_CHANGELOG: u8 = 14;
/// The previous version, still read: it has no `Meta1`.
pub const SNAPSHOT_VERSION_WITHOUT_META1: u8 = 13;
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
    /// Cluster time, in milliseconds, of its last split or move (a replica change, or the
    /// balancer's request for another leader), or of its creation; a merge keeps the later of
    /// both (018 FR-017). 0 in a descriptor written before it was kept.
    pub moved_ms: u64,
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
    /// A change of settings computed from a value that changed since (007 FR-017).
    SettingConflict,
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
    /// The descriptors were already handed over to Meta2 (007 FR-014).
    HandedOff,
    /// No range number is left: Group 0 gives numbers below 2^32, a Meta2 range 2^32 per range.
    OutOfGroupIds,
    /// A restore into a catalog that has collections or indexes (007 FR-021), or a tenant
    /// dropped with databases or users (FR-025).
    NotEmpty,
    /// A tenant with that name exists (007 FR-025).
    TenantExists,
    /// No tenant with that name.
    TenantNotFound,
    /// No tenant can have that name: empty, too long or with a zero byte.
    InvalidTenantName,
    /// The default tenant has no limits, access list or suspension (007 FR-030).
    InvalidOptions,
    /// An entry some node that is not removed does not read yet: it waits until every node runs
    /// the version that brought it (007 FR-027).
    NotEveryNodeRunsIt,
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
    /// Refused: the descriptor is held by this Meta2 range (007 FR-014), where the sender
    /// repeats the order.
    Meta1(Meta2Range),
    /// The tenant created (007 FR-025).
    Tenant(u32),
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
    /// The Meta2 ranges, once the descriptors were handed over to them (007 FR-014).
    pub meta1: Option<Meta1>,
    /// The history of the changes of its descriptors (`config.changelog`).
    pub changelog: Changelog,
    /// The tenants (007 FR-025), boxed to keep Group 0's state small.
    pub tenants: Box<Tenants>,
    /// The catalog changes change streams read (014 FR-014), boxed for the same reason.
    pub ddl: Box<DdlLog>,
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

    /// The merge that absorbs range `g`, if one is recorded: the range that absorbs it and the
    /// index of the intent.
    pub fn merging(&self, g: GroupId) -> Option<(GroupId, u64)> {
        crate::descriptors::merging_in(&self.merges, g)
    }

    /// The numbers of the capped collections in the catalog.
    pub fn capped_collections(&self) -> BTreeSet<(u32, u32)> {
        self.catalog
            .collections
            .values()
            .filter(|e| {
                matches!(
                    crate::collection_types::CollectionKind::of(&e.options),
                    Ok(crate::collection_types::CollectionKind::Capped { .. })
                )
            })
            .map(|e| (e.tenant, e.id))
            .collect()
    }

    /// An order on a data descriptor after the handoff to Meta2 (007 FR-014): the Meta2 range
    /// that holds the descriptor, if Group 0 still has it, or `NoSuchGroup`; `None` for any
    /// other command, which Group 0 applies.
    fn handed_off(&self, cmd: &Group0Command) -> Option<Group0Result> {
        // Faulty variant (007 T028): Group 0 goes on taking orders on the descriptors it handed
        // over, a stale route the Meta2 ranges never see.
        #[cfg(teseladb_bug = "meta2_stale_route")]
        let m1 = None::<&Meta1>?;
        #[cfg(not(teseladb_bug = "meta2_stale_route"))]
        let m1 = self.meta1.as_ref()?;
        let by_group = |g: &GroupId| match self.ranges.get(g) {
            Some(r) => {
                m1.holder_of(&r.end).map_or(Group0Result::Err(Group0Error::NoSuchGroup), |h| {
                    Group0Result::Meta1(h.clone())
                })
            }
            None => Group0Result::Err(Group0Error::NoSuchGroup),
        };
        Some(match cmd {
            Group0Command::CreateRange { end, .. } => {
                m1.holder_of(end).map_or(Group0Result::Err(Group0Error::BadBounds), |h| {
                    Group0Result::Meta1(h.clone())
                })
            }
            Group0Command::SetReplicas { group, .. }
            | Group0Command::ReplicasSettled { group, .. }
            | Group0Command::SetLeaderHint { group, .. }
            | Group0Command::SplitRange { group, .. }
            | Group0Command::CompleteSplit { group, .. }
            | Group0Command::RelocateRange { group, .. }
            | Group0Command::CompleteRelocate { group, .. } => by_group(group),
            Group0Command::MergeRanges { left, .. }
            | Group0Command::CompleteMerge { left, .. }
            | Group0Command::AbortMerge { left, .. } => by_group(left),
            _ => return None,
        })
    }

    /// `HandoffMeta2` (007 FR-014): the Meta2 ranges that split space `04`, numbered here.
    fn handoff(&mut self, ranges: &[Meta2Range]) -> Result<Group0Result, Group0Error> {
        if self.meta1.is_some() {
            return Err(Group0Error::HandedOff);
        }
        if !crate::meta2::partitions(ranges) {
            return Err(Group0Error::BadBounds);
        }
        let good = |r: &Meta2Range| {
            let nodes: Vec<NodeId> = r.replicas.iter().map(|(n, _)| *n).collect();
            crate::descriptors::good_replicas(&nodes, &self.nodes)
                && r.replicas.iter().any(|(_, role)| *role == ReplicaRole::Voter)
        };
        if !ranges.iter().all(good) {
            return Err(Group0Error::BadReplicas);
        }
        let n = ranges.len() as u64;
        if self.last_group + n >= crate::descriptors::BLOCK_BASE {
            return Err(Group0Error::OutOfGroupIds);
        }
        let numbered = ranges
            .iter()
            .zip(self.last_group + 1..)
            .map(|(r, g)| Meta2Range { group: GroupId(g), ..r.clone() })
            .collect();
        self.last_group += n;
        let facts = crate::meta2::Meta2Cluster::of(self);
        self.meta1 = Some(Meta1 { ranges: numbered, facts, seeded: BTreeSet::new() });
        Ok(Group0Result::Ok)
    }

    /// `Meta2Seeded` (007 FR-014): Meta2 range `g` committed its first entry, so Group 0 drops
    /// the descriptors of its interval with their changes in progress.
    fn seeded(&mut self, g: GroupId) -> Result<Group0Result, Group0Error> {
        let m1 = self.meta1.as_mut().ok_or(Group0Error::NoSuchGroup)?;
        let (start, end) = m1.interval(g).ok_or(Group0Error::NoSuchGroup)?;
        m1.seeded.insert(g);
        let inside = |r: &RangeDesc| crate::meta2::in_interval(&r.end, &start, &end);
        let (gone, kept) =
            std::mem::take(&mut self.ranges).into_iter().partition(|(_, r)| inside(r));
        self.ranges = kept;
        let gone: BTreeMap<GroupId, RangeDesc> = gone;
        for (g, r) in gone {
            self.range_index.remove(&r.start);
            self.split_ids.remove(&g);
            self.merges.remove(&g);
            self.relocations.remove(&g);
        }
        Ok(Group0Result::Ok)
    }

    /// `SetSettings`: every change or none, if each setting is as its change expects.
    fn set_settings(
        &mut self,
        changes: &[crate::command::SettingChange],
    ) -> Result<Group0Result, Group0Error> {
        if changes.iter().any(|c| self.settings.get(&c.key) != c.expect.as_ref()) {
            return Err(Group0Error::SettingConflict);
        }
        for c in changes {
            match &c.value {
                Some(v) => self.settings.insert(c.key.clone(), v.clone()),
                None => self.settings.remove(&c.key),
            };
        }
        Ok(Group0Result::Ok)
    }

    /// `CreateTenant`: a valid name no tenant has, with the next number.
    fn create_tenant(&mut self, name: &str) -> Result<Group0Result, Group0Error> {
        if !crate::tenants::valid_name(name) {
            return Err(Group0Error::InvalidTenantName);
        }
        if self.tenants.id_of(name).is_some() {
            return Err(Group0Error::TenantExists);
        }
        self.tenants.add(name).map(Group0Result::Tenant).ok_or(Group0Error::OutOfGroupIds)
    }

    /// `DropTenant`: a tenant without databases, users or roles goes.
    fn drop_tenant(&mut self, name: &str) -> Result<Group0Result, Group0Error> {
        if !crate::tenants::valid_name(name) {
            return Err(Group0Error::InvalidTenantName);
        }
        let id = self.tenants.id_of(name).ok_or(Group0Error::TenantNotFound)?;
        let has_db = self.catalog.dbs.keys().any(|db| crate::tenants::split_db(db).0 == id);
        let has_user = self.security.users.docs().any(|d| crate::tenants::of_user(d) == id);
        let has_role = self.security.roles.docs().any(|d| crate::tenants::of_user(d) == id);
        if has_db || has_user || has_role {
            return Err(Group0Error::NotEmpty);
        }
        self.tenants.remove(id);
        Ok(Group0Result::Ok)
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
        if let Some(r) = self.handed_off(cmd) {
            return r;
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
            Group0Command::SetSettings(changes) => self.set_settings(changes),
            Group0Command::CreateTenant { name } => self.create_tenant(name),
            Group0Command::DropTenant { name } => self.drop_tenant(name),
            Group0Command::SetTenantLimits { tenant, limits } => {
                self.change_tenant(*tenant, |s| s.limits = limits.clone())
            }
            Group0Command::SetTenantSuspended { tenant, suspended } => {
                self.change_tenant(*tenant, |s| s.suspended = *suspended)
            }
            Group0Command::SetTenantAccess { tenant, sources } => {
                self.change_tenant(*tenant, |s| s.allowed_sources = sources.clone())
            }
            Group0Command::CreateRange { start, end, replicas } => {
                let (mut t, nodes) = self.table();
                t.create_range(start, end, replicas, nodes).map(Group0Result::Range)
            }
            Group0Command::SetReplicas { group, epoch, add, remove } => {
                let (mut t, nodes) = self.table();
                t.set_replicas(*group, *epoch, add, remove, nodes).map(|()| Group0Result::Ok)
            }
            Group0Command::ReplicasSettled { group, epoch } => {
                self.table().0.settle(*group, *epoch).map(|()| Group0Result::Ok)
            }
            Group0Command::SetLeaderHint { group, node, term } => {
                self.table().0.leader_hint(*group, *node, *term).map(|()| Group0Result::Ok)
            }
            Group0Command::SplitRange { group, at } => {
                let capped = self.capped_collections();
                self.table().0.split(*group, at, &capped).map(Group0Result::Range)
            }
            Group0Command::CompleteSplit { group, at, .. } => {
                self.table().0.complete_split(*group, at).map(Group0Result::Range)
            }
            Group0Command::MergeRanges { left, right } => {
                let capped = self.capped_collections();
                self.table().0.merge(index, (*left, *right), &capped).map(|()| Group0Result::Ok)
            }
            Group0Command::CompleteMerge { left, right } => {
                self.table().0.complete_merge(*left, *right).map(|()| Group0Result::Ok)
            }
            Group0Command::AbortMerge { left, right } => {
                self.table().0.end_merge(*left, *right).map(|()| Group0Result::Ok)
            }
            Group0Command::RelocateRange { group, core } => {
                let (mut t, nodes) = self.table();
                t.relocate(index, *group, *core, nodes).map(|()| Group0Result::Ok)
            }
            Group0Command::CompleteRelocate { group, core } => {
                self.table().0.complete_relocate(*group, *core).map(|()| Group0Result::Ok)
            }
            Group0Command::Catalog(c) => {
                let ddl = crate::ddl_log::catalog_changes(&self.catalog, c);
                if let CatalogCommand::Purged { id } = c {
                    self.indexes.forget_collection(*id);
                }
                let r = Arc::make_mut(&mut self.catalog)
                    .apply_at(index, mark, c)
                    .map_err(catalog_error)
                    .map(|entry| entry.map_or(Group0Result::Ok, Group0Result::Collection));
                self.record_ddl(mark, &r, ddl);
                r
            }
            Group0Command::Indexes(c) => {
                let ddl = crate::ddl_log::index_changes(&self.catalog, &self.indexes, c);
                let live: BTreeSet<u32> = self.catalog.collections.values().map(|e| e.id).collect();
                let r = self
                    .indexes
                    .apply(c, |id| live.contains(&id))
                    .map_err(catalog_error)
                    .map(|entry| entry.map_or(Group0Result::Ok, Group0Result::Index));
                self.record_ddl(mark, &r, ddl);
                r
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
            Group0Command::HandoffMeta2 { ranges } => self.handoff(ranges),
            Group0Command::Meta2Seeded { group } => self.seeded(*group),
            Group0Command::RestoreCatalog { catalog, indexes } => {
                if !self.catalog.collections.is_empty() || !self.indexes.indexes.is_empty() {
                    Err(Group0Error::NotEmpty)
                } else {
                    self.catalog = Arc::new((**catalog).clone());
                    self.indexes = (**indexes).clone();
                    Ok(Group0Result::Ok)
                }
            }
            Group0Command::Idempotent { req, cmd } => {
                if let Some(done) = self.requests.get(*req) {
                    return done.clone();
                }
                let done = self.apply_command_at(index, mark, cmd);
                self.requests.record(*req, done.clone());
                return done;
            }
        };
        r.unwrap_or_else(Group0Result::Err)
    }

    /// Records `changes` of the entry at `mark` when its command applied (`r`).
    fn record_ddl(
        &mut self,
        mark: Option<teseladb_hlc::Hlc>,
        r: &Result<Group0Result, Group0Error>,
        changes: Vec<DdlChange>,
    ) {
        let (Some(mark), Ok(result)) = (mark, r) else { return };
        let created = match result {
            Group0Result::Collection(c) => Some(c),
            _ => None,
        };
        self.ddl.record_applied(mark, changes, created);
    }

    /// The snapshot in the lowest format a node that is not removed runs (007 FR-027).
    fn encode_snapshot(&self) -> Vec<u8> {
        self.encode_snapshot_as(crate::node_versions::snapshot_format(self))
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
            put_range(&mut e, r, version > SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS);
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
        if version > SNAPSHOT_VERSION_WITHOUT_META1 {
            e.bool(self.meta1.is_some());
            if let Some(m) = &self.meta1 {
                m.encode_with(&mut e, version > SNAPSHOT_VERSION_WITHOUT_TENANTS);
            }
        }
        if version > SNAPSHOT_VERSION_WITHOUT_CHANGELOG {
            self.changelog.encode(&mut e);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_DDL {
            self.ddl.encode(&mut e);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_TENANTS {
            self.tenants.encode_with(&mut e, version > SNAPSHOT_VERSION_WITHOUT_TENANT_STATE);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_API_KEYS {
            self.security.encode_keys(&mut e);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_INDEX_PLACES {
            self.indexes.encode_places(&mut e);
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
            let desc = get_range(&mut d, version > SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS)?;
            sm.ranges.insert(desc.group, desc);
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
        if version > SNAPSHOT_VERSION_WITHOUT_META1 && d.bool()? {
            let tenants = version > SNAPSHOT_VERSION_WITHOUT_TENANTS;
            sm.meta1 = Some(Meta1::decode_with(&mut d, tenants)?);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_CHANGELOG {
            sm.changelog = Changelog::decode(&mut d)?;
        }
        if version > SNAPSHOT_VERSION_WITHOUT_DDL {
            sm.ddl = Box::new(DdlLog::decode(&mut d)?);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_TENANTS {
            let state = version > SNAPSHOT_VERSION_WITHOUT_TENANT_STATE;
            sm.tenants = Box::new(Tenants::decode_with(&mut d, state)?);
        }
        if version > SNAPSHOT_VERSION_WITHOUT_API_KEYS {
            sm.security.decode_keys(&mut d)?;
        }
        if version > SNAPSHOT_VERSION_WITHOUT_INDEX_PLACES {
            sm.indexes.decode_places(&mut d)?;
        }
        d.finish()?;
        Ok(sm)
    }
}

/// A descriptor as the snapshots of Group 0 and of the Meta2 ranges write it; `marks`: with
/// [`RangeDesc::moved_ms`] (the formats before it leave it out).
pub(crate) fn put_range(e: &mut Encoder, r: &RangeDesc, marks: bool) {
    e.u64(r.group.0).bytes(&r.start).bytes(&r.end).u16(r.core);
    put_replicas(e, &r.replicas);
    e.u64(r.replica_epoch).u64(r.generation).u64(r.leader_hint.map_or(0, |n| n.0));
    e.u64(r.leader_term);
    e.bool(r.pending_split.is_some()).bytes(r.pending_split.as_deref().unwrap_or(&[]));
    e.bool(r.settling);
    if marks {
        e.u64(r.moved_ms);
    }
}

/// Replicas as every node, then the learners.
pub(crate) fn put_replicas(e: &mut Encoder, replicas: &[(NodeId, ReplicaRole)]) {
    let nodes: Vec<NodeId> = replicas.iter().map(|(n, _)| *n).collect();
    put_node_list(e, &nodes);
    let learners: Vec<NodeId> = replicas
        .iter()
        .filter(|(_, role)| *role == ReplicaRole::Learner)
        .map(|(n, _)| *n)
        .collect();
    put_node_list(e, &learners);
}

/// Replicas written by [`put_replicas`].
pub(crate) fn get_replicas(d: &mut Decoder<'_>) -> Result<Vec<(NodeId, ReplicaRole)>, DecodeError> {
    let (nodes, learners) = (get_node_list(d)?, get_node_list(d)?);
    Ok(nodes
        .into_iter()
        .map(|n| {
            let role =
                if learners.contains(&n) { ReplicaRole::Learner } else { ReplicaRole::Voter };
            (n, role)
        })
        .collect())
}

/// A descriptor written by [`put_range`] with the same `marks`.
pub(crate) fn get_range(d: &mut Decoder<'_>, marks: bool) -> Result<RangeDesc, DecodeError> {
    let group = GroupId(d.u64()?);
    let (start, end, core) = (d.bytes()?.to_vec(), d.bytes()?.to_vec(), d.u16()?);
    let replicas = get_replicas(d)?;
    let (replica_epoch, generation, hint) = (d.u64()?, d.u64()?, d.u64()?);
    let leader_term = d.u64()?;
    let split = d.bool()?;
    let at = d.bytes()?.to_vec();
    Ok(RangeDesc {
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
        moved_ms: if marks { d.u64()? } else { 0 },
    })
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
        self.ddl.applied_mark = self.ddl.applied_mark.max(Some(ctx.ts));
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
