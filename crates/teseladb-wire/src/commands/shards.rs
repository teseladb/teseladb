//! `removeShard` (018 FR-005): a shard is a node, and removing it is draining it (007 FR-015):
//! Group 0 marks it `Draining`, the balancer moves its replicas, leaderships and Group 0 vote to
//! the others and removes it once it holds nothing (D-046), when `config.shards` stops listing it.
//!
//! The replies of the reference router 9.0.4 of two shards (measured on 02-11-2026, `s1.js` and
//! `s2.js` in the lane's notes): a shard that does not exist is `completed` at once; the first call
//! starts the drain (`started`) and lists the databases whose primary the shard is (`dbsToMove`, to
//! move with `movePrimary`) and the collections to move, none in TeselaDB, whose collections live
//! in the ranges; the next ones say `ongoing` with what remains.

use teseladb_bson::{Document, Value};
use teseladb_meta::command::{Group0Command, NodeState};
use teseladb_types::NodeId;

use super::primaries::{of_settings, shard_node};
use super::{Backend, Ctx, admin};
use crate::error::{CommandError, ILLEGAL_OPERATION};
use crate::types::wrong_type;

/// `removeShard`.
pub const REMOVE_SHARD: &str = "removeShard";
/// What the reference router asks the operator to do with the databases still on the shard.
const NOTE: &str = "you need to call moveCollection for collectionsToMove and afterwards movePrimary for the dbsToMove";

/// Whether shard document `d` says it is being drained.
fn draining(d: &Document) -> bool {
    matches!(d.get("draining"), Some(Value::Boolean(true)))
}

/// The databases whose primary is `node`, in name order.
fn dbs_on<B: Backend + ?Sized>(backend: &mut B, node: NodeId) -> Result<Vec<Value>, CommandError> {
    let primaries = of_settings(&backend.group0_settings(super::primaries::PREFIX)?);
    Ok(primaries
        .into_iter()
        .filter(|(_, p)| p.node == node)
        .map(|(db, _)| Value::from(db))
        .collect())
}

/// What `node` still holds: its ranges and their bytes.
fn remaining<B: Backend + ?Sized>(
    backend: &mut B,
    node: NodeId,
    dbs: usize,
) -> Result<Document, CommandError> {
    let held: Vec<_> =
        backend.route_entries()?.into_iter().filter(|e| e.voters.contains(&node)).collect();
    let mut bytes = 0u64;
    for e in &held {
        bytes += backend.stored_bytes(&e.start, &e.end)?;
    }
    let long = |n: usize| Value::Int64(i64::try_from(n).unwrap_or(i64::MAX));
    Ok(Document::new()
        .with("chunks", long(held.len()))
        .with("collectionsToMove", 0i64)
        .with("dbs", long(dbs))
        .with("jumboChunks", 0i64)
        .with("estimatedRemainingBytes", Value::Int64(i64::try_from(bytes).unwrap_or(i64::MAX))))
}

/// `removeShard`.
pub fn remove_shard<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(REMOVE_SHARD, ctx.db)?;
    let name = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{REMOVE_SHARD}.{REMOVE_SHARD}"), v, "string")),
    };
    let shards = ctx.backend.config_documents(super::config_db::SHARDS)?;
    let doc = shards.iter().find(|d| matches!(d.get("_id"), Some(Value::String(s)) if *s == name));
    let (Some(doc), Some(node)) = (doc, shard_node(ctx.backend, &name)?) else {
        return Ok(Document::new()
            .with("shard", name.as_str())
            .with("msg", "removeshard completed successfully")
            .with("state", "completed")
            .with("ok", 1.0));
    };
    let already = draining(doc);
    if !already && !shards.iter().any(|d| d.get("_id") != doc.get("_id") && !draining(d)) {
        let msg = "Operation not allowed because it would drain the last shard";
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    let dbs = dbs_on(ctx.backend, node)?;
    let mut reply = Document::new();
    if !dbs.is_empty() {
        reply.push("note", NOTE);
    }
    reply.push("dbsToMove", Value::Array(dbs.clone()));
    reply.push("collectionsToMove", Value::Array(Vec::new()));
    if already {
        reply.push("msg", "draining ongoing");
        reply.push("state", "ongoing");
        reply.push("remaining", remaining(ctx.backend, node, dbs.len())?);
    } else {
        ctx.backend
            .range_order(Group0Command::SetNodeState { node, state: NodeState::Draining })?;
        reply.push("shard", name.as_str());
        reply.push("msg", "draining started successfully");
        reply.push("state", "started");
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

/// `startShardDraining`.
pub const START_SHARD_DRAINING: &str = "startShardDraining";
/// `stopShardDraining`.
pub const STOP_SHARD_DRAINING: &str = "stopShardDraining";
/// `shardDrainingStatus`.
pub const SHARD_DRAINING_STATUS: &str = "shardDrainingStatus";
/// `commitShardRemoval`.
pub const COMMIT_SHARD_REMOVAL: &str = "commitShardRemoval";

/// A draining command's shard: its name, its `config.shards` document and its node, if it is
/// one of the cluster's.
struct Named {
    name: String,
    doc: Option<Document>,
    node: Option<NodeId>,
    others_active: bool,
}

fn named<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>, cmd: &str) -> Result<Named, CommandError> {
    admin::admin_only(cmd, ctx.db)?;
    let name = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{cmd}.{cmd}"), v, "string")),
    };
    let shards = ctx.backend.config_documents(super::config_db::SHARDS)?;
    let is = |d: &Document| matches!(d.get("_id"), Some(Value::String(s)) if *s == name);
    let doc = shards.iter().find(|d| is(d)).cloned();
    let others_active = shards.iter().any(|d| !is(d) && !draining(d));
    let node = shard_node(ctx.backend, &name)?;
    Ok(Named { name, doc, node, others_active })
}

fn not_found(name: &str) -> CommandError {
    CommandError::new(crate::error::SHARD_NOT_FOUND, format!("Shard {name} not found"))
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// `startShardDraining`: the drain `removeShard` starts, answered with `ok`.
pub fn start_shard_draining<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let s = named(ctx, START_SHARD_DRAINING)?;
    let (Some(doc), Some(node)) = (s.doc, s.node) else { return Err(not_found(&s.name)) };
    if draining(&doc) {
        return Ok(ok());
    }
    if !s.others_active {
        let msg = "Operation not allowed because it would drain the last shard";
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    ctx.backend.range_order(Group0Command::SetNodeState { node, state: NodeState::Draining })?;
    Ok(ok())
}

/// `stopShardDraining`: the node takes replicas again.
pub fn stop_shard_draining<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let s = named(ctx, STOP_SHARD_DRAINING)?;
    let (Some(doc), Some(node)) = (s.doc, s.node) else { return Err(not_found(&s.name)) };
    if draining(&doc) {
        ctx.backend.range_order(Group0Command::SetNodeState { node, state: NodeState::Active })?;
    }
    Ok(ok())
}

/// `shardDrainingStatus`: what the drain still has to move.
pub fn shard_draining_status<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let s = named(ctx, SHARD_DRAINING_STATUS)?;
    let (Some(doc), Some(node)) = (s.doc, s.node) else { return Err(not_found(&s.name)) };
    if !draining(&doc) {
        let msg = format!("The shard {} isn't draining", s.name);
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    let dbs = dbs_on(ctx.backend, node)?;
    let mut reply = Document::new()
        .with("state", "ongoing")
        .with("remaining", remaining(ctx.backend, node, dbs.len())?)
        .with("msg", "draining ongoing")
        .with("dbsToMove", Value::Array(dbs.clone()));
    if !dbs.is_empty() {
        reply.push("note", NOTE);
    }
    reply.push("collectionsToMove", Value::Array(Vec::new()));
    reply.push("ok", 1.0);
    Ok(reply)
}

/// `commitShardRemoval`: a drained node goes at the balancer's next round (D-046); a shard that
/// does not exist is already gone.
pub fn commit_shard_removal<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let s = named(ctx, COMMIT_SHARD_REMOVAL)?;
    let (Some(doc), Some(node)) = (s.doc, s.node) else { return Ok(ok()) };
    if !draining(&doc) {
        let msg = format!("Shard {} is not currently draining", s.name);
        return Err(CommandError::new(crate::error::CONFLICTING_OPERATION_IN_PROGRESS, msg));
    }
    let holds = ctx.backend.route_entries()?.iter().any(|e| e.voters.contains(&node));
    if holds || !dbs_on(ctx.backend, node)?.is_empty() {
        let msg = format!(
            "The shard {} isn't completely drained. Please use the shardDrainingStatus command to \
             check the draining status.",
            s.name
        );
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    Ok(ok())
}

/// `addShard`.
pub const ADD_SHARD: &str = "addShard";

/// `addShard`: a node of the cluster, named by its client address (`host:port`, or
/// `<shard>/host:port` as a replica set's seed list), is a shard already, and the command answers
/// with its name as the reference router answers for a shard it has. Any other node joins the
/// cluster from its side with an enrollment token (013 FR-005), which is how TeselaDB adds a shard
/// (DV-086).
pub fn add_shard<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin::admin_only(ADD_SHARD, ctx.db)?;
    let given = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{ADD_SHARD}.{ADD_SHARD}"), v, "string")),
    };
    let (set, hosts) = match given.split_once('/') {
        Some((set, hosts)) => (Some(set), hosts),
        None => (None, given.as_str()),
    };
    let shards = ctx.backend.config_documents(super::config_db::SHARDS)?;
    let of = |d: &Document, f: &str| match d.get(f) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let found = shards.iter().find(|d| {
        hosts.split(',').any(|h| of(d, "host") == h) && set.is_none_or(|s| of(d, "_id") == s)
    });
    match found {
        Some(d) => Ok(Document::new().with("shardAdded", of(d, "_id")).with("ok", 1.0)),
        None => {
            let msg = format!(
                "{given} is not a node of this cluster: a node joins the cluster with an enrollment \
                 token (`teseladb admin token create`, then start it with `--join <addr> --enroll-token <text>`)"
            );
            Err(CommandError::new(crate::error::OPERATION_FAILED, msg))
        }
    }
}
