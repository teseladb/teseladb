//! Group 0 commands and their encoding. Each entry is `kind u16` followed by the command's
//! fields; the kinds that also existed in the origin keep their number.

use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::catalog::CatalogCommand;
use crate::clustertime_keys::SealedKey;
use crate::enroll_tokens::{
    EnrollToken, SECRET_HASH_LEN, TOKEN_ID_LEN, decode_token, decode_use, encode_token,
};
use crate::indexes::IndexCommand;
use crate::meta2::Meta2Range;
use crate::request::RequestId;
use crate::security::{SecurityOp, decode_ops, encode_ops};

/// `kind` of each command.
pub mod kind {
    /// [`super::Group0Command::Bootstrap`].
    pub const BOOTSTRAP: u16 = 1;
    /// [`super::Group0Command::AddNode`].
    pub const ADD_NODE: u16 = 2;
    /// [`super::Group0Command::SetNodeState`].
    pub const SET_NODE_STATE: u16 = 6;
    /// [`super::Group0Command::SetReplicas`].
    pub const SET_REPLICAS: u16 = 11;
    /// [`super::Group0Command::SplitRange`].
    pub const SPLIT_RANGE: u16 = 12;
    /// [`super::Group0Command::CompleteSplit`].
    pub const COMPLETE_SPLIT: u16 = 13;
    /// [`super::Group0Command::SetLeaderHint`].
    pub const SET_LEADER_HINT: u16 = 14;
    /// [`super::Group0Command::MergeRanges`].
    pub const MERGE_RANGES: u16 = 17;
    /// [`super::Group0Command::CompleteMerge`].
    pub const COMPLETE_MERGE: u16 = 18;
    /// [`super::Group0Command::AbortMerge`].
    pub const ABORT_MERGE: u16 = 19;
    /// [`super::Group0Command::RelocateRange`].
    pub const RELOCATE_RANGE: u16 = 20;
    /// [`super::Group0Command::CompleteRelocate`].
    pub const COMPLETE_RELOCATE: u16 = 21;
    /// [`super::Group0Command::SetSetting`].
    pub const SET_SETTING: u16 = 15;
    /// [`super::Group0Command::ClusterTick`].
    pub const CLUSTER_TICK: u16 = 16;
    /// [`super::Group0Command::NodeBoot`].
    pub const NODE_BOOT: u16 = 22;
    /// [`super::Group0Command::ReplicasSettled`].
    pub const REPLICAS_SETTLED: u16 = 29;
    /// [`super::Group0Command::CreateRange`].
    pub const CREATE_RANGE: u16 = 31;
    /// [`super::Group0Command::SetNodeAddrs`].
    pub const SET_NODE_ADDRS: u16 = 32;
    /// [`super::Group0Command::SetNodeTags`].
    pub const SET_NODE_TAGS: u16 = 33;
    /// [`super::Group0Command::Catalog`].
    pub const CATALOG: u16 = 40;
    /// [`super::Group0Command::Indexes`].
    pub const INDEXES: u16 = 41;
    /// [`super::Group0Command::Idempotent`].
    pub const IDEMPOTENT: u16 = 42;
    /// [`super::Group0Command::Security`].
    pub const SECURITY: u16 = 43;
    /// [`super::Group0Command::AddClusterTimeKey`].
    pub const ADD_CLUSTER_TIME_KEY: u16 = 44;
    /// [`super::Group0Command::ExpireClusterTimeKey`].
    pub const EXPIRE_CLUSTER_TIME_KEY: u16 = 45;
    /// [`super::Group0Command::CreateEnrollToken`].
    pub const CREATE_ENROLL_TOKEN: u16 = 46;
    /// [`super::Group0Command::ConsumeEnrollToken`].
    pub const CONSUME_ENROLL_TOKEN: u16 = 47;
    /// [`super::Group0Command::HandoffMeta2`].
    pub const HANDOFF_META2: u16 = 48;
    /// [`super::Group0Command::Meta2Seeded`].
    pub const META2_SEEDED: u16 = 49;
    /// [`super::Group0Command::RestoreCatalog`].
    pub const RESTORE_CATALOG: u16 = 50;
    /// [`super::Group0Command::SetSettings`].
    pub const SET_SETTINGS: u16 = 51;
    /// [`super::Group0Command::CreateTenant`].
    pub const CREATE_TENANT: u16 = 52;
    /// [`super::Group0Command::DropTenant`].
    pub const DROP_TENANT: u16 = 53;
    /// [`super::Group0Command::SetTenantLimits`].
    pub const SET_TENANT_LIMITS: u16 = 54;
    /// [`super::Group0Command::SetTenantSuspended`].
    pub const SET_TENANT_SUSPENDED: u16 = 55;
    /// [`super::Group0Command::SetTenantAccess`].
    pub const SET_TENANT_ACCESS: u16 = 56;
}

/// Addresses, name and cores of a node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeAddrs {
    /// Name.
    pub name: Vec<u8>,
    /// Inter-node address.
    pub peer_addr: Vec<u8>,
    /// HTTP (administration) address.
    pub http_addr: Vec<u8>,
    /// Address of the reference server protocol.
    pub native_addr: Vec<u8>,
    /// Cores.
    pub cores: u16,
}

impl NodeAddrs {
    fn encode(&self, e: &mut Encoder) {
        e.bytes(&self.name)
            .bytes(&self.peer_addr)
            .bytes(&self.http_addr)
            .bytes(&self.native_addr)
            .u16(self.cores);
    }

    fn decode(d: &mut Decoder<'_>) -> Result<NodeAddrs, DecodeError> {
        Ok(NodeAddrs {
            name: d.bytes()?.to_vec(),
            peer_addr: d.bytes()?.to_vec(),
            http_addr: d.bytes()?.to_vec(),
            native_addr: d.bytes()?.to_vec(),
            cores: d.u16()?,
        })
    }
}

/// Administrative state of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Active.
    Active,
    /// Its replicas are being moved away.
    Draining,
    /// Removed.
    Removed,
}

impl NodeState {
    fn code(self) -> u8 {
        match self {
            NodeState::Active => 0,
            NodeState::Draining => 1,
            NodeState::Removed => 2,
        }
    }

    fn from_code(c: u8) -> Option<NodeState> {
        match c {
            0 => Some(NodeState::Active),
            1 => Some(NodeState::Draining),
            2 => Some(NodeState::Removed),
            _ => None,
        }
    }
}

/// A Group 0 command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Group0Command {
    /// First entry of the cluster.
    Bootstrap {
        /// Cluster identifier.
        cluster_id: [u8; 16],
        /// First node.
        first_node: NodeAddrs,
        /// Fingerprint of the first node's certificate (zeros with `--insecure`).
        cert_fingerprint: [u8; 32],
    },
    /// A node joins (idempotent by certificate fingerprint).
    AddNode {
        /// Addresses and cores.
        node: NodeAddrs,
        /// Fingerprint of its certificate.
        cert_fingerprint: [u8; 32],
    },
    /// Administrative state of a node.
    SetNodeState {
        /// Node.
        node: NodeId,
        /// State.
        state: NodeState,
    },
    /// A node that came back with another name or other addresses (001 FR-022); its cores do not
    /// change.
    SetNodeAddrs {
        /// Node.
        node: NodeId,
        /// Its new name and addresses.
        addrs: NodeAddrs,
    },
    /// The tags a node started with (`--tags`), which reads with a `readPreference` match against
    /// its replicas (007 FR-013): pairs sorted by name, each name once.
    SetNodeTags {
        /// Node.
        node: NodeId,
        /// Its tags.
        tags: Vec<(String, String)>,
    },
    /// A node started with this `boot_epoch` from its `node.meta`.
    NodeBoot {
        /// Node.
        node: NodeId,
        /// Epoch.
        boot_epoch: u32,
    },
    /// Cluster time, proposed by Group 0's leader.
    ClusterTick {
        /// Milliseconds.
        cluster_time_ms: u64,
    },
    /// Cluster setting.
    SetSetting {
        /// Key.
        key: Vec<u8>,
        /// Value.
        value: Vec<u8>,
    },
    /// A new range with opaque bounds `[start, end)` (`end` empty = no end).
    CreateRange {
        /// First key.
        start: Vec<u8>,
        /// First key past the range.
        end: Vec<u8>,
        /// Nodes that hold it, all voters.
        replicas: Vec<NodeId>,
    },
    /// The balancer changes a range's replicas: the new ones start as learners.
    SetReplicas {
        /// Range.
        group: GroupId,
        /// Replica epoch the change was computed with.
        epoch: u64,
        /// Nodes that become replicas.
        add: Vec<NodeId>,
        /// Nodes that stop being replicas.
        remove: Vec<NodeId>,
    },
    /// The replica change of that epoch is complete: its learners are voters.
    ReplicasSettled {
        /// Range.
        group: GroupId,
        /// Its replica epoch.
        epoch: u64,
    },
    /// A range's leader: the one that published itself in `term`, or, with term 0, where the
    /// balancer asks its leadership to go (007 FR-010).
    SetLeaderHint {
        /// Range.
        group: GroupId,
        /// Node.
        node: NodeId,
        /// Term of the leader that published itself; 0 for the balancer's request.
        term: u64,
    },
    /// Records the intent of splitting a range at key `at`.
    SplitRange {
        /// Range.
        group: GroupId,
        /// First key of the new range.
        at: Vec<u8>,
    },
    /// The split is cut at `at_index` of the range: Group 0 creates the new range `[at, end)`.
    CompleteSplit {
        /// Range that was split.
        group: GroupId,
        /// First key of the new range.
        at: Vec<u8>,
        /// Index of the range's log where its state was cut.
        at_index: u64,
    },
    /// Records the intent of merging range `right` into its left neighbour `left` (007 FR-009).
    MergeRanges {
        /// The range that absorbs the other.
        left: GroupId,
        /// The range absorbed.
        right: GroupId,
    },
    /// `left` applied `Merge`: it holds `right`'s keys and `right` goes.
    CompleteMerge {
        /// The range that absorbed the other.
        left: GroupId,
        /// The range absorbed.
        right: GroupId,
    },
    /// `left`'s log gave up the merge: no `Merge` of this intent will apply, and `right` thaws.
    AbortMerge {
        /// The range that was to absorb the other.
        left: GroupId,
        /// The range that was to be absorbed.
        right: GroupId,
    },
    /// Records the intent of moving range `group` to core `core` of every node (007 FR-011).
    RelocateRange {
        /// The range.
        group: GroupId,
        /// The core it moves to.
        core: u16,
    },
    /// Every replica of `group` runs on `core`: the descriptor takes that core.
    CompleteRelocate {
        /// The range.
        group: GroupId,
        /// The core it moved to.
        core: u16,
    },
    /// A change of the catalog (006 FR-027, 007).
    Catalog(CatalogCommand),
    /// A change of the index definitions of the catalog (007 FR-001).
    Indexes(IndexCommand),
    /// A command applied once per request identifier (007 contract `catalog.md`); it never
    /// wraps another `Idempotent`.
    Idempotent {
        /// Request identifier.
        req: RequestId,
        /// The command.
        cmd: Box<Group0Command>,
    },
    /// An atomic batch of changes of the users and roles (013 FR-009, FR-013).
    Security(Vec<SecurityOp>),
    /// A `$clusterTime` signing key, added while the latest expiry is `after` (013 FR-014).
    AddClusterTimeKey {
        /// The key, sealed.
        key: SealedKey,
        /// The latest expiry the leader saw.
        after: u32,
    },
    /// Drops an expired signing key.
    ExpireClusterTimeKey {
        /// Its id.
        id: i64,
    },
    /// A node enrollment token (013 FR-005).
    CreateEnrollToken(EnrollToken),
    /// A node uses an enrollment token with the SHA-256 of its secret.
    ConsumeEnrollToken {
        /// The token's id.
        id: [u8; TOKEN_ID_LEN],
        /// SHA-256 of the secret the node gave.
        secret_hash: [u8; SECRET_HASH_LEN],
    },
    /// The descriptors go to these Meta2 ranges, which split space `04` in order, the last with
    /// no end (007 FR-014); Group 0 numbers them when it applies the entry.
    HandoffMeta2 {
        /// The Meta2 ranges.
        ranges: Vec<Meta2Range>,
    },
    /// Meta2 range `group` committed its first entry: Group 0 drops its interval's descriptors.
    Meta2Seeded {
        /// The Meta2 range.
        group: GroupId,
    },
    /// A backup's catalog and index definitions, whole, with their numbers (007 FR-021); only
    /// into a catalog without collections or indexes.
    RestoreCatalog {
        /// The catalog.
        catalog: Box<crate::catalog::Catalog>,
        /// The index definitions.
        indexes: Box<crate::indexes::Indexes>,
        /// The tenants, users, roles and API keys of the backup (FR-037); a backup without them
        /// leaves the cluster's.
        security: Option<Box<RestoredSecurity>>,
    },
    /// Settings changed together (007 FR-017, the documents of `config.settings`): each one
    /// expects its value as it was read (`None`: absent), and none changes unless all are so;
    /// a value `None` removes the setting.
    SetSettings(Vec<SettingChange>),
    /// Adds tenant `name` with a number never given before (007 FR-025).
    CreateTenant {
        /// Its name.
        name: String,
    },
    /// Takes tenant `name` out, if it has no database and no user left (007 FR-025).
    DropTenant {
        /// Its name.
        name: String,
    },
    /// The limits, switches and service user of tenant `tenant` (007 FR-030, FR-036). It names the
    /// tenant by its number, never given twice, so a tenant dropped and created again with the same
    /// name is never changed by an entry meant for the one before.
    SetTenantLimits {
        /// Its number.
        tenant: u32,
        /// Its limits, all of them.
        limits: crate::tenants::TenantLimits,
    },
    /// Suspends or reactivates tenant `tenant` (007 FR-034).
    SetTenantSuspended {
        /// Its number.
        tenant: u32,
        /// Whether it is suspended.
        suspended: bool,
    },
    /// The sources the users of tenant `tenant` may connect from, as CIDR blocks; empty is from
    /// anywhere (007 FR-035).
    SetTenantAccess {
        /// Its number.
        tenant: u32,
        /// The CIDR blocks.
        sources: Vec<String>,
    },
}

/// A setting of [`Group0Command::SetSettings`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingChange {
    /// The setting.
    pub key: Vec<u8>,
    /// Its value as it was read (`None`: absent).
    pub expect: Option<Vec<u8>>,
    /// Its new value (`None`: removed).
    pub value: Option<Vec<u8>>,
}

fn put_opt(e: &mut Encoder, v: Option<&[u8]>) {
    e.bool(v.is_some());
    if let Some(b) = v {
        e.bytes(b);
    }
}

fn get_opt(d: &mut Decoder<'_>) -> Result<Option<Vec<u8>>, DecodeError> {
    Ok(if d.bool()? { Some(d.bytes()?.to_vec()) } else { None })
}

fn put_nodes(e: &mut Encoder, nodes: &[NodeId]) {
    e.var(nodes.len() as u64);
    for n in nodes {
        e.u64(n.0);
    }
}

/// Tags: a count and its name and value pairs, in UTF-8.
pub(crate) fn get_tags(d: &mut Decoder<'_>) -> Result<Vec<(String, String)>, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    // Two lengths at least per pair.
    if n > (d.remaining() / 2) as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    let text = |d: &mut Decoder<'_>| {
        let at = d.position();
        String::from_utf8(d.bytes()?.to_vec()).map_err(|_| DecodeError::InvalidValue { offset: at })
    };
    (0..n).map(|_| Ok((text(d)?, text(d)?))).collect()
}

/// Writes tags as [`get_tags`] reads them.
pub(crate) fn put_tags(e: &mut Encoder, tags: &[(String, String)]) {
    e.var(tags.len() as u64);
    for (name, value) in tags {
        e.bytes(name.as_bytes()).bytes(value.as_bytes());
    }
}

fn get_nodes(d: &mut Decoder<'_>) -> Result<Vec<NodeId>, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    if n > (d.remaining() / 8) as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    (0..n).map(|_| d.u64().map(NodeId)).collect()
}

fn get_array<const N: usize>(d: &mut Decoder<'_>) -> Result<[u8; N], DecodeError> {
    let mut a = [0u8; N];
    a.copy_from_slice(d.raw(N)?);
    Ok(a)
}

impl Group0Command {
    /// Its `kind`.
    pub fn kind(&self) -> u16 {
        match self {
            Group0Command::Bootstrap { .. } => kind::BOOTSTRAP,
            Group0Command::AddNode { .. } => kind::ADD_NODE,
            Group0Command::SetNodeState { .. } => kind::SET_NODE_STATE,
            Group0Command::NodeBoot { .. } => kind::NODE_BOOT,
            Group0Command::SetNodeAddrs { .. } => kind::SET_NODE_ADDRS,
            Group0Command::SetNodeTags { .. } => kind::SET_NODE_TAGS,
            Group0Command::ClusterTick { .. } => kind::CLUSTER_TICK,
            Group0Command::SetSetting { .. } => kind::SET_SETTING,
            Group0Command::CreateRange { .. } => kind::CREATE_RANGE,
            Group0Command::SetReplicas { .. } => kind::SET_REPLICAS,
            Group0Command::ReplicasSettled { .. } => kind::REPLICAS_SETTLED,
            Group0Command::SetLeaderHint { .. } => kind::SET_LEADER_HINT,
            Group0Command::SplitRange { .. } => kind::SPLIT_RANGE,
            Group0Command::CompleteSplit { .. } => kind::COMPLETE_SPLIT,
            Group0Command::MergeRanges { .. } => kind::MERGE_RANGES,
            Group0Command::CompleteMerge { .. } => kind::COMPLETE_MERGE,
            Group0Command::AbortMerge { .. } => kind::ABORT_MERGE,
            Group0Command::RelocateRange { .. } => kind::RELOCATE_RANGE,
            Group0Command::CompleteRelocate { .. } => kind::COMPLETE_RELOCATE,
            Group0Command::Catalog(_) => kind::CATALOG,
            Group0Command::Indexes(_) => kind::INDEXES,
            Group0Command::Idempotent { .. } => kind::IDEMPOTENT,
            Group0Command::Security(_) => kind::SECURITY,
            Group0Command::AddClusterTimeKey { .. } => kind::ADD_CLUSTER_TIME_KEY,
            Group0Command::ExpireClusterTimeKey { .. } => kind::EXPIRE_CLUSTER_TIME_KEY,
            Group0Command::CreateEnrollToken(_) => kind::CREATE_ENROLL_TOKEN,
            Group0Command::ConsumeEnrollToken { .. } => kind::CONSUME_ENROLL_TOKEN,
            Group0Command::HandoffMeta2 { .. } => kind::HANDOFF_META2,
            Group0Command::Meta2Seeded { .. } => kind::META2_SEEDED,
            Group0Command::RestoreCatalog { .. } => kind::RESTORE_CATALOG,
            Group0Command::SetSettings(_) => kind::SET_SETTINGS,
            Group0Command::CreateTenant { .. } => kind::CREATE_TENANT,
            Group0Command::DropTenant { .. } => kind::DROP_TENANT,
            Group0Command::SetTenantLimits { .. } => kind::SET_TENANT_LIMITS,
            Group0Command::SetTenantSuspended { .. } => kind::SET_TENANT_SUSPENDED,
            Group0Command::SetTenantAccess { .. } => kind::SET_TENANT_ACCESS,
        }
    }

    /// Encodes the command as a Group 0 entry.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u16(self.kind());
        match self {
            Group0Command::Bootstrap { cluster_id, first_node, cert_fingerprint } => {
                e.raw(cluster_id);
                first_node.encode(&mut e);
                e.raw(cert_fingerprint);
            }
            Group0Command::AddNode { node, cert_fingerprint } => {
                node.encode(&mut e);
                e.raw(cert_fingerprint);
            }
            Group0Command::SetNodeState { node, state } => {
                e.u64(node.0).u8(state.code());
            }
            Group0Command::NodeBoot { node, boot_epoch } => {
                e.u64(node.0).u32(*boot_epoch);
            }
            Group0Command::SetNodeAddrs { node, addrs } => {
                e.u64(node.0);
                addrs.encode(&mut e);
            }
            Group0Command::SetNodeTags { node, tags } => {
                e.u64(node.0);
                put_tags(&mut e, tags);
            }
            Group0Command::ClusterTick { cluster_time_ms } => {
                e.u64(*cluster_time_ms);
            }
            Group0Command::SetSetting { key, value } => {
                e.bytes(key).bytes(value);
            }
            Group0Command::CreateRange { start, end, replicas } => {
                e.bytes(start).bytes(end);
                put_nodes(&mut e, replicas);
            }
            Group0Command::SetReplicas { group, epoch, add, remove } => {
                e.u64(group.0).u64(*epoch);
                put_nodes(&mut e, add);
                put_nodes(&mut e, remove);
            }
            Group0Command::ReplicasSettled { group, epoch } => {
                e.u64(group.0).u64(*epoch);
            }
            Group0Command::SetLeaderHint { group, node, term } => {
                e.u64(group.0).u64(node.0).u64(*term);
            }
            Group0Command::SplitRange { group, at } => {
                e.u64(group.0).bytes(at);
            }
            Group0Command::CompleteSplit { group, at, at_index } => {
                e.u64(group.0).bytes(at).u64(*at_index);
            }
            Group0Command::MergeRanges { left, right }
            | Group0Command::CompleteMerge { left, right }
            | Group0Command::AbortMerge { left, right } => {
                e.u64(left.0).u64(right.0);
            }
            Group0Command::RelocateRange { group, core }
            | Group0Command::CompleteRelocate { group, core } => {
                e.u64(group.0).u16(*core);
            }
            Group0Command::Catalog(c) => c.encode(&mut e),
            Group0Command::Indexes(c) => c.encode(&mut e),
            Group0Command::HandoffMeta2 { ranges } => {
                e.var(ranges.len() as u64);
                for r in ranges {
                    r.encode(&mut e);
                }
            }
            Group0Command::Meta2Seeded { group } => {
                e.u64(group.0);
            }
            Group0Command::RestoreCatalog { catalog, indexes, security } => {
                encode_restore(&mut e, catalog, indexes, security.as_deref());
            }
            Group0Command::CreateTenant { name } | Group0Command::DropTenant { name } => {
                e.str(name);
            }
            Group0Command::SetTenantLimits { tenant, limits } => {
                e.u32(*tenant);
                limits.encode(&mut e);
            }
            Group0Command::SetTenantSuspended { tenant, suspended } => {
                e.u32(*tenant).bool(*suspended);
            }
            Group0Command::SetTenantAccess { tenant, sources } => {
                e.u32(*tenant).var(sources.len() as u64);
                for s in sources {
                    e.str(s);
                }
            }
            Group0Command::SetSettings(changes) => {
                e.var(changes.len() as u64);
                for c in changes {
                    e.bytes(&c.key);
                    put_opt(&mut e, c.expect.as_deref());
                    put_opt(&mut e, c.value.as_deref());
                }
            }
            Group0Command::Idempotent { req, cmd } => {
                e.u64(req.origin.0).u64(req.id).raw(&cmd.encode());
            }
            Group0Command::Security(ops) => encode_ops(ops, &mut e),
            Group0Command::AddClusterTimeKey { key, after } => {
                key.encode(&mut e);
                e.u32(*after);
            }
            Group0Command::ExpireClusterTimeKey { id } => {
                e.i64(*id);
            }
            Group0Command::CreateEnrollToken(t) => encode_token(t, &mut e),
            Group0Command::ConsumeEnrollToken { id, secret_hash } => {
                e.raw(id).raw(secret_hash);
            }
        }
        e.finish()
    }

    /// Decodes an entry written by [`Group0Command::encode`].
    pub fn decode(b: &[u8]) -> Result<Group0Command, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let c = match d.u16()? {
            kind::BOOTSTRAP => Group0Command::Bootstrap {
                cluster_id: get_array(&mut d)?,
                first_node: NodeAddrs::decode(&mut d)?,
                cert_fingerprint: get_array(&mut d)?,
            },
            kind::ADD_NODE => Group0Command::AddNode {
                node: NodeAddrs::decode(&mut d)?,
                cert_fingerprint: get_array(&mut d)?,
            },
            kind::SET_NODE_STATE => {
                let node = NodeId(d.u64()?);
                let at = d.position();
                let state = NodeState::from_code(d.u8()?)
                    .ok_or(DecodeError::InvalidValue { offset: at })?;
                Group0Command::SetNodeState { node, state }
            }
            kind::NODE_BOOT => {
                Group0Command::NodeBoot { node: NodeId(d.u64()?), boot_epoch: d.u32()? }
            }
            kind::SET_NODE_ADDRS => Group0Command::SetNodeAddrs {
                node: NodeId(d.u64()?),
                addrs: NodeAddrs::decode(&mut d)?,
            },
            kind::SET_NODE_TAGS => {
                Group0Command::SetNodeTags { node: NodeId(d.u64()?), tags: get_tags(&mut d)? }
            }
            kind::CLUSTER_TICK => Group0Command::ClusterTick { cluster_time_ms: d.u64()? },
            kind::SET_SETTING => {
                Group0Command::SetSetting { key: d.bytes()?.to_vec(), value: d.bytes()?.to_vec() }
            }
            kind::CREATE_RANGE => Group0Command::CreateRange {
                start: d.bytes()?.to_vec(),
                end: d.bytes()?.to_vec(),
                replicas: get_nodes(&mut d)?,
            },
            kind::SET_REPLICAS => Group0Command::SetReplicas {
                group: GroupId(d.u64()?),
                epoch: d.u64()?,
                add: get_nodes(&mut d)?,
                remove: get_nodes(&mut d)?,
            },
            kind::REPLICAS_SETTLED => {
                Group0Command::ReplicasSettled { group: GroupId(d.u64()?), epoch: d.u64()? }
            }
            kind::SET_LEADER_HINT => Group0Command::SetLeaderHint {
                group: GroupId(d.u64()?),
                node: NodeId(d.u64()?),
                term: d.u64()?,
            },
            kind::SPLIT_RANGE => {
                Group0Command::SplitRange { group: GroupId(d.u64()?), at: d.bytes()?.to_vec() }
            }
            kind::COMPLETE_SPLIT => Group0Command::CompleteSplit {
                group: GroupId(d.u64()?),
                at: d.bytes()?.to_vec(),
                at_index: d.u64()?,
            },
            kind::MERGE_RANGES => {
                Group0Command::MergeRanges { left: GroupId(d.u64()?), right: GroupId(d.u64()?) }
            }
            kind::COMPLETE_MERGE => {
                Group0Command::CompleteMerge { left: GroupId(d.u64()?), right: GroupId(d.u64()?) }
            }
            kind::ABORT_MERGE => {
                Group0Command::AbortMerge { left: GroupId(d.u64()?), right: GroupId(d.u64()?) }
            }
            kind::RELOCATE_RANGE => {
                Group0Command::RelocateRange { group: GroupId(d.u64()?), core: d.u16()? }
            }
            kind::COMPLETE_RELOCATE => {
                Group0Command::CompleteRelocate { group: GroupId(d.u64()?), core: d.u16()? }
            }
            kind::HANDOFF_META2 => {
                // Grown one by one: a count read from the entry reserves nothing.
                let mut ranges = Vec::new();
                for _ in 0..d.var()? {
                    ranges.push(Meta2Range::decode(&mut d)?);
                }
                Group0Command::HandoffMeta2 { ranges }
            }
            kind::META2_SEEDED => Group0Command::Meta2Seeded { group: GroupId(d.u64()?) },
            kind::RESTORE_CATALOG => {
                let (catalog, indexes, security) = decode_restore(&mut d)?;
                Group0Command::RestoreCatalog {
                    catalog: Box::new(catalog),
                    indexes: Box::new(indexes),
                    security: security.map(Box::new),
                }
            }
            kind::SET_SETTINGS => {
                let mut changes = Vec::new();
                for _ in 0..d.var()? {
                    let key = d.bytes()?.to_vec();
                    changes.push(SettingChange {
                        key,
                        expect: get_opt(&mut d)?,
                        value: get_opt(&mut d)?,
                    });
                }
                Group0Command::SetSettings(changes)
            }
            kind::CREATE_TENANT => Group0Command::CreateTenant { name: d.str()?.to_owned() },
            kind::DROP_TENANT => Group0Command::DropTenant { name: d.str()?.to_owned() },
            kind::SET_TENANT_LIMITS => Group0Command::SetTenantLimits {
                tenant: d.u32()?,
                limits: crate::tenants::TenantLimits::decode(&mut d)?,
            },
            kind::SET_TENANT_SUSPENDED => {
                Group0Command::SetTenantSuspended { tenant: d.u32()?, suspended: d.bool()? }
            }
            kind::SET_TENANT_ACCESS => {
                let tenant = d.u32()?;
                let mut sources = Vec::new();
                for _ in 0..d.var()? {
                    sources.push(d.str()?.to_owned());
                }
                Group0Command::SetTenantAccess { tenant, sources }
            }
            kind::CATALOG => Group0Command::Catalog(CatalogCommand::decode(&mut d)?),
            kind::INDEXES => Group0Command::Indexes(IndexCommand::decode(&mut d)?),
            kind::SECURITY => Group0Command::Security(decode_ops(&mut d)?),
            kind::ADD_CLUSTER_TIME_KEY => Group0Command::AddClusterTimeKey {
                key: SealedKey::decode(&mut d)?,
                after: d.u32()?,
            },
            kind::EXPIRE_CLUSTER_TIME_KEY => Group0Command::ExpireClusterTimeKey { id: d.i64()? },
            kind::CREATE_ENROLL_TOKEN => Group0Command::CreateEnrollToken(decode_token(&mut d)?),
            kind::CONSUME_ENROLL_TOKEN => {
                let (id, secret_hash) = decode_use(&mut d)?;
                Group0Command::ConsumeEnrollToken { id, secret_hash }
            }
            kind::IDEMPOTENT => {
                let req = RequestId { origin: NodeId(d.u64()?), id: d.u64()? };
                let inner_at = d.position();
                let cmd = Group0Command::decode(d.raw(d.remaining())?)?;
                if matches!(cmd, Group0Command::Idempotent { .. }) {
                    return Err(DecodeError::InvalidValue { offset: inner_at });
                }
                Group0Command::Idempotent { req, cmd: Box::new(cmd) }
            }
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        d.finish()?;
        Ok(c)
    }
}

/// Encoding of [`NodeAddrs`] and [`NodeState`] for the snapshot of Group 0.
pub(crate) mod snapshot_codec {
    use super::*;

    pub(crate) fn put_addrs(e: &mut Encoder, a: &NodeAddrs) {
        a.encode(e);
    }

    pub(crate) fn get_addrs(d: &mut Decoder<'_>) -> Result<NodeAddrs, DecodeError> {
        NodeAddrs::decode(d)
    }

    pub(crate) fn state_code(s: NodeState) -> u8 {
        s.code()
    }

    pub(crate) fn get_state(d: &mut Decoder<'_>) -> Result<NodeState, DecodeError> {
        let at = d.position();
        NodeState::from_code(d.u8()?).ok_or(DecodeError::InvalidValue { offset: at })
    }

    pub(crate) fn put_node_list(e: &mut Encoder, nodes: &[NodeId]) {
        put_nodes(e, nodes);
    }

    pub(crate) fn get_node_list(d: &mut Decoder<'_>) -> Result<Vec<NodeId>, DecodeError> {
        get_nodes(d)
    }

    pub(crate) fn get_bytes_array<const N: usize>(
        d: &mut Decoder<'_>,
    ) -> Result<[u8; N], DecodeError> {
        get_array(d)
    }
}

/// The tenants and the security state of a backup (007 FR-037): the tenants with their limits,
/// access lists and service users, and the users, roles and API keys (only the SHA-256 of each).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoredSecurity {
    /// The tenants.
    pub tenants: crate::tenants::Tenants,
    /// The users, roles and API keys.
    pub security: crate::security::Security,
}

/// Writes a backup's catalog, index definitions and, if given, its tenants and security state,
/// as `RestoreCatalog` and the backup's manifest carry them.
pub fn encode_restore(
    e: &mut Encoder,
    catalog: &crate::catalog::Catalog,
    indexes: &crate::indexes::Indexes,
    security: Option<&RestoredSecurity>,
) {
    catalog.encode(e);
    indexes.encode(e);
    if let Some(s) = security {
        s.tenants.encode(e);
        s.security.encode(e);
        s.security.encode_keys(e);
    }
}

/// Reads what [`encode_restore`] wrote: a backup made before 007 T066 has no tenants or security.
pub fn decode_restore(
    d: &mut Decoder<'_>,
) -> Result<(crate::catalog::Catalog, crate::indexes::Indexes, Option<RestoredSecurity>), DecodeError>
{
    let catalog = crate::catalog::Catalog::decode(d)?;
    let indexes = crate::indexes::Indexes::decode(d)?;
    if d.remaining() == 0 {
        return Ok((catalog, indexes, None));
    }
    let tenants = crate::tenants::Tenants::decode(d)?;
    let mut security = crate::security::Security::decode(d)?;
    security.decode_keys(d)?;
    Ok((catalog, indexes, Some(RestoredSecurity { tenants, security })))
}
