//! `reshardCollection`, `refineCollectionShardKey` and `abortReshardCollection` (018 FR-002):
//! the collection is written again in the order of its new shard key while clients keep
//! writing, as `spec/tla/Reshard.tla` models it.
//!
//! - Clone: a collection `system.resharding.<uuid>` with the indexes of the collection (and the
//!   one of the new key) receives the documents as they were at the clone mark.
//! - Apply: the writes the collection took after that mark, read from its history as a change
//!   stream reads them, are applied to the copy (the double write).
//! - Critical section: the collection's options take [`CUTOVER`], so writes wait
//!   ([`writable`]), and every range of the collection's keys takes a write fence
//!   (`Reshard.tla`'s `blockedW`): a write that read the collection before and reaches a range
//!   after its fence is refused with `StaleConfig`, and its statement goes again. Once the copy
//!   holds every write up to the fence's mark, and up to a second fence's mark that covers the
//!   transactions decided before the first one, a rename with `dropTarget` puts the copy in the
//!   collection's place in one Group 0 entry. The fence stays on the dropped collection's keys.
//! - `abortReshardCollection` (or `killOp`) before the rename drops the copy and lifts the
//!   critical section; the command fails with 341.
//! - `moveCollection`, `unshardCollection` and `rewriteCollection`
//!   ([`super::move_collection`]) run the same steps with their own [`Provenance`]: the first
//!   two leave the copy without shard key ([`UNSPLITTABLE`]) and send its ranges to a shard.
//!
//! The replies, messages and events are those of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026). `currentOp` lists the operation as `ReshardingMetricsRecipientService <uuid>`.

use std::collections::BTreeSet;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_hlc::Hlc;
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexState};
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_types::NodeId;

use super::crud::{Writer, index_rules, rules_of};
use super::mutation::{Bound, DocMutation, Image, Rules};
use super::names::{full, parse_full};
use super::sharding::{SHARD_KEY, ShardKey, document_key, same_value, shard_key_of};
use super::{Backend, Ctx, Shared, display, lock, uuid_text};
use crate::error::{
    BAD_VALUE, CommandError, ILLEGAL_OPERATION, INTERNAL_ERROR, INVALID_NAMESPACE, INVALID_OPTIONS,
    MAX_TIME_MS_EXPIRED, NAMESPACE_NOT_FOUND, NAMESPACE_NOT_SHARDED, NO_SUCH_RESHARD_COLLECTION,
    RESHARD_CARDINALITY, RESHARD_COLLECTION_ABORTED, SAMPLE_SIZE, STALE_CONFIG,
};
use crate::ops::{Op, op_type};
use crate::types::wrong_type;

/// `reshardCollection`.
pub const RESHARD_COLLECTION: &str = "reshardCollection";
/// `refineCollectionShardKey`.
pub const REFINE_COLLECTION_SHARD_KEY: &str = "refineCollectionShardKey";
/// `abortReshardCollection`.
pub const ABORT_RESHARD_COLLECTION: &str = "abortReshardCollection";
/// The prefix of the copy's name in the collection's database.
pub const TEMP_PREFIX: &str = "system.resharding.";
/// The option of a collection in its critical section: the `operationDescription` of the event
/// the cutover gives. Its `collMod`-less changes carry it as their description, which keeps them
/// out of change streams.
pub const CUTOVER: &str = "$resharding";
/// The option of a collection without shard key that `config.collections` lists
/// (`unsplittable: true`, key `{_id: 1}`): the command that made it so.
pub const UNSPLITTABLE: &str = "$unsplittable";
/// `desc` of the operation in `currentOp`.
const METRICS_DESC: &str = "ReshardingMetricsRecipientService ";
/// The chunks a resharding needs without `numInitialChunks` (measured: 90).
pub(super) const DEFAULT_CHUNKS: i64 = 90;
/// The documents sampled per chunk, every tenth one a split point (measured).
const SAMPLES_PER_CHUNK: usize = 10;
const SCAN: usize = super::query::SCAN_CHUNK;
pub(super) const PROVENANCE: &str = "provenance";

/// The phase of a resharding, as `Reshard.tla` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The copy reads the collection at the clone mark.
    Clone,
    /// The copy applies the writes after the clone mark.
    Apply,
    /// Writes wait; the copy takes the last ones before the rename.
    Critical,
    /// The copy took the collection's place.
    Committed,
    /// The copy was dropped.
    Aborted,
}

impl Phase {
    /// `recipientState` in `currentOp`.
    fn state(self) -> &'static str {
        match self {
            Phase::Clone => "cloning",
            Phase::Apply => "applying",
            Phase::Critical => "strict-consistency",
            Phase::Committed => "done",
            Phase::Aborted => "aborting",
        }
    }
}

/// What a resharding does next, as `Reshard.tla` orders its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Writes the documents of the clone mark into the copy.
    Copy,
    /// Applies the writes of the collection's history the copy does not hold yet.
    CatchUp,
    /// Starts the critical section.
    EnterCritical,
    /// Puts the copy in the collection's place.
    Rename,
    /// Nothing left.
    Done,
}

/// The next step of a resharding in `phase`; `caught_up` says whether the copy holds every write
/// of the collection up to the mark taken when the phase began (after the copy, after the start
/// of the critical section, after the rename). The rename waits for the writes before the
/// critical section, and the end for those that found the collection before it.
pub fn next_action(phase: Phase, caught_up: bool) -> Action {
    // The faulty variant renames without the writes after the last read of the history.
    let caught_up = caught_up
        || (cfg!(teseladb_bug = "reshard_cutover_loses_writes")
            && matches!(phase, Phase::Critical | Phase::Committed));
    match (phase, caught_up) {
        (Phase::Clone, _) => Action::Copy,
        (Phase::Apply | Phase::Critical | Phase::Committed, false) => Action::CatchUp,
        (Phase::Apply, true) => Action::EnterCritical,
        (Phase::Critical, true) => Action::Rename,
        (Phase::Committed | Phase::Aborted, true) | (Phase::Aborted, false) => Action::Done,
    }
}

/// The command a resharding runs for: `provenance` in its event and in `currentOp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// `reshardCollection`: a new shard key.
    Reshard,
    /// `refineCollectionShardKey`: the copy keeps the collection's UUID and the event is a
    /// `refineCollectionShardKey`.
    Refine,
    /// `moveCollection`: a collection without shard key goes to another shard.
    Move,
    /// `unshardCollection`: the collection loses its shard key.
    Unshard,
    /// `rewriteCollection`: the collection is written again with its own key.
    Rewrite,
}

impl Provenance {
    /// The command's name.
    pub fn name(self) -> &'static str {
        match self {
            Provenance::Reshard => RESHARD_COLLECTION,
            Provenance::Refine => REFINE_COLLECTION_SHARD_KEY,
            Provenance::Move => super::move_collection::MOVE_COLLECTION,
            Provenance::Unshard => super::move_collection::UNSHARD_COLLECTION,
            Provenance::Rewrite => super::move_collection::REWRITE_COLLECTION,
        }
    }

    /// Whether the copy has no shard key: it is tracked as unsplittable
    /// ([`UNSPLITTABLE`]).
    pub fn unsplittable(self) -> bool {
        matches!(self, Provenance::Move | Provenance::Unshard)
    }
}

/// What the command asked: the new key and the event's description.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The collection.
    pub ns: Namespace,
    /// The new shard key (`{_id: 1}` for a collection that loses its key).
    pub key: ShardKey,
    /// The event's `operationDescription` without `reshardUUID`.
    pub description: Document,
    /// The command it runs for.
    pub provenance: Provenance,
    /// The shard the collection goes to once renamed (`moveCollection`, `unshardCollection`).
    pub to: Option<NodeId>,
}

/// A resharding in progress.
#[derive(Debug)]
pub struct Resharding {
    plan: Plan,
    old: CollectionEntry,
    temp_ns: Namespace,
    temp: CollectionEntry,
    mark: Hlc,
    target: Hlc,
    phase: Phase,
    /// The second fence's mark was taken: no write of the collection can come after it.
    settled: bool,
    op: u32,
    to_copy: i64,
    copied: i64,
    bytes: i64,
    applied: i64,
}

/// `reshardCollection`.
pub fn reshard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    match reshard_plan(ctx)? {
        Some(plan) => run(ctx, plan),
        None => Ok(ok()),
    }
}

/// What `reshardCollection` asks, checked: `None` when the collection already has that key.
pub fn reshard_plan<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Option<Plan>, CommandError> {
    super::admin::admin_only(RESHARD_COLLECTION, ctx.db)?;
    let name = command_ns(ctx.cmd, RESHARD_COLLECTION)?;
    let key = required_key(ctx.cmd, RESHARD_COLLECTION)?;
    if matches!(ctx.cmd.get("unique"), Some(Value::Boolean(true))) {
        return Err(CommandError::new(BAD_VALUE, "The unique field must be false"));
    }
    let types = ["int", "long", "decimal", "double"];
    let chunks = super::args::int_of(ctx.cmd, RESHARD_COLLECTION, "numInitialChunks", &types)?;
    super::sharding::check_collation(RESHARD_COLLECTION, ctx.cmd.get("collation"))?;
    super::move_collection::check_verification(ctx.cmd)?;
    let asked = ShardKey { key, unique: false };
    super::sharding::check_key(&asked)?;
    let ns = parse_ns(&name)?;
    let not_found = || {
        CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!("Collection '{}' not found in cluster catalog", full(&ns)),
        )
    };
    let old = ctx.backend.collection(&ns)?.ok_or_else(not_found)?;
    let have = shard_key_of(&old)?.ok_or_else(not_found)?;
    if have.key == asked.key {
        return Ok(None);
    }
    let need = chunks.unwrap_or(DEFAULT_CHUNKS);
    if need <= 0 {
        return Err(CommandError::new(
            SAMPLE_SIZE,
            "size argument to $sample must be a positive integer",
        ));
    }
    check_cardinality(ctx.backend, &old, &asked.key, need)?;
    let mut description = Document::new()
        .with("shardKey", asked.key.clone())
        .with("oldShardKey", have.key)
        .with("unique", false);
    if let Some(n) = chunks {
        description.push("numInitialChunks", n);
    }
    if let Some(c) = ctx.cmd.get("collation") {
        description.push("collation", c.clone());
    }
    description.push(PROVENANCE, RESHARD_COLLECTION);
    Ok(Some(Plan { ns, key: asked, description, provenance: Provenance::Reshard, to: None }))
}

/// `refineCollectionShardKey`.
pub fn refine_collection_shard_key<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    super::admin::admin_only(REFINE_COLLECTION_SHARD_KEY, ctx.db)?;
    let name = command_ns(ctx.cmd, REFINE_COLLECTION_SHARD_KEY)?;
    let key = required_key(ctx.cmd, REFINE_COLLECTION_SHARD_KEY)?;
    let asked = ShardKey { key, unique: false };
    super::sharding::check_key(&asked)?;
    let ns = parse_ns(&name)?;
    let unsharded = || {
        CommandError::new(
            NAMESPACE_NOT_SHARDED,
            format!("Can't execute {REFINE_COLLECTION_SHARD_KEY} on unsharded collection {name}"),
        )
    };
    let old = ctx.backend.collection(&ns)?.ok_or_else(|| {
        CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!("RefineCollectionShardKey: collection {name} does not exists"),
        )
    })?;
    let have = shard_key_of(&old)?.ok_or_else(unsharded)?;
    if have.key == asked.key {
        return Ok(ok());
    }
    let extends = asked.key.len() > have.key.len()
        && have.key.iter().zip(asked.key.iter()).all(|((a, x), (b, y))| a == b && x == y);
    if !extends {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            format!(
                "{REFINE_COLLECTION_SHARD_KEY} shard key {} does not extend the current shard key {}",
                display(&Value::Document(asked.key.clone())),
                display(&Value::Document(have.key.clone())),
            ),
        ));
    }
    if !has_prefix_index(ctx.backend, &old, &asked.key)? {
        return Err(CommandError::new(
            INVALID_OPTIONS,
            "Please create an index that starts with the proposed shard key before refining the collection's shard key. ",
        ));
    }
    let description =
        Document::new().with("shardKey", asked.key.clone()).with("oldShardKey", have.key);
    let key = ShardKey { key: asked.key, unique: have.unique };
    run(ctx, Plan { ns, key, description, provenance: Provenance::Refine, to: None })
}

/// `abortReshardCollection`: marks the resharding of the namespace, which stops before its
/// rename.
pub fn abort_reshard_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    abort(ctx, ABORT_RESHARD_COLLECTION, &[Provenance::Reshard, Provenance::Refine])
}

/// Kills the reshardings of the namespace `ctx`'s command `name` names that run for one of
/// `provenances`: 339 when none runs, 20 when the ones that run are of another command.
pub(super) fn abort<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    name: &str,
    provenances: &[Provenance],
) -> Result<Document, CommandError> {
    super::admin::admin_only(name, ctx.db)?;
    let ns = command_ns(ctx.cmd, name)?;
    let running = running(ctx.shared, &ns)?;
    let names: Vec<&str> = provenances.iter().map(|p| p.name()).collect();
    let found: Vec<u32> =
        running.iter().filter(|(_, p)| names.contains(&p.as_str())).map(|(id, _)| *id).collect();
    if found.is_empty() {
        return Err(CommandError::new(
            ILLEGAL_OPERATION,
            "Could not find in-progress resharding operation with matching provenance",
        ));
    }
    let mut s = lock(ctx.shared);
    for id in found {
        s.ops.kill(id);
    }
    Ok(ok())
}

/// The reshardings of namespace `ns` in `currentOp`, with their provenance; 339 when none.
pub(super) fn running(shared: &Shared, ns: &str) -> Result<Vec<(u32, String)>, CommandError> {
    let s = lock(shared);
    let found: Vec<(u32, String)> = s
        .ops
        .iter()
        .filter(|(_, o)| {
            o.ns == ns && o.desc.as_deref().is_some_and(|d| d.starts_with(METRICS_DESC))
        })
        .map(|(id, o)| {
            let provenance = match o.metrics.as_ref().and_then(|m| m.get(PROVENANCE)) {
                Some(Value::String(p)) => p.clone(),
                _ => String::new(),
            };
            (id, provenance)
        })
        .collect();
    if found.is_empty() {
        return Err(CommandError::new(
            NO_SUCH_RESHARD_COLLECTION,
            "Could not find resharding-related metadata that matches the given namespace",
        ));
    }
    Ok(found)
}

/// The name of a resharding command, checked as a string.
pub(super) fn command_ns_of(cmd: &Document, name: &str) -> Result<String, CommandError> {
    command_ns(cmd, name)
}

/// The collection of `ns` once no resharding holds its writes (018 FR-002): a write that finds
/// it in its critical section waits for the rename, or fails when its time runs out.
pub fn writable<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Option<CollectionEntry>, CommandError> {
    loop {
        let c = backend.collection(ns)?;
        if !c.as_ref().is_some_and(in_cutover) {
            return Ok(c);
        }
        if !backend.pause() {
            return Err(CommandError::new(MAX_TIME_MS_EXPIRED, crate::ops::TIME_LIMIT_MESSAGE));
        }
    }
}

/// Whether a write failed because a resharding's fence refused it (018 FR-002): nothing of it
/// was written.
pub fn fenced(e: &CommandError) -> bool {
    e.code == STALE_CONFIG
}

/// Waits before a write a fence refused goes again: its critical section ends, or the
/// operation's time does.
pub fn after_fence<B: Backend + ?Sized>(backend: &mut B) -> Result<(), CommandError> {
    match backend.pause() {
        true => Ok(()),
        false => Err(CommandError::new(MAX_TIME_MS_EXPIRED, crate::ops::TIME_LIMIT_MESSAGE)),
    }
}

/// Whether collection `c` is in the critical section of a resharding.
pub fn in_cutover(c: &CollectionEntry) -> bool {
    RawDoc::from_bytes(&c.options).is_ok_and(|o| o.get(CUTOVER).is_some())
}

/// The event of a cutover: a rename of a copy over the collection, whose dropped entry holds
/// the description. `(refine, the collection's UUID, operationDescription)`.
pub fn cutover_event(
    from: &Namespace,
    target: Option<&CollectionEntry>,
) -> Result<Option<(bool, [u8; 16], Document)>, CommandError> {
    let Some(t) = target.filter(|_| from.coll.starts_with(TEMP_PREFIX)) else {
        return Ok(None);
    };
    let options = RawDoc::from_bytes(&t.options)?;
    let Some(teseladb_bson::RawValue::Document(d)) = options.get(CUTOVER) else {
        return Ok(None);
    };
    let d = Document::from(d);
    Ok(Some((d.get("reshardUUID").is_none(), t.uuid, d)))
}

/// Whether a catalog change's description is one of a resharding's own changes, which change
/// streams do not show.
pub fn is_cutover_change(description: &[u8]) -> bool {
    RawDoc::from_bytes(description).is_ok_and(|d| d.get(CUTOVER).is_some())
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

fn command_ns(cmd: &Document, name: &str) -> Result<String, CommandError> {
    match cmd.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Err(wrong_type(&format!("{name}.{name}"), v, "string")),
        None => Err(super::args::missing(name, name)),
    }
}

fn required_key(cmd: &Document, name: &str) -> Result<Document, CommandError> {
    match cmd.get("key") {
        Some(Value::Document(k)) => Ok(k.clone()),
        Some(v) => Err(wrong_type(&format!("{name}.key"), v, "object")),
        None => Err(super::args::missing(name, "key")),
    }
}

fn parse_ns(name: &str) -> Result<Namespace, CommandError> {
    parse_full(name).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
        CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{name}'"))
    })
}

/// The index keys of collection `c` (its ready secondary indexes).
fn index_keys<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<Vec<Document>, CommandError> {
    let mut out = Vec::new();
    for e in backend.indexes(c.id)?.into_iter().filter(|e| e.state == IndexState::Ready) {
        if let Some(Value::Document(k)) = Document::from(RawDoc::from_bytes(&e.spec)?).get("key") {
            out.push(k.clone());
        }
    }
    Ok(out)
}

/// Whether an index of `c` starts with the fields of `key` (the `_id` index serves `{_id: 1}`).
pub(super) fn has_prefix_index<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    key: &Document,
) -> Result<bool, CommandError> {
    let fields: Vec<(&str, &Value)> = key.iter().collect();
    if fields.len() == 1 && fields[0].0 == "_id" && same_value(&Value::Int32(1), fields[0].1) {
        return Ok(true);
    }
    Ok(index_keys(backend, c)?.iter().any(|ik| {
        ik.len() >= fields.len()
            && ik.iter().zip(&fields).all(|((f, v), (sf, sv))| f == *sf && same_value(v, sv))
    }))
}

/// The chunks the documents of `c` give under `key`: sorted by it, every tenth one is a split
/// point and the distinct points cut one chunk more than their number; with more documents than
/// `need` × 10, evenly spaced ones stand for the sample of a `refrouter`.
pub(super) fn check_cardinality<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    key: &Document,
    need: i64,
) -> Result<(), CommandError> {
    let mut points = Vec::new();
    for_each_document(backend, c, None, |_, doc| {
        let mut probe = doc.clone();
        probe.insert("_id", Value::MinKey);
        points.push(document_key(c, key, &probe)?);
        Ok(())
    })?;
    points.sort();
    let want = usize::try_from(need).unwrap_or(usize::MAX).saturating_mul(SAMPLES_PER_CHUNK);
    let sample: Vec<&Vec<u8>> = if points.len() <= want {
        points.iter().collect()
    } else {
        (0..want).map(|i| &points[i * points.len() / want]).collect()
    };
    let distinct: BTreeSet<&Vec<u8>> = sample.iter().step_by(SAMPLES_PER_CHUNK).copied().collect();
    let can = i64::try_from(distinct.len()).unwrap_or(i64::MAX).saturating_add(1);
    if can < need {
        return Err(CommandError::new(
            RESHARD_CARDINALITY,
            format!(
                "The shard key provided does not have enough cardinality to make the required number of chunks of {need}, it can only make {can} chunks"
            ),
        ));
    }
    Ok(())
}

/// Calls `f` with each document of `c` in key order, read at mark `at` (now without one).
pub(super) fn for_each_document<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    at: Option<Hlc>,
    mut f: impl FnMut(&mut B, &Document) -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    let (start, end) = super::query::documents_of(c);
    let view = TxnView::default();
    let mut next = start;
    loop {
        let chunk = match at {
            Some(ts) => backend.txn_scan(&next, &end, SCAN, ts, &view, None)?,
            None => backend.scan(&next, &end, SCAN, ReadConcern::Local)?,
        };
        let short = chunk.len() < SCAN;
        if let Some((k, _)) = chunk.last() {
            next = [k.as_slice(), &[0]].concat();
        }
        for (_, bytes) in &chunk {
            f(backend, &Document::from(RawDoc::from_bytes(bytes)?))?;
        }
        if short {
            return Ok(());
        }
    }
}

/// Runs a resharding to its end.
pub(super) fn run<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    plan: Plan,
) -> Result<Document, CommandError> {
    let mut r = Resharding::start(ctx.backend, ctx.shared, plan, ctx.cmd)?;
    let result = r.drive(ctx.backend, ctx.shared);
    r.finish(ctx.shared);
    result.map(|()| ok())
}

impl Resharding {
    /// Creates the copy with the collection's indexes, records the clone mark and lists the
    /// operation in `currentOp`.
    pub fn start<B: Backend + ?Sized>(
        backend: &mut B,
        shared: &Shared,
        mut plan: Plan,
        cmd: &Document,
    ) -> Result<Resharding, CommandError> {
        let ns = plan.ns.clone();
        let old = backend
            .collection(&ns)?
            .ok_or_else(|| CommandError::new(NAMESPACE_NOT_FOUND, "no collection"))?;
        let refine = plan.provenance == Provenance::Refine;
        let uuid = if refine { old.uuid } else { super::new_uuid(backend) };
        if !refine {
            let desc = std::mem::take(&mut plan.description);
            plan.description = Document::new().with("reshardUUID", uuid_value(uuid));
            for (k, v) in desc.iter() {
                plan.description.push(k, v.clone());
            }
        }
        let temp_ns =
            Namespace { db: ns.db.clone(), coll: format!("{TEMP_PREFIX}{}", uuid_text(&uuid)) };
        let mut options = Document::from(RawDoc::from_bytes(&old.options)?);
        if plan.provenance.unsplittable() {
            options.remove(SHARD_KEY);
            options.insert(UNSPLITTABLE, plan.provenance.name());
        } else {
            options.insert(
                SHARD_KEY,
                Document::new().with("key", plan.key.key.clone()).with("unique", plan.key.unique),
            );
        }
        backend.catalog_write(CatalogCommand::Create {
            ns: temp_ns.clone(),
            uuid,
            options: options.to_bytes()?,
            if_absent: false,
        })?;
        let temp = backend
            .collection(&temp_ns)?
            .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the resharding copy is gone"))?;
        copy_indexes(backend, &old, &temp, &plan)?;
        let to_copy = i64::try_from(count(backend, &old)?).unwrap_or(i64::MAX);
        let mark = backend.read_versions(&[])?.0;
        let op = lock(shared).ops.begin(Op {
            conn: 0,
            client: None,
            metadata: None,
            app_name: None,
            op: op_type(RESHARD_COLLECTION),
            ns: full(&ns),
            command: cmd.clone(),
            started_ms: backend.now_ms(),
            deadline_ms: None,
            killed: false,
            user: None,
            tenant: old.tenant,
            privileges: None,
            roles: None,
            desc: Some(format!("{METRICS_DESC}{}", uuid_text(&uuid))),
            progress: None,
            metrics: None,
        });
        let r = Resharding {
            plan,
            old,
            temp_ns,
            temp,
            mark,
            target: mark,
            phase: Phase::Clone,
            settled: false,
            op,
            to_copy,
            copied: 0,
            bytes: 0,
            applied: 0,
        };
        r.report(shared);
        Ok(r)
    }

    /// The phase it is in.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The copy's namespace.
    pub fn temp_ns(&self) -> &Namespace {
        &self.temp_ns
    }

    /// The phases from the copy to the rename; an abort drops the copy.
    pub fn drive<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<(), CommandError> {
        match self.resume(backend, shared) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.abort(backend)?;
                Err(e)
            }
        }
    }

    /// The phases left from the one it is in.
    fn resume<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<(), CommandError> {
        loop {
            match next_action(self.phase, self.mark >= self.target) {
                Action::Copy => self.copy(backend, shared)?,
                Action::CatchUp => {
                    let before = self.mark;
                    self.catch_up(backend, shared)?;
                    // A history not yet complete past the mark: wait before reading again.
                    if self.mark == before && !backend.pause() {
                        return Err(CommandError::new(
                            MAX_TIME_MS_EXPIRED,
                            crate::ops::TIME_LIMIT_MESSAGE,
                        ));
                    }
                }
                Action::EnterCritical => self.enter_critical(backend, shared)?,
                Action::Rename => self.rename(backend, shared)?,
                Action::Done => return Ok(()),
            }
        }
    }

    /// Writes the documents of the clone mark into the copy.
    pub fn copy<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<(), CommandError> {
        let rules = self.temp_rules(backend)?;
        let mut w = Writer::internal();
        let mark = self.mark;
        let old = self.old.clone();
        let mut done = (0i64, 0i64);
        for_each_document(backend, &old, Some(mark), |b, doc| {
            done.0 += 1;
            done.1 += i64::try_from(doc.to_bytes()?.len()).unwrap_or(i64::MAX);
            self.put(b, &mut w, &rules, doc)
        })?;
        self.copied = done.0;
        self.bytes = done.1;
        self.target = backend.read_versions(&[])?.0;
        self.phase = Phase::Apply;
        self.report(shared);
        self.check_killed(shared)
    }

    /// Applies to the copy the writes the collection took after the mark, up to the newest
    /// mark its history is complete at: each key's document as it is now replaces the one of
    /// its `_id`. The mark it reached.
    pub fn catch_up<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<Hlc, CommandError> {
        // Once renamed, the resharding can no longer stop.
        if self.phase != Phase::Committed {
            self.check_killed(shared)?;
        }
        let (start, end) = super::query::documents_of(&self.old);
        let (rows, resolved) = backend.changes(&start, &end, self.mark)?;
        if resolved <= self.mark {
            return Ok(self.mark);
        }
        let mut first: Vec<(Vec<u8>, Hlc)> = Vec::new();
        for r in rows.iter().filter(|r| r.ts <= resolved) {
            match first.iter_mut().find(|(k, _)| *k == r.key) {
                Some((_, ts)) => *ts = (*ts).min(r.ts),
                None => first.push((r.key.clone(), r.ts)),
            }
        }
        let mut ids = Vec::new();
        let mut now = Vec::new();
        for (key, ts) in first {
            let current = current_doc(backend, &key)?;
            let seen = rows.iter().find_map(|r| match &r.value {
                Some((teseladb_changestream::DOCUMENT_VALUE, b)) if r.key == key => Some(b.clone()),
                _ => None,
            });
            let some = match (&current, seen) {
                (Some(d), _) => Some(d.clone()),
                (None, Some(b)) => Some(Document::from(RawDoc::from_bytes(&b)?)),
                (None, None) => super::change_stream::previous(backend, &key, ts)?
                    .map(|b| RawDoc::from_bytes(&b).map(Document::from))
                    .transpose()?,
            };
            if let Some(id) = some.and_then(|d| d.get("_id").cloned()) {
                ids.push(id);
            }
            now.extend(current);
        }
        let rules = self.temp_rules(backend)?;
        let mut w = Writer::internal();
        for id in ids {
            if let Some(k) = super::crud::id_doc_key(backend, &self.temp, &id)? {
                let filter = Document::new().with("_id", id);
                let gone = DocMutation::Delete {
                    filter,
                    multi: false,
                    image: Image::None,
                    rules: rules.clone(),
                };
                w.write(backend, &Bound { mutation: gone, keys: vec![k] }, 0)?;
            }
        }
        for doc in &now {
            self.put(backend, &mut w, &rules, doc)?;
        }
        self.applied += i64::try_from(now.len()).unwrap_or(i64::MAX);
        self.mark = resolved;
        self.report(shared);
        Ok(resolved)
    }

    /// Starts the critical section: the collection's options take [`CUTOVER`].
    pub fn enter_critical<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<(), CommandError> {
        self.check_killed(shared)?;
        let mut options = Document::from(RawDoc::from_bytes(&self.old.options)?);
        options.insert(CUTOVER, self.plan.description.clone());
        self.modify_old(backend, options, true)?;
        self.phase = Phase::Critical;
        self.target = self.fence(backend, true)?;
        self.report(shared);
        Ok(())
    }

    /// The rename of the copy over the collection, in one Group 0 entry. The copy holds every
    /// write up to the fence's mark, so no intent laid before the fence is pending; their
    /// transactions may have committed above that mark, so a second fence, past their
    /// resolutions, gives the mark the copy reaches first.
    fn rename<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        shared: &Shared,
    ) -> Result<(), CommandError> {
        self.check_killed(shared)?;
        if !self.settled {
            self.settled = true;
            self.target = self.target.max(self.fence(backend, true)?);
            if self.mark < self.target {
                return Ok(());
            }
        }
        backend.catalog_write(CatalogCommand::Rename {
            from: self.temp_ns.clone(),
            to: self.plan.ns.clone(),
            drop_target: true,
            uuid: self.temp.uuid,
        })?;
        self.target = backend.read_versions(&[])?.0;
        self.phase = Phase::Committed;
        let Some(now) = backend.collection(&self.plan.ns)? else { return Ok(()) };
        // The zones' intervals follow the new key order (018 FR-003).
        if !self.plan.provenance.unsplittable() {
            super::zones::rekey(backend, &self.plan.ns, &now)?;
        }
        match self.plan.to {
            Some(node) => super::move_collection::lead_from(backend, &now, node),
            None => Ok(()),
        }
    }

    /// Drops the copy and lifts the critical section.
    pub fn abort<B: Backend + ?Sized>(&mut self, backend: &mut B) -> Result<(), CommandError> {
        if matches!(self.phase, Phase::Committed | Phase::Aborted) {
            return Ok(());
        }
        backend.catalog_write(CatalogCommand::Drop { ns: self.temp_ns.clone() })?;
        if self.phase == Phase::Critical {
            let mut options = Document::from(RawDoc::from_bytes(&self.old.options)?);
            options.remove(CUTOVER);
            self.modify_old(backend, options, false)?;
            self.fence(backend, false)?;
        }
        self.phase = Phase::Aborted;
        Ok(())
    }

    /// Takes the operation out of `currentOp`.
    pub fn finish(&self, shared: &Shared) {
        lock(shared).ops.end(self.op);
    }

    fn modify_old<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        options: Document,
        on: bool,
    ) -> Result<(), CommandError> {
        backend.catalog_write(CatalogCommand::Modify {
            ns: self.plan.ns.clone(),
            options: options.to_bytes()?,
            description: Document::new().with(CUTOVER, on).to_bytes()?,
        })?;
        self.old = backend
            .collection(&self.plan.ns)?
            .ok_or_else(|| CommandError::new(NAMESPACE_NOT_FOUND, "no collection"))?;
        Ok(())
    }

    /// Raises or lifts the write fence of the collection's keys (documents and indexes); the
    /// mark of the fence.
    fn fence<B: Backend + ?Sized>(&self, backend: &mut B, on: bool) -> Result<Hlc, CommandError> {
        // Faulty variant (018 T005): the ranges take the writes of the critical section.
        if cfg!(teseladb_bug = "reshard_without_fence") {
            return Ok(backend.read_versions(&[])?.0);
        }
        let (tenant, id) = (self.old.tenant, self.old.id);
        let start = KeyBuilder::collection_start(tenant, id);
        let end = KeyBuilder::collection_end(tenant, id);
        backend.fence(start.as_bytes(), end.as_bytes(), on)
    }

    fn check_killed(&self, shared: &Shared) -> Result<(), CommandError> {
        if lock(shared).ops.get(self.op).is_some_and(|o| o.killed) {
            return Err(CommandError::new(
                RESHARD_COLLECTION_ABORTED,
                "resharding aborted by user",
            ));
        }
        Ok(())
    }

    fn temp_rules<B: Backend + ?Sized>(&self, backend: &mut B) -> Result<Rules, CommandError> {
        // The copy's writes are no changes of the collection: no stream sees them.
        Ok(Rules {
            unlogged: true,
            ..rules_of(&self.temp_ns, &self.temp, true, None, index_rules(backend, &self.temp)?)?
        })
    }

    fn put<B: Backend + ?Sized>(
        &self,
        backend: &mut B,
        w: &mut Writer,
        rules: &Rules,
        doc: &Document,
    ) -> Result<(), CommandError> {
        let at = match self.plan.provenance.unsplittable() {
            true => super::crud::doc_key(&self.temp, doc.get("_id").unwrap_or(&Value::Null))?,
            false => document_key(&self.temp, &self.plan.key.key, doc)?,
        };
        let m =
            DocMutation::Insert { docs: vec![doc.clone()], ordered: true, rules: rules.clone() };
        let r = w.write(backend, &Bound { mutation: m, keys: vec![at] }, 0)?;
        match r.errors.into_iter().next() {
            Some(e) => Err(e.error),
            None => Ok(()),
        }
    }

    fn report(&self, shared: &Shared) {
        let originating = Document::new()
            .with(RESHARD_COLLECTION, full(&self.plan.ns))
            .with("key", self.plan.key.key.clone())
            .with("unique", self.plan.key.unique.to_string())
            .with("collation", Document::new().with("locale", "simple"));
        let provenance = self.plan.provenance.name();
        let metrics = Document::new()
            .with("originatingCommand", originating)
            .with(PROVENANCE, provenance)
            .with("recipientState", self.phase.state())
            .with("approxDocumentsToCopy", self.to_copy)
            .with("documentsCopied", self.copied)
            .with("bytesCopied", self.bytes)
            .with("oplogEntriesApplied", self.applied);
        if let Some(o) = lock(shared).ops.get_mut(self.op) {
            o.metrics = Some(metrics);
        }
    }
}

/// Gives the copy the indexes of the collection, ready (it is empty), and the one of the new
/// key when no index starts with it, named as `shardCollection` names it.
fn copy_indexes<B: Backend + ?Sized>(
    backend: &mut B,
    old: &CollectionEntry,
    temp: &CollectionEntry,
    plan: &Plan,
) -> Result<(), CommandError> {
    let mut specs: Vec<(String, Vec<u8>, u8)> = backend
        .indexes(old.id)?
        .into_iter()
        .filter(|e| e.state == IndexState::Ready)
        .map(|e| (e.name, e.spec, e.buckets))
        .collect();
    if !has_prefix_index(backend, old, &plan.key.key)? {
        let name: Vec<String> = plan
            .key
            .key
            .iter()
            .map(|(f, v)| match v {
                Value::String(s) => format!("{f}_{s}"),
                _ => format!("{f}_1"),
            })
            .collect();
        let name = name.join("_");
        let spec = Document::new()
            .with("v", 2)
            .with("key", plan.key.key.clone())
            .with("name", name.as_str());
        specs.push((name, spec.to_bytes()?, 0));
    }
    for (name, spec, buckets) in specs {
        backend.index_write(IndexCommand::Create {
            collection: temp.id,
            name: name.clone(),
            spec,
            buckets,
        })?;
        let id = backend
            .indexes(temp.id)?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.id)
            .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the copied index is gone"))?;
        backend.index_write(IndexCommand::SetState { id, state: IndexState::Building })?;
        backend.index_write(IndexCommand::SetState { id, state: IndexState::Ready })?;
    }
    Ok(())
}

/// The documents of `c`.
fn count<B: Backend + ?Sized>(backend: &mut B, c: &CollectionEntry) -> Result<usize, CommandError> {
    let mut n = 0usize;
    for_each_document(backend, c, None, |_, _| {
        n += 1;
        Ok(())
    })?;
    Ok(n)
}

/// The document at `key` now, if any.
fn current_doc<B: Backend + ?Sized>(
    backend: &mut B,
    key: &[u8],
) -> Result<Option<Document>, CommandError> {
    let end = [key, &[0]].concat();
    let rows = backend.scan(key, &end, 1, ReadConcern::Local)?;
    match rows.into_iter().find(|(k, _)| k == key) {
        Some((_, b)) => Ok(Some(Document::from(RawDoc::from_bytes(&b)?))),
        None => Ok(None),
    }
}

fn uuid_value(u: [u8; 16]) -> Value {
    Value::Binary { subtype: super::uuid::UUID_SUBTYPE, bytes: u.to_vec() }
}
