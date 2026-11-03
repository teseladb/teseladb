//! `moveCollection`, `unshardCollection` and `rewriteCollection`, their aborts and
//! `commitReshardCollection` (018 FR-017, FR-018): each one is a resharding ([`super::reshard`])
//! with its own provenance.
//!
//! - `moveCollection` writes a collection without shard key again under a new UUID and sends the
//!   leadership of its ranges to the shard it names. From then on `config.collections` lists it
//!   with key `{_id: 1}` and `unsplittable: true` ([`reshard::UNSPLITTABLE`]).
//! - `unshardCollection` writes a collection with a shard key again in `_id` order without it, on
//!   the shard it names or else on the one that holds the least data.
//! - `rewriteCollection` writes a collection with a shard key again with the same key.
//! - A capped or time series collection keeps its documents and UUID: `moveCollection` only
//!   sends the leadership of its ranges (its documents are keyed by insertion order or by bucket,
//!   which a copy by `_id` does not keep).
//!
//! The replies, messages and codes are those of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026, `t1.js` to `t12.js` in the lane's notes).

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::command::Group0Command;
use teseladb_types::NodeId;

use super::names::{full, parse_full};
use super::reshard::{self, Plan, Provenance, UNSPLITTABLE};
use super::sharding::{SHARD_KEY, ShardKey, shard_key_of};
use super::{Backend, Ctx, admin, kinds};
use crate::error::{
    CommandError, ILLEGAL_OPERATION, INVALID_NAMESPACE, INVALID_OPTIONS, NAMESPACE_NOT_FOUND,
    NAMESPACE_NOT_SHARDED, SHARD_NOT_FOUND,
};

/// `moveCollection`.
pub const MOVE_COLLECTION: &str = "moveCollection";
/// `unshardCollection`.
pub const UNSHARD_COLLECTION: &str = "unshardCollection";
/// `rewriteCollection`.
pub const REWRITE_COLLECTION: &str = "rewriteCollection";
/// `abortMoveCollection`.
pub const ABORT_MOVE_COLLECTION: &str = "abortMoveCollection";
/// `abortUnshardCollection`.
pub const ABORT_UNSHARD_COLLECTION: &str = "abortUnshardCollection";
/// `abortRewriteCollection`.
pub const ABORT_REWRITE_COLLECTION: &str = "abortRewriteCollection";
/// `commitReshardCollection`.
pub const COMMIT_RESHARD_COLLECTION: &str = "commitReshardCollection";
/// The argument that asks for a verification of the copy, which this cluster does not run
/// (`reshardingDocumentVerification` is off on a `refrouter` 9.0.4).
const PERFORM_VERIFICATION: &str = "performVerification";
const TO_SHARD: &str = "toShard";
const CONFIG_DB: &str = "config";
const SYSTEM_PREFIX: &str = "system.";

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// `moveCollection`: the collection without shard key goes to shard `toShard`.
pub fn move_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = MOVE_COLLECTION;
    let text = reshard::command_ns_of(ctx.cmd, name)?;
    let to = required_shard(ctx.cmd, name)?;
    admin::admin_only(name, ctx.db)?;
    let ns =
        parse_full(&text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {text}"))
        })?;
    let node = shard(ctx.backend, &to)?;
    check_verification(ctx.cmd)?;
    check_trackable(&ns)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(|| {
        CommandError::new(NAMESPACE_NOT_FOUND, format!("Namespace {text} not found"))
    })?;
    let kind = kinds::kind_of(&coll)?;
    if matches!(kind, CollectionKind::View { .. }) {
        return Err(not_in_catalog(&ns));
    }
    if shard_key_of(&coll)?.is_some() {
        return Err(CommandError::new(
            NAMESPACE_NOT_FOUND,
            "MoveCollection can only be called on an unsharded collection.",
        ));
    }
    if tracked(&coll) && node_of(ctx.backend, &coll)? == Some(node) {
        return Ok(ok());
    }
    if matches!(kind, CollectionKind::Capped { .. } | CollectionKind::TimeSeries(_)) {
        track(ctx.backend, &ns, &coll, Provenance::Move)?;
        lead_from(ctx.backend, &coll, node)?;
        return Ok(ok());
    }
    reshard::run(ctx, unsplittable_plan(ns, &coll, Provenance::Move, node)?)
}

/// `unshardCollection`: the collection loses its shard key, on shard `toShard` or on the one
/// that holds the least data.
pub fn unshard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = UNSHARD_COLLECTION;
    let text = reshard::command_ns_of(ctx.cmd, name)?;
    let asked = super::args::string(ctx.cmd, name, TO_SHARD)?.map(str::to_owned);
    admin::admin_only(name, ctx.db)?;
    let ns = collection_ns(&text)?;
    let node = match asked {
        Some(to) => shard(ctx.backend, &to)?,
        None => least_loaded(ctx.backend)?,
    };
    check_verification(ctx.cmd)?;
    check_config(&ns)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(|| not_in_catalog(&ns))?;
    if shard_key_of(&coll)?.is_none() {
        if !tracked(&coll) {
            return Err(not_in_catalog(&ns));
        }
        if node_of(ctx.backend, &coll)? == Some(node) {
            return Ok(ok());
        }
        return Err(CommandError::new(
            NAMESPACE_NOT_SHARDED,
            "Collection is already unsharded on a different shard. Use moveCollection.",
        ));
    }
    reshard::run(ctx, unsplittable_plan(ns, &coll, Provenance::Unshard, node)?)
}

/// `rewriteCollection`: the collection is written again with its shard key.
pub fn rewrite_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = REWRITE_COLLECTION;
    let text = reshard::command_ns_of(ctx.cmd, name)?;
    admin::admin_only(name, ctx.db)?;
    let types = ["int", "long", "decimal", "double"];
    let chunks = super::args::int_of(ctx.cmd, name, "numInitialChunks", &types)?;
    let ns = collection_ns(&text)?;
    check_verification(ctx.cmd)?;
    check_config(&ns)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(|| not_in_catalog(&ns))?;
    let key = shard_key_of(&coll)?.ok_or_else(|| {
        CommandError::new(NAMESPACE_NOT_SHARDED, "Collection has to be a sharded collection.")
    })?;
    reshard::check_cardinality(
        ctx.backend,
        &coll,
        &key.key,
        chunks.unwrap_or(reshard::DEFAULT_CHUNKS),
    )?;
    let mut description = Document::new()
        .with("shardKey", key.key.clone())
        .with("oldShardKey", key.key.clone())
        .with("unique", false);
    if let Some(n) = chunks {
        description.push("numInitialChunks", n);
    }
    description.push(reshard::PROVENANCE, name);
    reshard::run(ctx, Plan { ns, key, description, provenance: Provenance::Rewrite, to: None })
}

/// `abortMoveCollection`.
pub fn abort_move_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    reshard::abort(ctx, ABORT_MOVE_COLLECTION, &[Provenance::Move])
}

/// `abortUnshardCollection`.
pub fn abort_unshard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    reshard::abort(ctx, ABORT_UNSHARD_COLLECTION, &[Provenance::Unshard])
}

/// `abortRewriteCollection`.
pub fn abort_rewrite_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    reshard::abort(ctx, ABORT_REWRITE_COLLECTION, &[Provenance::Rewrite])
}

/// `commitReshardCollection`: a resharding here renames its copy as soon as the copy holds the
/// collection's writes, so the command only checks that one runs.
pub fn commit_reshard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = COMMIT_RESHARD_COLLECTION;
    let ns = reshard::command_ns_of(ctx.cmd, name)?;
    admin::admin_only(name, ctx.db)?;
    reshard::running(ctx.shared, &ns)?;
    Ok(ok())
}

/// Whether collection `c` is tracked without shard key (`unsplittable`).
pub fn tracked(c: &CollectionEntry) -> bool {
    RawDoc::from_bytes(&c.options).is_ok_and(|o| o.get(UNSPLITTABLE).is_some())
}

/// The command that left collection `c` tracked without shard key, if any.
pub fn unsplittable_by(c: &CollectionEntry) -> Option<String> {
    let o = RawDoc::from_bytes(&c.options).ok()?;
    match o.get(UNSPLITTABLE)? {
        teseladb_bson::RawValue::String(s) => Some(s.to_owned()),
        _ => None,
    }
}

/// The node that leads the range holding the first key of collection `c`.
pub fn node_of<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<Option<NodeId>, CommandError> {
    let start = KeyBuilder::collection_start(c.tenant, c.id);
    let at = start.as_bytes();
    Ok(backend
        .route_entries()?
        .into_iter()
        .find(|e| e.start.as_slice() <= at && (e.end.is_empty() || at < e.end.as_slice()))
        .and_then(|e| e.leader_hint.or_else(|| e.voters.first().copied())))
}

/// Sends the leadership of every range of collection `c` to `node`, which gets a replica of
/// the ones it has none of, in place of their leader's.
pub fn lead_from<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    node: NodeId,
) -> Result<(), CommandError> {
    let start = KeyBuilder::collection_start(c.tenant, c.id);
    let end = KeyBuilder::collection_end(c.tenant, c.id);
    let (lo, hi) = (start.as_bytes(), end.as_bytes());
    let entries = backend.route_entries()?;
    for e in entries
        .iter()
        .filter(|e| e.start.as_slice() < hi && (e.end.is_empty() || e.end.as_slice() > lo))
    {
        let leader = e.leader_hint.or_else(|| e.voters.first().copied());
        if leader == Some(node) {
            continue;
        }
        if !e.voters.contains(&node) {
            let leaving: Vec<NodeId> = leader.into_iter().collect();
            backend.range_order(Group0Command::SetReplicas {
                group: e.group,
                epoch: e.replica_epoch,
                add: vec![node],
                remove: leaving,
            })?;
        }
        backend.range_order(Group0Command::SetLeaderHint { group: e.group, node, term: 0 })?;
    }
    Ok(())
}

/// The shard among the cluster's that holds the least data, the first listed on a tie: as a
/// `refrouter` 9.0.4 picks the shard of `unshardCollection` without `toShard` (measured on
/// 02-11-2026, whatever the chunks of each shard). The data of a shard are the bytes of the
/// collections in the ranges it leads.
fn least_loaded<B: Backend + ?Sized>(backend: &mut B) -> Result<NodeId, CommandError> {
    let shards: Vec<NodeId> = backend
        .config_documents(super::config_db::SHARDS)?
        .iter()
        .filter(|d| !matches!(d.get("draining"), Some(Value::Boolean(true))))
        .filter_map(|d| match d.get("_id") {
            Some(Value::String(s)) => s.strip_prefix(super::SHARD_PREFIX)?.parse().ok(),
            _ => None,
        })
        .map(NodeId)
        .collect();
    let entries = backend.route_entries()?;
    let mut held: BTreeMap<NodeId, u64> = BTreeMap::new();
    for (_, c) in backend.collections()? {
        let start = KeyBuilder::collection_start(c.tenant, c.id);
        let end = KeyBuilder::collection_end(c.tenant, c.id);
        let (lo, hi) = (start.as_bytes(), end.as_bytes());
        for e in entries
            .iter()
            .filter(|e| e.start.as_slice() < hi && (e.end.is_empty() || e.end.as_slice() > lo))
        {
            let Some(leader) = e.leader_hint.or_else(|| e.voters.first().copied()) else {
                continue;
            };
            let from = lo.max(e.start.as_slice());
            let to = if e.end.is_empty() { hi } else { hi.min(e.end.as_slice()) };
            *held.entry(leader).or_default() += backend.stored_bytes(from, to)?;
        }
    }
    shards
        .iter()
        .copied()
        .min_by_key(|n| held.get(n).copied().unwrap_or(0))
        .ok_or_else(|| CommandError::new(SHARD_NOT_FOUND, "No shards found"))
}

/// The plan of a resharding that leaves collection `c` without shard key on `node`.
fn unsplittable_plan(
    ns: Namespace,
    c: &CollectionEntry,
    provenance: Provenance,
    node: NodeId,
) -> Result<Plan, CommandError> {
    let id = Document::new().with("_id", 1);
    let old = shard_key_of(c)?.map_or_else(|| id.clone(), |k| k.key);
    let description = Document::new()
        .with("shardKey", id.clone())
        .with("oldShardKey", old)
        .with("unique", false)
        .with("numInitialChunks", 1i64)
        .with(reshard::PROVENANCE, provenance.name());
    let key = ShardKey { key: id, unique: false };
    Ok(Plan { ns, key, description, provenance, to: Some(node) })
}

/// Tracks collection `c` without shard key, its documents where they are.
fn track<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    provenance: Provenance,
) -> Result<(), CommandError> {
    let mut options = Document::from(RawDoc::from_bytes(&c.options)?);
    options.remove(SHARD_KEY);
    options.insert(UNSPLITTABLE, provenance.name());
    backend
        .catalog_write(teseladb_meta::catalog::CatalogCommand::Modify {
            ns: ns.clone(),
            options: options.to_bytes()?,
            description: Document::new().with(reshard::CUTOVER, false).to_bytes()?,
        })
        .map(drop)
}

/// `toShard`, required.
fn required_shard(cmd: &Document, name: &str) -> Result<String, CommandError> {
    match super::args::string(cmd, name, TO_SHARD)? {
        Some(s) => Ok(s.to_owned()),
        None => Err(super::args::missing(name, TO_SHARD)),
    }
}

/// The node of shard `name`, or 70.
fn shard<B: Backend + ?Sized>(backend: &mut B, name: &str) -> Result<NodeId, CommandError> {
    super::primaries::shard_node(backend, name)?
        .ok_or_else(|| CommandError::new(SHARD_NOT_FOUND, format!("Shard {name} not found")))
}

/// The namespace of `unshardCollection` and `rewriteCollection`.
fn collection_ns(text: &str) -> Result<Namespace, CommandError> {
    parse_full(text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
        CommandError::new(
            INVALID_NAMESPACE,
            format!("Namespace {text}is not a valid collection name"),
        )
    })
}

fn not_in_catalog(ns: &Namespace) -> CommandError {
    CommandError::new(
        NAMESPACE_NOT_FOUND,
        format!("Collection '{}' not found in cluster catalog", full(ns)),
    )
}

/// A copy is never verified here: `performVerification: true` fails as on a `refrouter` 9.0.4.
pub(super) fn check_verification(cmd: &Document) -> Result<(), CommandError> {
    match cmd.get(PERFORM_VERIFICATION) {
        Some(Value::Boolean(true)) => Err(CommandError::new(
            INVALID_OPTIONS,
            "Cannot set 'performVerification' to true when reshardingDocumentVerification is false",
        )),
        _ => Ok(()),
    }
}

/// The `config` database's collections are not resharded.
fn check_config(ns: &Namespace) -> Result<(), CommandError> {
    match ns.db == CONFIG_DB {
        true => Err(CommandError::new(
            ILLEGAL_OPERATION,
            "Can't reshard a collection in the config database",
        )),
        false => Ok(()),
    }
}

/// Neither the `config` database's collections nor system ones nor those of `admin` are
/// tracked.
fn check_trackable(ns: &Namespace) -> Result<(), CommandError> {
    check_config(ns)?;
    if ns.db == super::admin::ADMIN_DB || ns.coll.starts_with(SYSTEM_PREFIX) {
        return Err(CommandError::new(
            ILLEGAL_OPERATION,
            format!("Tracking of collection '{}' is not supported.", full(ns)),
        ));
    }
    Ok(())
}
