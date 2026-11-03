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
//!
//! A member's holds (`replSetFreeze`, `replSetMaintenance` and the stepped-down primary's
//! `stepDownSecs`) are kept in Group 0 ([`super::presentation::Hold`]) and keep it from being
//! designated; one in maintenance mode is `RECOVERING` to every member and to the drivers, as on
//! RefDb 9.0.4 (measured on 02-11-2026, `f1.js`, `f2.js`).

use teseladb_bson::{Document, Value};
use teseladb_topology::replset::{Member, MemberId, ReplicaSet, whole_number};

use super::presentation::{
    Hold, Presentation, REASON_STEP_UP, TOPOLOGY_PRESENTATION, holds, hosts, member_of, set_hold,
    stored,
};
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
/// `shardingState`: a member of a replica set that is not a shard says it is not enabled.
pub const SHARDING_STATE: &str = "shardingState";

/// The `replSet*` commands of this module (`replSetGetStatus` is in [`super::admin`]).
pub const NAMES: [&str; 10] = [
    SHARDING_STATE,
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
/// `InvalidOptions`: a member asked to sync from itself.
const INVALID_SYNC_SOURCE: Code = Code { code: 72, name: "InvalidOptions" };
/// `NodeNotFound`.
const NODE_NOT_FOUND: Code = Code { code: 74, name: "NodeNotFound" };
/// `FailedToParse`.
const FAILED_TO_PARSE: Code = Code { code: 9, name: "FailedToParse" };
/// `replSetStepDown`'s default `stepDownSecs`.
const STEP_DOWN_SECS: i64 = 60;
/// Milliseconds in a second.
const MS_PER_SECOND: i64 = 1000;

/// The prefix of the config server's commands.
pub const CONFIG_SERVER_PREFIX: &str = "_configsvr";

/// A config server's command on a member, which is not one (measured on 9.0.4): a secondary
/// says it is not primary, the primary that only config servers run it.
pub fn config_server_refusal(
    view: &crate::hello::ReplicaView,
    name: &str,
    node: &crate::hello::NodeInfo,
) -> CommandError {
    let topology = Document::new()
        .with("processId", Value::ObjectId(node.topology.process_id))
        .with("counter", node.topology.counter);
    match gate_writes(&view.set, view.me, topology) {
        Err(e) => e,
        Ok(()) => CommandError::new(
            ILLEGAL_OPERATION,
            format!("{name} can only be run on config servers"),
        ),
    }
}

/// `NotWritablePrimary` on a member that is not the primary.
fn gate_writes(set: &ReplicaSet, me: MemberId, topology: Document) -> Result<(), CommandError> {
    let primary = set.primary().is_some_and(|p| set.member(p) == Some(me));
    if primary {
        return Ok(());
    }
    let mut e = CommandError::new(NOT_WRITABLE_PRIMARY, "not primary");
    e.extra = Document::new().with("topologyVersion", topology);
    Err(e)
}

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
    if PRIMARY_ONLY.contains(&name) {
        return gate_writes(set, me, topology);
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
        // A `refrouter` 9.0.4 does not have them (measured).
        return Err(CommandError::new(COMMAND_NOT_FOUND, format!("no such cmd: {name}")));
    };
    let me = my_host(backend)?;
    let member = rs
        .member(&me)
        .ok_or_else(|| CommandError::new(ILLEGAL_OPERATION, "this node is not a member"))?;
    Ok((rs, member, stored(backend)?, id))
}

/// This node's host, as `config.shards` gives it.
fn my_host<B: Backend + ?Sized>(backend: &mut B) -> Result<String, CommandError> {
    // Group 0 names each node's shard after the node.
    let name = Value::from(super::shard_name(backend.node()).as_str());
    let shards = backend.config_documents(super::config_db::SHARDS)?;
    let host = shards.iter().find(|d| d.get("_id") == Some(&name)).and_then(|d| d.get("host"));
    Ok(match host {
        Some(Value::String(h)) => h.clone(),
        // A node that is not a shard (being removed) has no host: it is no member.
        _ => String::new(),
    })
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
    let now = ctx.backend.now_ms();
    let held = holds(ctx.backend)?;
    let free = |host: &str| !held.iter().any(|(h, x)| h == host && x.holds_back(now));
    let Some(next) = rs.step_down(&free) else {
        return Err(CommandError::new(
            Code { code: 262, name: "ExceededTimeLimit" },
            "No electable secondaries caught up as of the step down deadline",
        ));
    };
    // The old primary may not be designated again for `stepDownSecs`, as RefDb freezes it.
    let secs = seconds(ctx.first()).unwrap_or(STEP_DOWN_SECS).max(0);
    let me_host = rs.members()[member_index(&rs, me)].host().to_owned();
    let hold = held.iter().find(|(h, _)| *h == me_host).map(|(_, x)| *x).unwrap_or_default();
    let until = now.saturating_add(u64::try_from(secs.saturating_mul(MS_PER_SECOND)).unwrap_or(0));
    set_hold(ctx.backend, &me_host, Hold { frozen_until_ms: until, ..hold })?;
    let mark = ctx.backend.cluster_mark();
    let p = Presentation { primary: next.primary().map(str::to_owned), term: next.term(), ..p }
        .elected(mark, REASON_STEP_UP);
    store(ctx.backend, &p)?;
    Ok(Document::new().with("ok", 1.0))
}

/// The position of member `me` in the configuration.
fn member_index(rs: &ReplicaSet, me: MemberId) -> usize {
    rs.members().iter().position(|m| rs.member(m.host()) == Some(me)).unwrap_or(0)
}

/// A number of seconds as RefDb reads it (a double truncated); `None` for anything else.
fn seconds(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(d) if d.is_finite() => Some(*d as i64),
        _ => None,
    }
}

/// This member's host and its stored hold.
fn my_hold<B: Backend + ?Sized>(
    backend: &mut B,
    rs: &ReplicaSet,
    me: MemberId,
) -> Result<(String, Hold), CommandError> {
    let host = rs.members()[member_index(rs, me)].host().to_owned();
    let hold = holds(backend)?.into_iter().find(|(h, _)| *h == host).map(|(_, x)| x);
    Ok((host, hold.unwrap_or_default()))
}

fn member_error(d: &Document, why: &str) -> CommandError {
    CommandError::new(
        INVALID_REPLICA_SET_CONFIG,
        format!("member: {} :: caused by :: {why}", super::display(&Value::Document(d.clone()))),
    )
}

/// A member of `replSetReconfig` checked as RefDb 9.0.4 checks it.
fn reconfig_member(d: &Document) -> Result<Member, CommandError> {
    const NUMBERS: &str = "[long, double, decimal, int]";
    const FLAGS: &str = "[double, bool, int, long, decimal]";
    let wrong = |f: &str, v: &Value, expected: &str| {
        let t = crate::types::type_name(v);
        let why = format!(
            "BSON field 'MemberConfig.{f}' is the wrong type '{t}', expected types '{expected}'"
        );
        member_error(d, &why)
    };
    for (f, expected) in [("priority", NUMBERS), ("votes", NUMBERS), ("hidden", FLAGS)] {
        match d.get(f) {
            None
            | Some(Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)) => {
            }
            Some(Value::Boolean(_)) if f == "hidden" => {}
            Some(v) => return Err(wrong(f, v, expected)),
        }
    }
    if let Some(Value::Document(tags)) = d.get("tags")
        && let Some((k, v)) = tags.iter().find(|(_, v)| !matches!(v, Value::String(_)))
    {
        let t = crate::types::type_name(v);
        let why = format!("tags.{k} field has non-string value of type {t}");
        return Err(CommandError::new(INVALID_REPLICA_SET_CONFIG, why));
    }
    let num = |f: &str| d.get(f).and_then(crate::types::number);
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
    let hidden = match d.get("hidden") {
        Some(Value::Boolean(b)) => *b,
        // A number is a flag: anything but 0 hides the member.
        Some(_) => num("hidden").is_some_and(|n| n != 0.0),
        None => false,
    };
    // RefDb keeps the whole part of `votes` (0.5 is 0, measured).
    let votes = num("votes").unwrap_or(1.0).trunc();
    // RefDb's default priority is 1, also for a member without votes or hidden.
    let priority = num("priority").unwrap_or(1.0);
    if votes == 0.0 && priority > 0.0 {
        return Err(member_error(d, "priority must be 0 when non-voting (votes:0)"));
    }
    if hidden && priority > 0.0 {
        return Err(member_error(d, "priority must be 0 when hidden=true"));
    }
    let attrs =
        d.clone().with("priority", priority).with("votes", votes as i32).with("hidden", hidden);
    // The attributes are checked above: what is left is the host (measured on 03-11-2026).
    member_of(&attrs).map_err(|_| {
        let why = match d.get("host") {
            None => "BSON field 'MemberConfig.host' is missing but a required field".to_owned(),
            Some(v) => format!(
                "BSON field 'MemberConfig.host' is the wrong type '{}', expected type 'string'",
                crate::types::type_name(v)
            ),
        };
        member_error(d, &why)
    })
}

/// The `version` of a `replSetReconfig` configuration as RefDb 9.0.4 reads it: any whole
/// number within 64 bits.
fn config_version(v: Option<&Value>) -> Result<i64, CommandError> {
    let invalid = |why: String| CommandError::new(INVALID_REPLICA_SET_CONFIG, why);
    let Some(v) = v else {
        return Err(invalid(
            "BSON field 'ReplSetConfig.version' is missing but a required field".into(),
        ));
    };
    let Some(x) = crate::types::number(v) else {
        let t = crate::types::type_name(v);
        return Err(invalid(format!("Expected a number, got value of type {t} for field version")));
    };
    let n = match v {
        Value::Int64(n) => *n,
        _ if x.fract() == 0.0 && x >= -(2f64.powi(63)) && x < 2f64.powi(63) => x as i64,
        _ => {
            return Err(invalid(format!(
                "Expected field \"version\" to have a value exactly representable as a 64-bit integer, but found version: {}",
                super::display(v)
            )));
        }
    };
    if n <= 0 {
        return Err(invalid(format!("BSON field 'version' value must be > 0, actual value '{n}'")));
    }
    Ok(n)
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
    let version = config_version(conf.get("version"))?;
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

/// `replSetFreeze`: a secondary may not be designated primary for that many seconds (0, or what
/// is not a number, unfreezes it); the primary refuses. The answers are RefDb 9.0.4's
/// (measured on 02-11-2026, `f1.js`).
pub fn freeze<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_FREEZE, ctx.db)?;
    let (rs, me, _, _) = member_view(ctx.backend, REPL_SET_FREEZE)?;
    if is_primary(&rs, me) {
        return Err(CommandError::new(
            NOT_SECONDARY,
            "Cannot freeze node when primary or running for election, state: Primary",
        ));
    }
    let secs = seconds(ctx.first()).unwrap_or(0);
    let (host, hold) = my_hold(ctx.backend, &rs, me)?;
    let now = ctx.backend.now_ms();
    let until = match u64::try_from(secs.saturating_mul(MS_PER_SECOND)) {
        Ok(ms) if ms > 0 => now.saturating_add(ms),
        _ => 0,
    };
    set_hold(ctx.backend, &host, Hold { frozen_until_ms: until, ..hold })?;
    let d = match secs {
        0 => Document::new().with("info", "unfreezing"),
        1 => Document::new().with("warning", "you really want to freeze for only 1 second?"),
        _ => Document::new(),
    };
    Ok(d.with("ok", 1.0))
}

/// Whether `v` is true as RefDb reads a flag (anything but `false`, 0 and null).
fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Boolean(false) | Value::Null | Value::Undefined)
        && seconds(v).is_none_or(|n| n != 0)
}

/// `replSetMaintenance`: `true` takes a secondary one level deeper into maintenance mode
/// (`RECOVERING`), `false` one level out; the primary refuses (measured on 9.0.4, `f1.js`).
pub fn maintenance<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    super::admin::admin_only(REPL_SET_MAINTENANCE, ctx.db)?;
    let (rs, me, _, _) = member_view(ctx.backend, REPL_SET_MAINTENANCE)?;
    if is_primary(&rs, me) {
        return Err(CommandError::new(NOT_SECONDARY, "primaries can't modify maintenance mode"));
    }
    let (host, hold) = my_hold(ctx.backend, &rs, me)?;
    let maintenance = if truthy(ctx.first()) {
        hold.maintenance.saturating_add(1)
    } else if hold.maintenance > 0 {
        hold.maintenance - 1
    } else {
        return Err(CommandError::new(OPERATION_FAILED, "already out of maintenance mode"));
    };
    set_hold(ctx.backend, &host, Hold { maintenance, ..hold })?;
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
    let host = match ctx.first() {
        Value::String(h) if !h.is_empty() => h.clone(),
        _ => {
            let why = "Cannot parse HostAndPort from empty string";
            return Err(CommandError::new(FAILED_TO_PARSE, why));
        }
    };
    match rs.member(&host) {
        Some(id) if id == me => {
            return Err(CommandError::new(INVALID_SYNC_SOURCE, "I cannot sync from myself"));
        }
        Some(_) => {}
        None => {
            let why = format!("Could not find member \"{host}\" in replica set");
            return Err(CommandError::new(NODE_NOT_FOUND, why));
        }
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
        SHARDING_STATE => {
            member_view(ctx.backend, SHARDING_STATE)?;
            Ok(Document::new().with("enabled", false).with("ok", 1.0))
        }
        _ => abort_primary_catch_up(ctx),
    }
}

/// `heartbeatIntervalMillis` of `replSetGetStatus`.
const HEARTBEAT_INTERVAL_MS: i64 = 2000;
/// `electionTimeoutMillis` of RefDb's default `settings`.
const ELECTION_TIMEOUT_MS: i64 = 10_000;

/// `replSetGetStatus` in the replica set presentation (measured on a replica set of three
/// members of RefDb 9.0.4 on 02-11-2026, `s1.js`, `s2.js`): a member in maintenance mode is
/// `RECOVERING`, the secondaries name the primary as their sync source, and the latest
/// designation by Group 0 is the election (`electionTime`, `electionCandidateMetrics` on the
/// primary, `electionParticipantMetrics` on the others). The ranges replicate by Raft, not by the
/// oplog, so every member's optimes are the node's mark; the members do not send each other
/// heartbeats (each node sends its own to Group 0's leader), so there are no `lastHeartbeat`,
/// `lastHeartbeatRecv` or `pingMs` (DV-088).
pub fn get_status<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let (rs, me, p, _) = member_view(ctx.backend, super::admin::REPL_SET_GET_STATUS)?;
    let mark = ctx.backend.cluster_mark();
    let op = op_time_doc(mark, rs.term());
    let now = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
    let wall = Value::DateTime(now);
    let (time, _) = teseladb_hlc::to_timestamp(mark);
    let op_date = Value::DateTime(i64::from(time) * MS_PER_SECOND);
    let voting = rs.members().iter().filter(|m| m.votes() > 0).count();
    let voting = i32::try_from(voting).unwrap_or(i32::MAX);
    let majority = voting / 2 + 1;
    let primary = rs.primary().and_then(|h| rs.member(h));
    let primary_index = primary.map(|id| member_index(&rs, id));
    let index = |i: usize| i32::try_from(i).unwrap_or(i32::MAX);
    let state = |id: MemberId| match () {
        () if Some(id) == primary => (1, "PRIMARY"),
        () if rs.is_recovering(id) => (3, "RECOVERING"),
        () => (2, "SECONDARY"),
    };
    let source = |id: MemberId| match (Some(id) == primary, primary_index) {
        (false, Some(i)) => (Value::from(rs.members()[i].host()), index(i)),
        _ => (Value::from(""), -1),
    };
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
    let election = p.election.as_ref().filter(|_| primary.is_some());
    let mut members = Vec::new();
    for (i, m) in rs.members().iter().enumerate() {
        let id = rs.member(m.host()).unwrap_or(me);
        let (code, name) = state(id);
        let (sync_host, sync_id) = source(id);
        let mut d = Document::new()
            .with("_id", index(i))
            .with("name", m.host())
            .with("health", 1)
            .with("state", code)
            .with("stateStr", name)
            .with("uptime", uptime)
            .with("optime", op.clone());
        if id != me {
            d.push("optimeDurable", op.clone());
        }
        d.push("optimeWritten", op.clone());
        d.push("optimeDate", op_date.clone());
        if id != me {
            d.push("optimeDurableDate", op_date.clone());
        }
        d.push("optimeWrittenDate", op_date.clone());
        for f in ["lastAppliedWallTime", "lastDurableWallTime", "lastWrittenWallTime"] {
            d.push(f, wall.clone());
        }
        d.push("lastStableRecoveryTimestamp", stable_recovery(mark));
        if id != me {
            d.push("lastHeartbeatMessage", "");
        }
        d.push("syncSourceHost", sync_host);
        d.push("syncSourceId", sync_id);
        d.push("infoMessage", "");
        if let (Some(e), true) = (election, Some(id) == primary) {
            let (t, inc) = teseladb_hlc::to_timestamp(e.at);
            d.push("electionTime", Value::Timestamp { time: t, increment: inc });
            d.push("electionDate", Value::DateTime(i64::from(t) * MS_PER_SECOND));
        }
        d.push("configVersion", whole_number(rs.set_version()));
        d.push("configTerm", i32::try_from(rs.term()).unwrap_or(i32::MAX));
        if id == me {
            d.push("self", true);
            d.push("lastHeartbeatMessage", "");
        }
        members.push(Value::Document(d));
    }
    let (my_state, _) = state(me);
    let (sync_host, sync_id) = source(me);
    let mut out = Document::new()
        .with("set", rs.set_name())
        .with("date", wall.clone())
        .with("myState", my_state)
        .with("term", rs.term())
        .with("syncSourceHost", sync_host)
        .with("syncSourceId", sync_id)
        .with("heartbeatIntervalMillis", HEARTBEAT_INTERVAL_MS)
        .with("majorityVoteCount", majority)
        .with("writeMajorityCount", majority)
        .with("votingMembersCount", voting)
        .with("writableVotingMembersCount", voting)
        .with("optimes", optimes)
        .with("lastStableRecoveryTimestamp", stable_recovery(mark));
    if let (Some(e), Some(pi)) = (election, primary_index) {
        let me_index = member_index(&rs, me);
        let priority = rs.members()[me_index].priority();
        if Some(me_index) == primary_index {
            out.push("electionCandidateMetrics", candidate_metrics(e, &rs, majority, priority));
        } else {
            out.push(
                "electionParticipantMetrics",
                participant_metrics(e, &rs, index(pi), priority),
            );
        }
    }
    Ok(out.with("members", members).with("ok", 1.0))
}

/// The optime of mark `mark` in term `term`.
fn op_time_doc(mark: teseladb_hlc::Hlc, term: i64) -> Document {
    let (time, increment) = teseladb_hlc::to_timestamp(mark);
    Document::new().with("ts", Value::Timestamp { time, increment }).with("t", term)
}

/// `lastStableRecoveryTimestamp`: what a restart recovers to is every applied entry (the ranges
/// persist their Raft logs), so it is the node's mark.
fn stable_recovery(mark: teseladb_hlc::Hlc) -> Value {
    let (time, increment) = teseladb_hlc::to_timestamp(mark);
    Value::Timestamp { time, increment }
}

/// A priority as RefDb shows it: an integer when it is whole.
fn priority_value(p: f64) -> Value {
    // A member's priority is within 0 and 1000, checked when the set is made.
    if p.fract() == 0.0 { Value::Int32(p as i32) } else { Value::Double(p) }
}

/// The date of mark `at`, in milliseconds.
fn date_of(at: teseladb_hlc::Hlc) -> Value {
    Value::DateTime(i64::try_from(at.physical).unwrap_or(i64::MAX))
}

/// `electionCandidateMetrics` of the designated primary: the designation as its election, with
/// the optimes at the designation in the term before it.
fn candidate_metrics(
    e: &super::presentation::Election,
    rs: &ReplicaSet,
    majority: i32,
    priority: f64,
) -> Document {
    let before = op_time_doc(e.at, rs.term().saturating_sub(1));
    Document::new()
        .with("lastElectionReason", e.reason.as_str())
        .with("lastElectionDate", date_of(e.at))
        .with("electionTerm", rs.term())
        .with("lastCommittedOpTimeAtElection", before.clone())
        .with("lastSeenWrittenOpTimeAtElection", before.clone())
        .with("lastSeenOpTimeAtElection", before)
        .with("numVotesNeeded", majority)
        .with("priorityAtElection", priority_value(priority))
        .with("electionTimeoutMillis", ELECTION_TIMEOUT_MS)
        .with("numCatchUpOps", 0i64)
        .with("newTermStartDate", date_of(e.at))
        .with("wMajorityWriteAvailabilityDate", date_of(e.at))
}

/// `electionParticipantMetrics` of the other members: each took the designation as its vote.
fn participant_metrics(
    e: &super::presentation::Election,
    rs: &ReplicaSet,
    candidate: i32,
    priority: f64,
) -> Document {
    let before = op_time_doc(e.at, rs.term().saturating_sub(1));
    Document::new()
        .with("votedForCandidate", true)
        .with("electionTerm", rs.term())
        .with("lastVoteDate", date_of(e.at))
        .with("electionCandidateMemberId", candidate)
        .with("voteReason", "")
        .with("lastWrittenOpTimeAtElection", before.clone())
        .with("maxWrittenOpTimeInSet", before.clone())
        .with("lastAppliedOpTimeAtElection", before.clone())
        .with("maxAppliedOpTimeInSet", before)
        .with("priorityAtElection", priority_value(priority))
        .with("newTermStartDate", date_of(e.at))
        .with("newTermAppliedDate", date_of(e.at))
}

/// The write commands whose replies a member completes ([`member_write_reply`]).
const MEMBER_WRITES: [&str; 3] = ["insert", "update", "delete"];
/// The fields of a write's reply that a member gives after `electionId` and `opTime`, in its
/// order.
const AFTER_OP_TIME: [&str; 3] = ["writeErrors", "upserted", "nModified"];

/// The reply of write `command` as a replica set member gives it (measured on a replica set of
/// `ref:9.0.4`, 03-11-2026): `n`, then `electionId` and `opTime` (the write's optime: its
/// `operationTime` and the presentation's term), then `writeErrors`, `upserted` and `nModified`,
/// then the rest. A reply without `n` (an error) or another command's stays as it is, without the
/// `recoveryToken` a `refrouter` adds inside a transaction, which a member never gives (measured on
/// the same replica set).
pub fn member_write_reply(
    view: &crate::hello::ReplicaView,
    command: &str,
    mut reply: Document,
) -> Document {
    reply.remove("recoveryToken");
    let (Some(n), Some(ts @ Value::Timestamp { .. })) =
        (reply.get("n"), reply.get("operationTime"))
    else {
        return reply;
    };
    if !MEMBER_WRITES.contains(&command) {
        return reply;
    }
    let op_time = Document::new().with("ts", ts.clone()).with("t", view.set.term());
    let mut out = Document::new()
        .with("n", n.clone())
        .with("electionId", Value::ObjectId(view.set.election_id()))
        .with("opTime", op_time);
    for k in AFTER_OP_TIME {
        if let Some(v) = reply.get(k) {
            out.push(k, v.clone());
        }
    }
    for (k, v) in reply.iter().filter(|(k, _)| *k != "n" && !AFTER_OP_TIME.contains(k)) {
        out.push(k, v.clone());
    }
    out
}
