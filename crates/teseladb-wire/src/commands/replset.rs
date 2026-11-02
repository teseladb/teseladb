//! The replica set presentation (018 FR-007, FR-008, contract `topology-modes.md`): its
//! configuration from the cluster parameter `topologyPresentation` and the cluster's nodes,
//! `local.system.replset`, the `replSet*` commands and what a member refuses.
//!
//! Measured on a replica set of three members of RefDb 9.0.4 (`ref:9.0`, 02-11-2026): a
//! secondary refuses writes and DDL with `NotWritablePrimary` (`not primary`, with the
//! `topologyVersion`); the router's commands do not exist on a member (`no such command`);
//! `replSetInitiate` answers `already initialized`; `replSetFreeze`, `replSetMaintenance` and
//! `replSetSyncFrom` refuse on the primary with 95; `replSetAbortPrimaryCatchUp` outside a
//! catch-up is 20; `replSetStepDown` on a secondary is 10107.

use teseladb_bson::{Document, Value};
use teseladb_topology::replset::{Member, MemberId, ReplicaSet};

use super::presentation::{Presentation, TOPOLOGY_PRESENTATION, hosts, member_of, stored};
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, COMMAND_NOT_FOUND, Code, CommandError, ILLEGAL_OPERATION, INVALID_OPTIONS,
    NOT_WRITABLE_PRIMARY,
};

/// `replSetGetConfig`.
pub const REPL_SET_GET_CONFIG: &str = "replSetGetConfig";
/// `replSetStepDown`.
pub const REPL_SET_STEP_DOWN: &str = "replSetStepDown";
/// `replSetReconfig`.
pub const REPL_SET_RECONFIG: &str = "replSetReconfig";
/// `replSetFreeze`.
pub const REPL_SET_FREEZE: &str = "replSetFreeze";
/// `replSetMaintenance`.
pub const REPL_SET_MAINTENANCE: &str = "replSetMaintenance";
/// `replSetInitiate`.
pub const REPL_SET_INITIATE: &str = "replSetInitiate";
/// `replSetResizeOplog`.
pub const REPL_SET_RESIZE_OPLOG: &str = "replSetResizeOplog";
/// `replSetSyncFrom`.
pub const REPL_SET_SYNC_FROM: &str = "replSetSyncFrom";
/// `replSetAbortPrimaryCatchUp`.
pub const REPL_SET_ABORT_PRIMARY_CATCH_UP: &str = "replSetAbortPrimaryCatchUp";

/// The `replSet*` commands of this module (`replSetGetStatus` is in [`super::admin`]).
pub const NAMES: [&str; 9] = [
    REPL_SET_GET_CONFIG,
    REPL_SET_STEP_DOWN,
    REPL_SET_RECONFIG,
    REPL_SET_FREEZE,
    REPL_SET_MAINTENANCE,
    REPL_SET_INITIATE,
    REPL_SET_RESIZE_OPLOG,
    REPL_SET_SYNC_FROM,
    REPL_SET_ABORT_PRIMARY_CATCH_UP,
];

/// The commands a secondary refuses with `NotWritablePrimary` (measured: writes and DDL).
const PRIMARY_ONLY: [&str; 14] = [
    "insert",
    "update",
    "delete",
    "findAndModify",
    "bulkWrite",
    "create",
    "drop",
    "dropDatabase",
    "createIndexes",
    "dropIndexes",
    "renameCollection",
    "collMod",
    super::apply_ops::APPLY_OPS,
    "convertToCapped",
];

/// The router's commands, which a member of a replica set does not have.
const ROUTER_ONLY: [&str; 33] = [
    "isdbgrid",
    "listShards",
    "addShard",
    "removeShard",
    "addShardToZone",
    "removeShardFromZone",
    "updateZoneKeyRange",
    "shardCollection",
    "enableSharding",
    "reshardCollection",
    "refineCollectionShardKey",
    "movePrimary",
    "moveChunk",
    "moveRange",
    "split",
    "splitVector",
    "mergeChunks",
    "clearJumboFlag",
    "balancerStart",
    "balancerStop",
    "balancerStatus",
    "balancerCollectionStatus",
    "configureCollectionBalancing",
    "getShardMap",
    "getShardVersion",
    "flushRouterConfig",
    "checkMetadataConsistency",
    "transitionToDedicatedConfigServer",
    "startShardDraining",
    "stopShardDraining",
    "shardDrainingStatus",
    "commitShardRemoval",
    "abortReshardCollection",
];

/// `CannotFreeze`-like refusals of the primary (code 95, `NotSecondary`).
const NOT_SECONDARY: Code = Code { code: 95, name: "NotSecondary" };
/// `AlreadyInitialized`.
const ALREADY_INITIALIZED: Code = Code { code: 23, name: "AlreadyInitialized" };
/// `InvalidReplicaSetConfig`.
const INVALID_REPLICA_SET_CONFIG: Code = Code { code: 93, name: "InvalidReplicaSetConfig" };
/// `NewReplicaSetConfigurationIncompatible`.
const NEW_CONFIG_INCOMPATIBLE: Code =
    Code { code: 103, name: "NewReplicaSetConfigurationIncompatible" };
/// `AlreadyInitialized` is not maintenance's: out of maintenance mode is `OperationFailed`.
const OPERATION_FAILED: Code = Code { code: 96, name: "OperationFailed" };
/// `InvalidSyncSource`.
const INVALID_SYNC_SOURCE: Code = Code { code: 72, name: "InvalidSyncSource" };

/// What a member refuses before it runs command `name`: writes on a secondary and the
/// router's commands (measured on 9.0.4).
pub fn gate(
    set: &ReplicaSet,
    me: MemberId,
    name: &str,
    topology: Document,
) -> Result<(), CommandError> {
    if ROUTER_ONLY.contains(&name) {
        return Err(CommandError::new(COMMAND_NOT_FOUND, format!("no such command: '{name}'")));
    }
    let primary = set.primary().is_some_and(|p| set.member(p) == Some(me));
    if !primary && PRIMARY_ONLY.contains(&name) {
        let mut e = CommandError::new(NOT_WRITABLE_PRIMARY, "not primary");
        e.extra = Document::new().with("topologyVersion", topology);
        return Err(e);
    }
    Ok(())
}

/// `local.system.replset`: the configuration, in the replica set presentation only.
pub fn system_replset<B: Backend + ?Sized>(
    backend: &mut B,
) -> Result<Option<Document>, CommandError> {
    Ok(super::presentation::replica_set(backend)?.map(|(rs, id)| rs.config_document(id)))
}

/// The set, this node's member and the stored presentation, or the refusal of a node that
/// presents itself as a `refrouter`.
fn member_view<B: Backend + ?Sized>(
    backend: &mut B,
    name: &str,
) -> Result<(ReplicaSet, MemberId, Presentation, [u8; 12]), CommandError> {
    let Some((rs, id)) = super::presentation::replica_set(backend)? else {
        return Err(CommandError::new(COMMAND_NOT_FOUND, format!("no such command: '{name}'")));
    };
    let me = my_host(backend)?;
    let member = rs
        .member(&me)
        .ok_or_else(|| CommandError::new(ILLEGAL_OPERATION, "this node is not a member"))?;
    Ok((rs, member, stored(backend)?, id))
}

/// This node's host, as `config.shards` gives it.
fn my_host<B: Backend + ?Sized>(backend: &mut B) -> Result<String, CommandError> {
    let id = backend.node();
    let node = i64::try_from(id).unwrap_or(i64::MAX);
    let name = Value::from(super::shard_name(id).as_str());
    let shards = backend.config_documents(super::config_db::SHARDS)?;
    Ok(shards
        .iter()
        .find(|d| d.get("node") == Some(&Value::Int64(node)) || d.get("_id") == Some(&name))
        .and_then(|d| match d.get("host") {
            Some(Value::String(h)) => Some(h.clone()),
            _ => None,
        })
        .unwrap_or_default())
}

fn is_primary(rs: &ReplicaSet, me: MemberId) -> bool {
    rs.primary().is_some_and(|p| rs.member(p) == Some(me))
}

fn store<B: Backend + ?Sized>(backend: &mut B, p: &Presentation) -> Result<(), CommandError> {
    backend.set_cluster_parameter(TOPOLOGY_PRESENTATION, p.to_document().to_bytes()?)
}

/// `replSetGetConfig`.
pub fn get_config<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_GET_CONFIG, ctx.db)?;
    let (rs, _, _, id) = member_view(ctx.backend, REPL_SET_GET_CONFIG)?;
    Ok(Document::new().with("config", rs.config_document(id)).with("ok", 1.0))
}

/// `replSetStepDown`: the primary hands over to another live electable member with a new term,
/// as an election does.
pub fn step_down<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_STEP_DOWN, ctx.db)?;
    let (rs, me, p, _) = member_view(ctx.backend, REPL_SET_STEP_DOWN)?;
    if !is_primary(&rs, me) {
        return Err(CommandError::new(NOT_WRITABLE_PRIMARY, "not primary so can't step down"));
    }
    let Some(next) = rs.step_down(&|_| true) else {
        return Err(CommandError::new(
            Code { code: 262, name: "ExceededTimeLimit" },
            "No electable secondaries caught up as of the step down deadline",
        ));
    };
    let p = Presentation { primary: next.primary().map(str::to_owned), term: next.term(), ..p };
    store(ctx.backend, &p)?;
    Ok(Document::new().with("ok", 1.0))
}

fn member_error(d: &Document, why: &str) -> CommandError {
    CommandError::new(
        INVALID_REPLICA_SET_CONFIG,
        format!("member: {} :: caused by :: {why}", super::display(&Value::Document(d.clone()))),
    )
}

/// A member of `replSetReconfig` checked as RefDb 9.0.4 checks it.
fn reconfig_member(d: &Document) -> Result<Member, CommandError> {
    let num = |f: &str| match d.get(f) {
        Some(Value::Int32(n)) => Some(f64::from(*n)),
        Some(Value::Int64(n)) => Some(*n as f64),
        Some(Value::Double(x)) => Some(*x),
        _ => None,
    };
    if let Some(p) = num("priority").filter(|p| *p > 1000.0) {
        return Err(member_error(
            d,
            &format!("BSON field 'priority' value must be <= 1000, actual value '{p}'"),
        ));
    }
    if let Some(v) = num("votes").filter(|v| *v > 1.0) {
        return Err(member_error(
            d,
            &format!("BSON field 'votes' value must be <= 1, actual value '{v}'"),
        ));
    }
    let hidden = matches!(d.get("hidden"), Some(Value::Boolean(true)));
    let votes = num("votes").unwrap_or(1.0);
    // RefDb's default priority is 1, also for a member without votes or hidden.
    let priority = num("priority").unwrap_or(1.0);
    if votes == 0.0 && priority > 0.0 {
        return Err(member_error(d, "priority must be 0 when non-voting (votes:0)"));
    }
    if hidden && priority > 0.0 {
        return Err(member_error(d, "priority must be 0 when hidden=true"));
    }
    let attrs = d.clone().with("priority", priority).with("votes", votes as i32);
    member_of(&attrs).map_err(|e| member_error(d, &e.errmsg))
}

/// `replSetReconfig`: the members' attributes (priority, votes, hidden, tags) over the
/// cluster's nodes, with a `version` above the current one.
pub fn reconfig<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_RECONFIG, ctx.db)?;
    let (rs, _, p, _) = member_view(ctx.backend, REPL_SET_RECONFIG)?;
    let Value::Document(conf) = ctx.first().clone() else {
        return Err(CommandError::new(BAD_VALUE, "replSetReconfig needs a configuration"));
    };
    if let Some(Value::String(name)) = conf.get("_id")
        && name != rs.set_name()
    {
        return Err(CommandError::new(
            INVALID_REPLICA_SET_CONFIG,
            format!(
                "Rejecting reconfig where new config set name differs from command line set name, new config set name: {name}, old config set name: {}",
                rs.set_name()
            ),
        ));
    }
    let version = match conf.get("version") {
        Some(Value::Int32(v)) => *v,
        Some(Value::Int64(v)) => i32::try_from(*v).unwrap_or(i32::MAX),
        _ => return Err(CommandError::new(BAD_VALUE, "replSetReconfig needs a version")),
    };
    if version <= rs.set_version() {
        return Err(CommandError::new(
            NEW_CONFIG_INCOMPATIBLE,
            format!(
                "New replica set configuration version and term must be greater than old, but {{version: {version}, term: {term}}} is not greater than {{version: {old}, term: {term}}} for replica set {name}",
                term = rs.term(),
                old = rs.set_version(),
                name = rs.set_name()
            ),
        ));
    }
    let Some(Value::Array(list)) = conf.get("members") else {
        return Err(CommandError::new(BAD_VALUE, "replSetReconfig needs members"));
    };
    let mut members: Vec<Member> = Vec::new();
    for (i, m) in list.iter().enumerate() {
        let Value::Document(d) = m else {
            return Err(CommandError::new(BAD_VALUE, "a member must be a document"));
        };
        let member = reconfig_member(d)?;
        if let Some(j) = members.iter().position(|o| o.host() == member.host()) {
            return Err(CommandError::new(
                NEW_CONFIG_INCOMPATIBLE,
                format!(
                    "Found two member configurations with same host field, members.{j}.host == members.{i}.host == {}",
                    member.host()
                ),
            ));
        }
        members.push(member);
    }
    let nodes = hosts(ctx.backend)?;
    if let Some(m) = members.iter().find(|m| !nodes.iter().any(|h| h == m.host())) {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            format!("{} is not a node of the cluster: add it with addShard first", m.host()),
        ));
    }
    let next = Presentation { members, set_version: version, ..p };
    next.replica_set(&nodes)
        .map_err(|e| CommandError::new(INVALID_REPLICA_SET_CONFIG, format!("{e:?}")))?;
    store(ctx.backend, &next)?;
    Ok(Document::new().with("ok", 1.0))
}

/// `replSetFreeze`: a secondary accepts it (Group 0 designates the primary, so it changes
/// nothing); the primary refuses.
pub fn freeze<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_FREEZE, ctx.db)?;
    let (rs, me, _, _) = member_view(ctx.backend, REPL_SET_FREEZE)?;
    if is_primary(&rs, me) {
        return Err(CommandError::new(
            NOT_SECONDARY,
            "Cannot freeze node when primary or running for election, state: Primary",
        ));
    }
    let unfreeze = matches!(ctx.first(), Value::Int32(0) | Value::Int64(0))
        || matches!(ctx.first(), Value::Double(d) if *d == 0.0);
    let d = Document::new();
    let d = if unfreeze { d.with("info", "unfreezing") } else { d };
    Ok(d.with("ok", 1.0))
}

/// `replSetMaintenance`: a secondary accepts it; the primary refuses.
pub fn maintenance<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_MAINTENANCE, ctx.db)?;
    let (rs, me, _, _) = member_view(ctx.backend, REPL_SET_MAINTENANCE)?;
    if is_primary(&rs, me) {
        return Err(CommandError::new(NOT_SECONDARY, "primaries can't modify maintenance mode"));
    }
    if matches!(ctx.first(), Value::Boolean(false)) {
        return Err(CommandError::new(OPERATION_FAILED, "already out of maintenance mode"));
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `replSetInitiate`: the replica set is always initiated.
pub fn initiate<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_INITIATE, ctx.db)?;
    member_view(ctx.backend, REPL_SET_INITIATE)?;
    Err(CommandError::new(ALREADY_INITIALIZED, "already initialized"))
}

/// `replSetResizeOplog`: `minRetentionHours` raises the history window (FR-008: the
/// effective retention is the larger of it and `teselaHistoryRetention`).
pub fn resize_oplog<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_RESIZE_OPLOG, ctx.db)?;
    member_view(ctx.backend, REPL_SET_RESIZE_OPLOG)?;
    let hours = match ctx.cmd.get("minRetentionHours") {
        None => None,
        Some(Value::Int32(n)) => Some(f64::from(*n)),
        Some(Value::Int64(n)) => Some(*n as f64),
        Some(Value::Double(d)) => Some(*d),
        Some(other) => {
            return Err(crate::types::wrong_type(
                "replSetResizeOplog.minRetentionHours",
                other,
                "double",
            ));
        }
    };
    if let Some(h) = hours.filter(|h| *h > 0.0) {
        let seconds = (h * 3600.0).ceil() as i64;
        let params = ctx.backend.cluster_parameters()?;
        let stored = params.get(super::parameters::HISTORY_RETENTION).map(Vec::as_slice);
        let current = super::parameters::history_retention_ms(stored) / 1000;
        if u64::try_from(seconds).is_ok_and(|s| s > current) {
            let doc = Document::new().with("seconds", seconds);
            ctx.backend
                .set_cluster_parameter(super::parameters::HISTORY_RETENTION, doc.to_bytes()?)?;
        }
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `replSetSyncFrom`: TeselaDB's members do not copy from one another (Multi-Raft), so the
/// request is taken as asked.
pub fn sync_from<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_SYNC_FROM, ctx.db)?;
    let (rs, me, _, _) = member_view(ctx.backend, REPL_SET_SYNC_FROM)?;
    if is_primary(&rs, me) {
        return Err(CommandError::new(NOT_SECONDARY, "primaries don't sync"));
    }
    let Value::String(host) = ctx.first().clone() else {
        return Err(CommandError::new(BAD_VALUE, "replSetSyncFrom needs a host"));
    };
    if rs.member(&host) == Some(me) {
        return Err(CommandError::new(INVALID_SYNC_SOURCE, "I cannot sync from myself"));
    }
    let mut d = Document::new().with("syncFromRequested", host.as_str());
    if let Some(p) = rs.primary() {
        d.push("prevSyncTarget", p);
    }
    Ok(d.with("ok", 1.0))
}

/// `replSetAbortPrimaryCatchUp`: a designated primary never catches up.
pub fn abort_primary_catch_up<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_ABORT_PRIMARY_CATCH_UP, ctx.db)?;
    member_view(ctx.backend, REPL_SET_ABORT_PRIMARY_CATCH_UP)?;
    Err(CommandError::new(ILLEGAL_OPERATION, "The node is not in catch-up mode."))
}

/// Runs command `name` of this module.
pub fn run<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    name: &str,
) -> Result<Document, CommandError> {
    match name {
        REPL_SET_GET_CONFIG => get_config(ctx),
        REPL_SET_STEP_DOWN => step_down(ctx),
        REPL_SET_RECONFIG => reconfig(ctx),
        REPL_SET_FREEZE => freeze(ctx),
        REPL_SET_MAINTENANCE => maintenance(ctx),
        REPL_SET_INITIATE => initiate(ctx),
        REPL_SET_RESIZE_OPLOG => resize_oplog(ctx),
        REPL_SET_SYNC_FROM => sync_from(ctx),
        _ => abort_primary_catch_up(ctx),
    }
}

/// `heartbeatIntervalMillis` of `replSetGetStatus`.
const HEARTBEAT_INTERVAL_MS: i64 = 2000;

/// `replSetGetStatus` in the replica set presentation: every member healthy and caught up to the
/// node's mark (the ranges replicate by Raft, not by the oplog), with the fields of a RefDb
/// 9.0.4 member (measured on 02-11-2026; the election metrics and the recovery timestamps are
/// left out, DV-088).
pub fn get_status<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let (rs, me, _, _) = member_view(ctx.backend, super::admin::REPL_SET_GET_STATUS)?;
    let (time, increment) = teseladb_hlc::to_timestamp(ctx.backend.cluster_mark());
    let op = Document::new().with("ts", Value::Timestamp { time, increment }).with("t", rs.term());
    let wall = Value::DateTime(i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX));
    let op_date = Value::DateTime(i64::from(time) * 1000);
    let voting = rs.members().iter().filter(|m| m.votes() > 0).count();
    let voting = i32::try_from(voting).unwrap_or(i32::MAX);
    let majority = voting / 2 + 1;
    let primary = rs.primary().and_then(|p| rs.member(p));
    let state = |id: MemberId| if Some(id) == primary { (1, "PRIMARY") } else { (2, "SECONDARY") };
    let mut optimes = Document::new().with("lastCommittedOpTime", op.clone());
    optimes.push("lastCommittedWallTime", wall.clone());
    for f in ["readConcernMajorityOpTime", "appliedOpTime", "durableOpTime", "writtenOpTime"] {
        optimes.push(f, op.clone());
    }
    for f in ["lastAppliedWallTime", "lastDurableWallTime", "lastWrittenWallTime"] {
        optimes.push(f, wall.clone());
    }
    let facts = ctx.backend.process();
    let uptime =
        i32::try_from(facts.now_ms.saturating_sub(facts.start_ms) / 1000).unwrap_or(i32::MAX);
    let mut members = Vec::new();
    for (i, m) in rs.members().iter().enumerate() {
        let id = rs.member(m.host()).unwrap_or(me);
        let (code, name) = state(id);
        let mut d = Document::new()
            .with("_id", i32::try_from(i).unwrap_or(i32::MAX))
            .with("name", m.host())
            .with("health", 1)
            .with("state", code)
            .with("stateStr", name)
            .with("uptime", uptime)
            .with("optime", op.clone())
            .with("optimeDate", op_date.clone())
            .with("lastAppliedWallTime", wall.clone())
            .with("lastDurableWallTime", wall.clone())
            .with("lastWrittenWallTime", wall.clone())
            .with("syncSourceHost", "")
            .with("syncSourceId", -1)
            .with("infoMessage", "")
            .with("configVersion", rs.set_version())
            .with("configTerm", i32::try_from(rs.term()).unwrap_or(i32::MAX));
        if id == me {
            d.push("self", true);
        }
        d.push("lastHeartbeatMessage", "");
        members.push(Value::Document(d));
    }
    let (my_state, _) = state(me);
    Ok(Document::new()
        .with("set", rs.set_name())
        .with("date", wall.clone())
        .with("myState", my_state)
        .with("term", rs.term())
        .with("syncSourceHost", "")
        .with("syncSourceId", -1)
        .with("heartbeatIntervalMillis", HEARTBEAT_INTERVAL_MS)
        .with("majorityVoteCount", majority)
        .with("writeMajorityCount", majority)
        .with("votingMembersCount", voting)
        .with("writableVotingMembersCount", voting)
        .with("optimes", optimes)
        .with("members", members)
        .with("ok", 1.0))
}
