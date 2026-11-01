//! `insert`, `update`, `delete`, `findAndModify` and `bulkWrite` (FR-006, FR-007, FR-013,
//! FR-018). Each write becomes range mutations ([`super::mutation`]) over the keys of the
//! documents it may touch: an insert's own keys, the key of an `_id` equality, or the documents
//! a scan finds matching, which the leader checks again. A batch reports `n`, `nModified`,
//! `upserted`, `writeErrors` and `writeConcernError` as a `refrouter` 7.0 does (measured).
//!
//! A collection that does not exist is created by the first write, as in RefDb.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::IndexState;
use teseladb_mql::update::{Now, Update, UpdateContext, upsert};
use teseladb_mql::{Filter, FilterUse, MatchContext};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::read::ReadConcern;
use teseladb_session::retry::{StatementKey, TxnCheck, check_txn_number};

use super::args::{self, missing};
use super::index_maintenance::{IndexRule, probe, probe_requests};
use super::mutation::{
    Bound, DocMutation, Image, MutationResult, Rules, Stored, executor_error, executor_of,
    from_mql, stored_bytes,
};
use super::names::{self, full};
use super::query::{documents_of, for_each_match_with, options_collation, target};
use super::sort::SortSpec;
use super::{Backend, Ctx, WriteOp, lock, new_object_id, uuid};
use crate::MAX_WRITE_BATCH_SIZE;
use crate::concerns::{self, WriteConcernArgs, write_concern_error};
use crate::error::{
    BAD_VALUE, CONSTANTS_ONLY_FOR_PIPELINE, CommandError, DOLLAR_PREFIXED_FIELD_NAME,
    FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, INVALID_ID_FIELD, INVALID_LENGTH, TRANSACTION_TOO_OLD,
    UNSATISFIABLE_WRITE_CONCERN,
};
use crate::msg::command_limits;
use crate::types::{BULK_SAFE_BOOL_TYPES, WRITE_SAFE_BOOL_TYPES, wrong_type};

/// `insert`.
pub const INSERT: &str = "insert";
/// `update`.
pub const UPDATE: &str = "update";
/// `delete`.
pub const DELETE: &str = "delete";
/// `findAndModify`.
pub const FIND_AND_MODIFY: &str = "findAndModify";
/// `findandmodify`, the spelling older drivers send.
pub const FIND_AND_MODIFY_LEGACY: &str = "findandmodify";
/// `bulkWrite`.
pub const BULK_WRITE: &str = "bulkWrite";

/// Documents one insert mutation carries at most.
pub const INSERT_CHUNK_DOCS: usize = 1000;
/// Bytes of documents one insert mutation carries at most.
pub const INSERT_CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Keys one update or delete mutation checks at most.
pub const MODIFY_CHUNK_KEYS: usize = 1000;

/// The command's write concern as RefDb describes it in a `writeConcernError`:
/// `{ writeConcern: { w: 5, j: true, wtimeout: 0, provenance: "clientSupplied" } }`.
/// The field of a write's reply that lists the statements answered from an earlier send.
const RETRIED_STMT_IDS: &str = "retriedStmtIds";

/// What every write of a command shares.
pub struct Writer {
    /// Write concern.
    pub wc: WriteConcern,
    /// Session and transaction number of a retryable write.
    pub retry: Option<(u128, i64)>,
    /// `writeConcernError` to report.
    pub wc_error: Option<Document>,
    /// The write concern as the command gave it, for its errors.
    args: WriteConcernArgs,
    /// Statements answered from the records of an earlier send (`retriedStmtIds`).
    pub retried: Vec<i32>,
    /// `redactClientLogData` when the command began (013 FR-027).
    redact: bool,
    /// What follows the message of a `writeConcernError`: the node it came from, as a `refrouter`
    /// names the shard (D-102).
    relayed: String,
}

impl Writer {
    /// A writer of the node's own writes: `w: "majority"`, not retryable.
    pub fn internal() -> Writer {
        Writer {
            wc: WriteConcern { w: W::Majority, j: true },
            retry: None,
            wc_error: None,
            args: concerns::default_write_concern(),
            retried: Vec::new(),
            redact: false,
            relayed: String::new(),
        }
    }

    /// Reads the write concern and the retryable-write fields of `cmd`.
    pub fn new<B: Backend + ?Sized>(ctx: &Ctx<'_, B>, name: &str) -> Result<Writer, CommandError> {
        // An unknown mode or tag set does not stop the write: the reply carries the error, as a
        // shard's (measured in `refrouter` 9.0.4).
        let wc_args = args::write_concern(ctx.cmd, name)?;
        let wc = wc_args.to_range();
        // `findAndModify` adds its own empty details (measured).
        let at = format!(" at {}", super::shard_name(ctx.backend.node()));
        let relayed = if name == FIND_AND_MODIFY || name == FIND_AND_MODIFY_LEGACY {
            format!("; Error details: <none>{at}")
        } else {
            at
        };
        let wc_error =
            wc_args.error_after_write().map(|e| write_concern_error(&e, &wc_args, &relayed));
        let lsid = args::lsid(ctx.cmd)?;
        let txn = args::txn_number(ctx.cmd)?;
        let now = ctx.backend.now_ms();
        let retry = match (lsid, txn) {
            (Some(l), Some(t)) => {
                let mut g = lock(ctx.shared);
                let session = g.sessions.touch(l, now);
                if let TxnCheck::TooOld { highest } = check_txn_number(session, t) {
                    return Err(CommandError::new(
                        TRANSACTION_TOO_OLD,
                        format!(
                            "Retryable write with txnNumber {t} is prohibited on session because a newer retryable write with txnNumber {highest} has already started on this session."
                        ),
                    ));
                }
                Some((l, t))
            }
            (Some(l), None) => {
                lock(ctx.shared).sessions.touch(l, now);
                None
            }
            (None, _) => None,
        };
        let redact = lock(ctx.shared).params.redact_client_log_data;
        Ok(Writer { wc, retry, wc_error, args: wc_args, retried: Vec::new(), redact, relayed })
    }

    /// Runs one mutation: its statement is `stmt` within the command.
    pub fn write<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        bound: &Bound,
        stmt: u32,
    ) -> Result<MutationResult, CommandError> {
        let dedup =
            self.retry.map(|(lsid, txn_number)| StatementKey { lsid, txn_number, stmt_id: stmt });
        // `w: 0` waits for no one: the write is applied by the range's leader before the next
        // request of the connection runs, and the reply says nothing of it (`finish`).
        let wc = match self.wc.w {
            W::Zero => WriteConcern { w: W::Nodes(1), j: false },
            _ => self.wc,
        };
        // Each send of a retryable write has its own number: a result that comes back with
        // another one was kept from an earlier send, and the statement counts as retried.
        let attempt = match self.retry {
            Some(_) => i64::try_from(backend.random() >> 1).unwrap_or(i64::MAX).max(1),
            None => 0,
        };
        // A write that adds a value to an index with `prepareUnique` asks for probes of it and
        // runs again with them (009 FR-004).
        let mut probed: Option<Bound> = None;
        let mut rounds = 0;
        let (op, written) = loop {
            let b = probed.as_ref().unwrap_or(bound);
            let mutation = b.encode_with_attempt(attempt)?;
            let op = WriteOp { keys: b.latch_keys(), mutation, dedup, wc };
            // A write whose index entries are in another range goes as a transaction across
            // ranges (009 FR-003); a retryable one that went that way left its result there.
            let earlier = match dedup {
                Some(k) if rounds == 0 && b.keeps_indexes() => {
                    super::cross_range::earlier_result(backend, &k)?
                }
                _ => None,
            };
            let written = match earlier {
                Some(result) => Ok(result),
                None => match backend.write(op.clone()) {
                    Err(e)
                        if super::cross_range::crosses(&e) || super::cross_range::too_large(&e) =>
                    {
                        super::cross_range::write(backend, b, dedup)
                    }
                    w => w,
                },
            };
            let Some(requests) = written.as_ref().err().and_then(probe_requests) else {
                break (op, written);
            };
            rounds += 1;
            if rounds > MAX_PROBE_ROUNDS {
                let msg =
                    "the values of an index with prepareUnique kept changing during the write";
                return Err(CommandError::new(crate::error::WRITE_CONFLICT, msg));
            }
            let mut probes = b.probes().to_vec();
            for request in &requests {
                let p = probe(backend, request)?;
                probes.retain(|q| q.guard != p.guard);
                probes.push(p);
            }
            probed = Some(bound.with_probes(probes));
        };
        let r = match written {
            Err(e) if e.code == UNSATISFIABLE_WRITE_CONCERN => {
                // As a shard does: the write goes ahead and the reply carries the error.
                let e = CommandError::new(e.code, "Not enough data-bearing nodes");
                self.wc_error = Some(write_concern_error(&e, &self.args, &self.relayed));
                let op = WriteOp { wc: WriteConcern { w: W::Majority, j: true }, ..op };
                MutationResult::decode(&backend.write(op)?)?
            }
            Err(e) => return Err(e),
            Ok(bytes) => MutationResult::decode(&bytes)?,
        };
        let r = MutationResult { retried: attempt != 0 && r.attempt != attempt, ..r };
        log_validation(backend, bound, &r, self.redact);
        Ok(r)
    }

    /// Records that statements `stmts` were answered from an earlier send.
    pub fn note_retried(&mut self, stmts: impl IntoIterator<Item = u32>) {
        for s in stmts {
            let s = i32::try_from(s).unwrap_or(i32::MAX);
            if !self.retried.contains(&s) {
                self.retried.push(s);
            }
        }
    }

    /// The `retriedStmtIds` of the reply.
    fn retried_value(&self) -> Value {
        Value::Array(self.retried.iter().map(|&s| Value::Int32(s)).collect())
    }

    /// Adds `retriedStmtIds` when statements were retried (measured on a `refrouter` 9.0.4) and
    /// `writeConcernError` to a reply; with `w: 0` the reply is only `{ok: 1}`.
    pub fn finish(&self, mut reply: Document) -> Document {
        if self.wc.w == W::Zero {
            return Document::new().with("ok", 1.0);
        }
        if !self.retried.is_empty() && reply.get(RETRIED_STMT_IDS).is_none() {
            reply.push(RETRIED_STMT_IDS, self.retried_value());
        }
        if let Some(e) = &self.wc_error {
            reply.insert("writeConcernError", e.clone());
        }
        reply.insert("ok", 1.0);
        reply
    }
}

/// How many times a write probes the values of an index with `prepareUnique` before it gives up
/// with a `WriteConflict`: each round means another write added one of its values meanwhile.
const MAX_PROBE_ROUNDS: u32 = 64;

/// The collection of `ns`, created if it does not exist.
pub fn ensure_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
) -> Result<CollectionEntry, CommandError> {
    if let Some(c) = ctx.backend.collection(ns)? {
        super::kinds::not_a_view(ns, &c)?;
        return Ok(c);
    }
    names::check_writable(ns)?;
    let uuid = super::new_uuid(ctx.backend);
    let options = Document::new().to_bytes()?;
    let cmd = CatalogCommand::Create { ns: ns.clone(), uuid, options, if_absent: true };
    super::catalog::write_adding_database(ctx.backend, &ns.db, cmd)?.ok_or_else(|| {
        CommandError::new(crate::error::INTERNAL_ERROR, "the collection was not created")
    })
}

/// Whether `e` fails a whole write command instead of one of its statements: a `refrouter`
/// creates the database before it runs any statement (13297, measured on 9.0.4).
fn fails_command(e: &CommandError) -> bool {
    e.code == crate::error::DATABASE_DIFFER_CASE
}

/// The error of `findAndModify` and `bulkWrite` when the database cannot be created: a
/// `refrouter` 9.0.4 names the step that failed (measured).
fn routing_error(e: CommandError) -> CommandError {
    if !fails_command(&e) {
        return e;
    }
    let msg = format!(
        "Failed to create RoutingContext in WriteBatchScheduler :: caused by :: {}",
        e.errmsg
    );
    CommandError { errmsg: msg, ..e }
}

/// The secondary indexes of `coll` its writes keep: those being built and those ready (009
/// FR-003, FR-006).
pub fn index_rules<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<Vec<IndexRule>, CommandError> {
    let entries = backend.indexes(coll.id)?;
    let mut rules = entries
        .into_iter()
        .filter(|e| e.state != IndexState::Dropping)
        .map(|e| {
            let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
            let delete_only = e.state == IndexState::DeleteOnly;
            Ok(IndexRule { id: e.id, spec, buckets: e.buckets, delete_only })
        })
        .collect::<Result<Vec<_>, CommandError>>()?;
    // A capped collection keeps `_id` unique in an index of its own (015 FR-003).
    if super::capped::rule_of(coll)?.is_some() {
        rules.push(super::capped::id_index_rule(coll));
    }
    Ok(rules)
}

/// The rules of `coll` for a write of command `cmd`.
pub fn rules_of(
    ns: &Namespace,
    coll: &CollectionEntry,
    bypass: bool,
    collation: Option<Document>,
    indexes: Vec<IndexRule>,
) -> Result<Rules, CommandError> {
    let opts = Document::from(RawDoc::from_bytes(&coll.options)?);
    let text = |k: &str, default: &str| match opts.get(k) {
        Some(Value::String(s)) => s.clone(),
        _ => default.to_owned(),
    };
    let validator = match opts.get("validator") {
        Some(Value::Document(v)) if !v.is_empty() => {
            Some((v.clone(), text("validationLevel", "strict"), text("validationAction", "error")))
        }
        _ => None,
    };
    Ok(Rules {
        ns: full(ns),
        validator,
        bypass,
        collation: collation.or_else(|| options_collation(coll)),
        collection: coll.id,
        indexes,
        vars: None,
        attempt: 0,
        probes: Vec::new(),
        capped: super::capped::rule_of(coll)?,
        timeseries: None,
    })
}

/// The key of a document with `_id` `id` in collection `coll`: in `_id` order without bucket in a
/// clustered collection (015 FR-009), after the hash bucket of its `_id` in the others.
pub fn doc_key(coll: &CollectionEntry, id: &Value) -> Result<Vec<u8>, CommandError> {
    let holder = Document::new().with("_id", id.clone());
    let bytes = holder.to_bytes()?;
    let raw = RawDoc::from_bytes(&bytes)?;
    let id = raw.get("_id").ok_or(CommandError::new(BAD_VALUE, "bad _id"))?;
    let key = match super::kinds::kind_of(coll)? {
        CollectionKind::Clustered { .. } => KeyBuilder::document_clustered(0, coll.id, id),
        // Its documents are keyed by insertion order: the `_id_` index says where one is.
        CollectionKind::Capped { .. } => {
            return Err(CommandError::new(
                crate::error::INTERNAL_ERROR,
                "a capped document has no _id key",
            ));
        }
        CollectionKind::Normal | CollectionKind::View { .. } | CollectionKind::TimeSeries(_) => {
            KeyBuilder::document(0, coll.id, id)
        }
    };
    Ok(key.as_bytes().to_vec())
}

/// The keys an insert of a document with `_id` `id` latches in `coll`: its own, or none in a
/// capped collection, which takes the next `seq` on the range.
fn insert_key(coll: &CollectionEntry, id: &Value) -> Result<Vec<u8>, CommandError> {
    match (super::capped::rule_of(coll)?, super::timeseries::spec_of(coll)?) {
        (None, None) => doc_key(coll, id),
        _ => Ok(Vec::new()),
    }
}

/// The key of the document with `_id` `id` in `coll`, `None` when a capped collection has none.
pub(crate) fn id_doc_key<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    id: &Value,
) -> Result<Option<Vec<u8>>, CommandError> {
    match super::capped::rule_of(coll)? {
        Some(_) => super::capped::id_key(backend, coll, id),
        None => doc_key(coll, id).map(Some),
    }
}

/// Runs write `bound` of `coll`: a capped collection's goes with its state and oldest documents.
fn run<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    w: &mut Writer,
    coll: &CollectionEntry,
    bound: &Bound,
    stmt: u32,
) -> Result<MutationResult, CommandError> {
    if bound.rules().capped.is_some() {
        return super::capped::write(backend, w, coll, bound, stmt);
    }
    match (&bound.mutation, super::timeseries::spec_of(coll)?) {
        (DocMutation::Insert { .. }, Some(spec)) => {
            super::timeseries::write(backend, shared, w, (coll, &spec), bound, stmt)
        }
        _ => w.write(backend, bound, stmt),
    }
}

/// Checks an `_id` as RefDb does for stored documents.
fn check_id(id: &Value) -> Result<(), CommandError> {
    let bad_type = |t: &str| {
        CommandError::new(INVALID_ID_FIELD, format!("The '_id' value cannot be of type {t}"))
    };
    match id {
        Value::Array(_) => Err(bad_type("array")),
        Value::Regex { .. } => Err(bad_type("regex")),
        Value::Undefined => Err(bad_type("undefined")),
        Value::Document(d) => dollar_free_id(d),
        _ => Ok(()),
    }
}

/// An `_id` document has no `$` name at any depth (measured on a `refrouter` 9.0.4).
fn dollar_free_id(d: &Document) -> Result<(), CommandError> {
    for (k, v) in d.iter() {
        if k.starts_with('$') {
            return Err(CommandError::new(
                DOLLAR_PREFIXED_FIELD_NAME,
                format!(
                    "_id fields may not contain '$'-prefixed fields: {k} is not valid for storage."
                ),
            ));
        }
        if let Value::Document(inner) = v {
            dollar_free_id(inner)?;
        }
    }
    Ok(())
}

/// The document as stored: `_id` first, generated if missing.
pub(crate) fn with_id<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    doc: &Document,
) -> Result<Document, CommandError> {
    let id = match doc.get("_id") {
        Some(v) => v.clone(),
        None => Value::ObjectId(new_object_id(backend, shared)),
    };
    check_id(&id)?;
    let mut out = Document::new().with("_id", id);
    for (k, v) in doc.iter().filter(|(k, _)| *k != "_id") {
        out.push(k, v.clone());
    }
    Ok(out)
}

pub(crate) fn write_error(index: usize, e: &CommandError) -> Value {
    let mut d = Document::new()
        .with("index", index as i32)
        .with("code", e.code.code)
        .with("errmsg", e.errmsg.as_str());
    for (k, v) in e.extra.iter() {
        d.push(k, v.clone());
    }
    Value::Document(d)
}

/// The statements of a write batch, between one and the most a batch takes.
pub(crate) fn batch<'a>(
    cmd: &'a Document,
    name: &str,
    field: &str,
) -> Result<&'a [Value], CommandError> {
    let items = args::array(cmd, name, field)?.ok_or_else(|| missing(name, field))?;
    if items.is_empty() || items.len() > MAX_WRITE_BATCH_SIZE as usize {
        return Err(CommandError::new(
            INVALID_LENGTH,
            format!(
                "Write batch sizes must be between 1 and {MAX_WRITE_BATCH_SIZE}. Got {} operations.",
                items.len()
            ),
        ));
    }
    Ok(items)
}

/// The statement id of statement `i` (`stmtIds` if the driver gave them).
fn stmt_id(cmd: &Document, i: usize) -> u32 {
    match cmd.get("stmtIds") {
        Some(Value::Array(ids)) => match ids.get(i) {
            Some(Value::Int32(n)) => *n as u32,
            _ => i as u32,
        },
        _ => match cmd.get("stmtId") {
            Some(Value::Int32(n)) => (*n as u32).wrapping_add(i as u32),
            _ => i as u32,
        },
    }
}

/// A count as RefDb answers it: an `int`.
pub fn count(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// The results of `statements` on a collection kept in Group 0 (the users and roles, and
/// `config.settings`), which runs the whole command as one batch; `None` for any other
/// collection.
fn direct_modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    statements: impl FnOnce() -> Vec<Result<Modify, CommandError>>,
    ordered: bool,
    bypass: bool,
) -> Result<Option<Vec<Result<MutationResult, CommandError>>>, CommandError> {
    if let Some(kind) = super::authz_writes::kind_of(ns) {
        return super::authz_writes::modify(ctx, kind, &statements(), ordered).map(Some);
    }
    if super::config_settings::is_settings(ns) {
        return super::config_settings::modify(ctx, &statements(), ordered, bypass).map(Some);
    }
    Ok(None)
}

/// `insert`.
pub fn insert<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    // A required field that is missing fails before the collection name (measured).
    let docs = batch(ctx.cmd, INSERT, "documents")?.to_vec();
    let ns = target(ctx)?;
    let ordered = args::bool_field(ctx.cmd, INSERT, "ordered")?.unwrap_or(true);
    let bypass =
        args::safe_bool(ctx.cmd, INSERT, "bypassDocumentValidation", &WRITE_SAFE_BOOL_TYPES)?
            .unwrap_or(false);
    let expected_uuid = uuid::expected(ctx.cmd, INSERT)?;
    let mut w = Writer::new(ctx, INSERT)?;
    if let Some(kind) = super::authz_writes::kind_of(&ns) {
        let reply = super::authz_writes::insert(ctx, kind, &docs, ordered)?;
        return Ok(w.finish(reply));
    }
    if super::config_settings::is_settings(&ns) {
        let reply = super::config_settings::insert(ctx, &docs, ordered, bypass)?;
        return Ok(w.finish(reply));
    }
    let mut errors: Vec<Value> = Vec::new();
    let host = super::host_port(ctx.backend);
    let refused = names::cannot_write(&ns, &host).map_or(Ok(()), Err);
    let checked =
        refused.and_then(|()| names::check_writable(&ns)).and_then(|()| match expected_uuid {
            Some(u) => uuid::check(ctx.backend, &ns, u),
            None => Ok(()),
        });
    let coll = match checked.and_then(|()| ensure_collection(ctx, &ns)) {
        Ok(c) => c,
        Err(e) if fails_command(&e) => return Err(e),
        Err(e) => {
            let n = if ordered { 1 } else { docs.len() };
            errors.extend((0..n).map(|i| write_error(i, &e)));
            return Ok(w.finish(Document::new().with("n", 0).with("writeErrors", errors)));
        }
    };
    let rules = rules_of(&ns, &coll, bypass, None, index_rules(ctx.backend, &coll)?)?;
    let mut n: i64 = 0;
    let mut chunk: Vec<(usize, Document, Vec<u8>)> = Vec::new();
    let mut bytes = 0usize;
    let mut stop = false;
    for (i, d) in docs.iter().enumerate() {
        let prepared = match d {
            Value::Document(d) => with_id(ctx.backend, ctx.shared, d).and_then(|doc| {
                let id = doc.get("_id").cloned().unwrap_or(Value::Null);
                insert_key(&coll, &id).map(|k| (doc, k))
            }),
            other => Err(wrong_type(&format!("insert.documents.{i}"), other, "object")),
        };
        let prepared = prepared.and_then(|(doc, key)| {
            let size = stored_bytes(&doc, Stored::Insert)?.len();
            Ok((doc, key, size))
        });
        match prepared {
            Ok((doc, key, size)) => {
                bytes += size;
                chunk.push((i, doc, key));
            }
            Err(e) => {
                stop |=
                    flush(ctx, &mut w, (&coll, &rules), ordered, &mut chunk, &mut n, &mut errors);
                errors.push(write_error(i, &e));
                if ordered {
                    stop = true;
                }
            }
        }
        if stop {
            break;
        }
        if chunk.len() >= INSERT_CHUNK_DOCS || bytes >= INSERT_CHUNK_BYTES {
            bytes = 0;
            if flush(ctx, &mut w, (&coll, &rules), ordered, &mut chunk, &mut n, &mut errors) {
                stop = true;
                break;
            }
        }
    }
    if !stop {
        flush(ctx, &mut w, (&coll, &rules), ordered, &mut chunk, &mut n, &mut errors);
    }
    let mut reply = Document::new().with("n", count(n));
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    Ok(w.finish(reply))
}

/// Writes the documents gathered; `true` if an ordered insert has to stop.
fn flush<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    w: &mut Writer,
    (coll, rules): (&CollectionEntry, &Rules),
    ordered: bool,
    chunk: &mut Vec<(usize, Document, Vec<u8>)>,
    n: &mut i64,
    errors: &mut Vec<Value>,
) -> bool {
    if chunk.is_empty() {
        return false;
    }
    let items = std::mem::take(chunk);
    // A capped collection takes its documents one by one, each at the next `seq` (015 FR-003).
    let one_by_one = rules.capped.is_some();
    if one_by_one && items.len() > 1 {
        for item in items {
            if flush(ctx, w, (coll, rules), ordered, &mut vec![item], n, errors) {
                return true;
            }
        }
        return false;
    }
    let first = items[0].0;
    // A capped insert has no key of its own (`insert_key`).
    let keys: Vec<Vec<u8>> =
        items.iter().map(|(_, _, k)| k.clone()).filter(|k| !k.is_empty()).collect();
    let docs: Vec<Document> = items.iter().map(|(_, d, _)| d.clone()).collect();
    let bound =
        Bound { mutation: DocMutation::Insert { docs, ordered, rules: rules.clone() }, keys };
    match run(ctx.backend, ctx.shared, w, coll, &bound, stmt_id(ctx.cmd, first)) {
        Ok(r) => {
            if r.retried {
                // A statement that failed was not recorded: it runs again (measured).
                let failed: Vec<usize> = r.errors.iter().map(|e| e.index).collect();
                let done = items.iter().enumerate().filter(|(k, _)| !failed.contains(k));
                w.note_retried(done.map(|(_, (i, _, _))| stmt_id(ctx.cmd, *i)));
            }
            *n += r.n;
            for e in &r.errors {
                errors.push(write_error(items[e.index].0, &e.error));
            }
            ordered && !r.errors.is_empty()
        }
        Err(e) => {
            if ordered {
                errors.push(write_error(first, &e));
            } else {
                errors.extend(items.iter().map(|(i, _, _)| write_error(*i, &e)));
            }
            ordered
        }
    }
}

/// The `_id` an equality filter names (`{_id: v}` or `{_id: {$eq: v}}`), if it is that simple.
pub(crate) fn id_equality(filter: &Document) -> Option<Value> {
    let v = filter.get("_id")?;
    let v = match v {
        Value::Document(d) if d.len() == 1 && d.get("$eq").is_some() => d.get("$eq")?,
        Value::Document(d) if d.iter().any(|(k, _)| k.starts_with('$')) => return None,
        Value::Regex { .. } | Value::Array(_) => return None,
        other => other,
    };
    check_id(v).ok()?;
    Some(v.clone())
}

/// The keys of the documents `filter` may match: the `_id` equality's key, or those a scan finds
/// (only the first unless `multi`).
fn candidates<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    m: &Modify,
    collation: Option<&Document>,
) -> Result<Vec<Vec<u8>>, CommandError> {
    if let Some(id) = id_equality(&m.filter) {
        return Ok(id_doc_key(backend, coll, &id)?.into_iter().collect());
    }
    // An index plan that wins gives the documents, and a write of one takes its first (T019).
    if let Some(keys) = super::plan_exec::planned_keys(backend, coll, m)? {
        return Ok(keys);
    }
    let mut keys = Vec::new();
    let (range, vars) = (documents_of(coll), m.vars.as_ref());
    for_each_match_with(backend, range, &m.filter, collation, vars, ReadConcern::Local, |k, _| {
        keys.push(k);
        Ok(m.multi)
    })?;
    Ok(keys)
}

/// Writes the validation lines the write made (`warn` and `errorAndLog`), as a `refrouter` 9.0.4's
/// shard does (measured: `{"namespace", "document", "errInfo"}`).
/// With `redact`, the document and `errInfo` go redacted (`redactClientLogData`, 013 FR-027).
fn log_validation<B: Backend + ?Sized>(
    backend: &mut B,
    bound: &Bound,
    r: &MutationResult,
    redact: bool,
) {
    use super::mutation::{LOG_FAILED, LOG_WOULD_FAIL};
    let ns = match &bound.mutation {
        DocMutation::Insert { rules, .. }
        | DocMutation::Update { rules, .. }
        | DocMutation::Delete { rules, .. }
        | DocMutation::Backfill { rules }
        | DocMutation::Erase { rules } => rules.ns.as_str(),
    };
    for line in &r.warnings {
        let id = match line.get(super::mutation::LOG_ID_FIELD) {
            Some(Value::Int32(id)) => *id,
            _ => continue,
        };
        let msg = if id == LOG_FAILED {
            "Document failed validation"
        } else if id == LOG_WOULD_FAIL {
            "Document would fail validation"
        } else {
            continue;
        };
        let mut attr = Document::new().with("namespace", ns);
        for k in [super::mutation::LOG_DOCUMENT_FIELD, super::mutation::LOG_ERR_INFO_FIELD] {
            if let Some(v) = line.get(k) {
                attr.push(k, if redact { crate::redact::value(v) } else { v.clone() });
            }
        }
        backend.log_warning(id, msg, &attr);
    }
}

/// The current time for an upsert's `$currentDate`, from the cluster time.
pub(crate) fn now_of<B: Backend + ?Sized>(b: &B) -> Now {
    let ms = b.now_ms();
    Now {
        millis: i64::try_from(ms).unwrap_or(i64::MAX),
        timestamp: (u32::try_from(ms / 1000).unwrap_or(u32::MAX), 1),
    }
}

/// A part of a command (filter, update, projection) in BSON, within a command's limits.
fn raw(d: &Document) -> Result<Vec<u8>, CommandError> {
    d.to_bytes_with(command_limits()).map_err(CommandError::from)
}

/// The document an upsert inserts for `filter` and `update`.
pub(crate) fn upsert_doc<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    op: &str,
    filter: &Document,
    update: &Value,
    array_filters: Option<&[Value]>,
) -> Result<Document, CommandError> {
    let holder = Document::new()
        .with("q", filter.clone())
        .with("u", update.clone())
        .with("af", array_filters.map_or(Value::Null, |a| Value::Array(a.to_vec())));
    let hb = raw(&holder)?;
    let h = RawDoc::from_bytes_with(&hb, command_limits())?;
    let Some(teseladb_bson::RawValue::Document(q)) = h.get("q") else {
        return Err(CommandError::new(BAD_VALUE, "bad filter"));
    };
    let f = Filter::parse(q, FilterUse::Query).map_err(from_mql)?;
    teseladb_mql::update::check_upsert_filter(&f).map_err(from_mql)?;
    let af = match h.get("af") {
        Some(teseladb_bson::RawValue::Array(a)) => Some(a),
        _ => None,
    };
    let u = Update::parse(h.get("u").ok_or(CommandError::new(BAD_VALUE, "bad update"))?, af)
        .map_err(from_mql)?;
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &super::engines::NodeExtensions, vars: None },
        position: None,
        now: now_of(backend),
        insert: true,
    };
    let oid = new_object_id(backend, shared);
    let during = executor_error(op);
    let doc = upsert(&f, &u, &ctx, || Value::ObjectId(oid)).map_err(from_mql).map_err(&during)?;
    if let Some(id) = doc.get("_id") {
        check_id(id).map_err(&during)?;
    }
    Ok(doc)
}

/// One update or delete statement, already read.
#[derive(Clone)]
pub struct Modify {
    /// Filter.
    pub filter: Document,
    /// The update (`None` for a delete).
    pub update: Option<Value>,
    /// `arrayFilters`.
    pub array_filters: Option<Vec<Value>>,
    /// Every matching document.
    pub multi: bool,
    /// Insert when nothing matches.
    pub upsert: bool,
    /// Collation.
    pub collation: Option<Document>,
    /// `sort` of a `findAndModify`.
    pub sort: Option<SortSpec>,
    /// Version a `findAndModify` returns.
    pub image: Image,
    /// The command's `let` variables, resolved (FR-032).
    pub vars: Option<Document>,
    /// The index `hint` names (a name or a key pattern).
    pub hint: Option<Value>,
}

/// Runs one update or delete statement: the sum of its mutations' results.
pub fn modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    w: &mut Writer,
    ns: &Namespace,
    m: &Modify,
    bypass: bool,
    stmt: u32,
) -> Result<MutationResult, CommandError> {
    // Parse first: a malformed filter or update fails even on a missing collection.
    let probe = Document::new().with("q", m.filter.clone());
    let pb = raw(&probe)?;
    if let Some(teseladb_bson::RawValue::Document(q)) =
        RawDoc::from_bytes_with(&pb, command_limits()).ok().and_then(|d| d.get("q"))
    {
        Filter::parse(q, FilterUse::Query).map_err(from_mql)?;
        // A constant of an `$expr` that fails fails the statement before any document is read.
        teseladb_exec::expr::optimize::optimize_filter(&m.filter, m.vars.as_ref())
            .map_err(from_mql)?;
    }
    let host = super::host_port(ctx.backend);
    if let Some(e) = names::cannot_write(ns, &host) {
        return Err(e);
    }
    let existing = ctx.backend.collection(ns)?;
    let coll = match (existing, m.upsert && m.update.is_some()) {
        (Some(c), _) => {
            super::kinds::not_a_view(ns, &c)?;
            c
        }
        (None, true) => {
            names::check_writable(ns)?;
            ensure_collection(ctx, ns)?
        }
        (None, false) => {
            super::catalog::ensure_database(ctx.backend, &ns.db)?;
            return Ok(MutationResult::default());
        }
    };
    let rules = Rules {
        vars: m.vars.clone(),
        ..rules_of(ns, &coll, bypass, m.collation.clone(), index_rules(ctx.backend, &coll)?)?
    };
    // The measurements of a time series live in its buckets (015 FR-007); with `rawData` the
    // statement is on the buckets themselves.
    let raw = matches!(ctx.cmd.get("rawData"), Some(Value::Boolean(true)));
    if let Some(spec) = super::timeseries::spec_of(&coll)?.filter(|_| !raw) {
        return super::timeseries::modify(ctx.backend, w, (&coll, &spec), m, &rules, stmt);
    }
    // A scan counts once the statement ends without an error (measured on a `refnode` 9.0.4).
    let scans = m.sort.is_some() || id_equality(&m.filter).is_none();
    let mut keys = match &m.sort {
        Some(sort) => sorted_first(ctx.backend, &coll, m, rules.collation.as_ref(), sort)?,
        None => candidates(ctx.backend, &coll, m, rules.collation.as_ref())?,
    };
    let upsert_doc = match (&m.update, m.upsert) {
        (Some(u), true) => {
            let op = executor_of(m.image);
            Some(upsert_doc(ctx.backend, ctx.shared, op, &m.filter, u, m.array_filters.as_deref())?)
        }
        _ => None,
    };
    let mut total = MutationResult::default();
    let chunks: Vec<Vec<Vec<u8>>> = if keys.is_empty() {
        vec![Vec::new()]
    } else {
        std::mem::take(&mut keys).chunks(MODIFY_CHUNK_KEYS).map(<[Vec<u8>]>::to_vec).collect()
    };
    let last = chunks.len() - 1;
    for (i, mut chunk) in chunks.into_iter().enumerate() {
        // The upsert goes with the last chunk, when nothing else matched.
        let up = if i == last && total.n == 0 { upsert_doc.clone() } else { None };
        // A capped collection's upsert takes the next `seq` on the range, and its writes are
        // routed by the collection's state.
        let capped = rules.capped.is_some();
        if let Some(d) = up.as_ref().filter(|_| !capped) {
            chunk.push(doc_key(&coll, d.get("_id").unwrap_or(&Value::Null))?);
        }
        // Nothing matched: the write still goes to the collection's range, which applies nothing
        // and answers with the write concern, as RefDb does (a write needs a key to be routed).
        if chunk.is_empty() && !capped {
            chunk.push(documents_of(&coll).0);
        }
        let mutation = match &m.update {
            Some(u) => DocMutation::Update {
                filter: m.filter.clone(),
                update: u.clone(),
                array_filters: m.array_filters.clone(),
                multi: m.multi,
                upsert: up,
                image: m.image,
                rules: rules.clone(),
            },
            None => DocMutation::Delete {
                filter: m.filter.clone(),
                multi: m.multi,
                image: m.image,
                rules: rules.clone(),
            },
        };
        let r = run(ctx.backend, ctx.shared, w, &coll, &Bound { mutation, keys: chunk }, stmt)?;
        if let Some(e) = r.errors.first() {
            return Err(e.error.clone());
        }
        total.n += r.n;
        total.n_modified += r.n_modified;
        total.upserted = total.upserted.or(r.upserted);
        total.value = total.value.or(r.value);
        total.warnings.extend(r.warnings);
        if r.retried {
            w.note_retried([stmt]);
        }
        if !m.multi && total.n > 0 {
            break;
        }
    }
    if scans {
        super::query::count_scan(ctx.shared, ns);
    }
    Ok(total)
}

/// The key of the first matching document in `sort` order.
fn sorted_first<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    m: &Modify,
    collation: Option<&Document>,
    sort: &SortSpec,
) -> Result<Vec<Vec<u8>>, CommandError> {
    let mut best: Option<(Vec<super::sort::SortKey>, Vec<u8>)> = None;
    for_each_match_with(
        backend,
        documents_of(coll),
        &m.filter,
        collation,
        m.vars.as_ref(),
        ReadConcern::Local,
        |k, bytes| {
            let doc = RawDoc::from_bytes(bytes)?;
            let keys = sort.keys(doc, None);
            let better =
                best.as_ref().is_none_or(|(b, _)| sort.compare_keys(&keys, b, None).is_lt());
            if better {
                best = Some((keys, k));
            }
            Ok(true)
        },
    )?;
    Ok(best.map(|(_, k)| vec![k]).unwrap_or_default())
}

/// The fields of a statement in `updates` (`UpdateOpEntry`, measured in `refrouter` 9.0.4).
const UPDATE_STATEMENT_FIELDS: [&str; 11] = [
    "q",
    "u",
    "c",
    "upsert",
    "multi",
    "collation",
    "arrayFilters",
    "hint",
    "sort",
    "sampleId",
    "upsertSupplied",
];

/// `IDLUnknownField` for the first field of a statement that is not in `known`.
fn unknown_field(d: &Document, name: &str, known: &[&str]) -> Result<(), CommandError> {
    match d.iter().find(|(k, _)| !known.contains(k)) {
        Some((k, _)) => Err(CommandError::new(
            IDL_UNKNOWN_FIELD,
            format!("BSON field '{name}.{k}' is an unknown field."),
        )),
        None => Ok(()),
    }
}

/// What the parser of an update statement checks: its fields and their types. A statement it
/// rejects fails the whole command (measured).
/// What an update statement says beyond its `Modify`, checked when the statement runs.
pub(crate) struct UpdateFlags {
    /// Its constants (`c`), which its pipeline reads as variables.
    constants: Option<Document>,
    /// `upsertSupplied: true`.
    upsert_supplied: bool,
}

pub(crate) fn update_statement(v: &Value, i: usize) -> Result<(Modify, UpdateFlags), CommandError> {
    let Value::Document(d) = v else {
        return Err(wrong_type(&format!("update.updates.{i}"), v, "object"));
    };
    let name = "update.updates";
    unknown_field(d, name, &UPDATE_STATEMENT_FIELDS)?;
    let filter = args::doc(d, name, "q")?.cloned().ok_or_else(|| missing(name, "q"))?;
    let update = d.get("u").cloned().ok_or_else(|| missing(name, "u"))?;
    let multi = args::bool_field(d, name, "multi")?.unwrap_or(false);
    let flags = UpdateFlags {
        constants: args::doc(d, name, "c")?.cloned(),
        upsert_supplied: args::bool_field(d, name, "upsertSupplied")?.unwrap_or(false),
    };
    let m = Modify {
        filter,
        update: Some(update),
        array_filters: args::array(d, name, "arrayFilters")?.map(<[Value]>::to_vec),
        multi,
        upsert: args::bool_field(d, name, "upsert")?.unwrap_or(false),
        collation: args::doc(d, name, "collation")?.cloned(),
        // 9.0: a single update takes the first document in this order.
        sort: match args::doc(d, name, "sort")? {
            Some(s) if !s.is_empty() => Some(SortSpec::parse(s)?),
            _ => None,
        },
        image: Image::None,
        vars: None,
        hint: d.get("hint").cloned(),
    };
    Ok((m, flags))
}

/// What an update statement checks when it runs; a failure is that statement's write error.
pub(crate) fn check_update_statement(
    (m, flags): &(Modify, UpdateFlags),
) -> Result<(), CommandError> {
    let pipeline = matches!(m.update, Some(Value::Array(_)));
    let replacement = matches!(&m.update, Some(Value::Document(u)) if u.iter().next().is_none_or(|(k, _)| !k.starts_with('$')));
    if flags.constants.is_some() && !pipeline {
        return Err(CommandError::new(
            CONSTANTS_ONLY_FOR_PIPELINE,
            "Constant values may only be specified for pipeline updates",
        ));
    }
    if m.multi && m.sort.is_some() {
        return Err(CommandError::new(FAILED_TO_PARSE, "Cannot specify sort with multi=true"));
    }
    if m.multi && replacement {
        return Err(CommandError::new(
            FAILED_TO_PARSE,
            "multi update is not supported for replacement-style update",
        ));
    }
    if flags.upsert_supplied && !m.upsert {
        return Err(CommandError::new(
            FAILED_TO_PARSE,
            "cannot specify 'upsertSupplied: true' for a non-upsert operation",
        ));
    }
    // A pipeline is parsed with the statement: its stages and variables (measured).
    if let Some(u @ Value::Array(_)) = &m.update {
        super::engines::check_update_pipeline(u, m.vars.as_ref()).map_err(from_mql)?;
    }
    Ok(())
}

/// The variables of an update statement: the command's `let`, then its constants (`c`).
fn statement_vars(vars: Option<&Document>, flags: &UpdateFlags) -> Option<Document> {
    match (vars, &flags.constants) {
        (v, None) => v.cloned(),
        (v, Some(c)) => {
            let mut all = v.cloned().unwrap_or_default();
            for (k, x) in c.iter() {
                all.insert(k, x.clone());
            }
            Some(all)
        }
    }
}

/// A `refrouter` 9.0 refuses the whole command when an update is neither a document nor a
/// pipeline (measured).
fn check_updates(items: &[Value]) -> Result<(), CommandError> {
    let bad = items.iter().any(|i| {
        matches!(i, Value::Document(d) if d.get("u").is_some_and(|u| !matches!(u, Value::Document(_) | Value::Array(_))))
    });
    match bad {
        true => Err(CommandError::new(
            FAILED_TO_PARSE,
            "Update argument must be either an object or an array",
        )),
        false => Ok(()),
    }
}

/// `update`.
pub fn update<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    // A required field that is missing fails before the collection name (measured).
    let items = batch(ctx.cmd, UPDATE, "updates")?.to_vec();
    let ns = target(ctx)?;
    check_updates(&items)?;
    let ordered = args::bool_field(ctx.cmd, UPDATE, "ordered")?.unwrap_or(true);
    let bypass =
        args::safe_bool(ctx.cmd, UPDATE, "bypassDocumentValidation", &WRITE_SAFE_BOOL_TYPES)?
            .unwrap_or(false);
    let vars = super::query::command_let(ctx.backend, ctx.cmd, UPDATE)?;
    let statements: Vec<(Modify, UpdateFlags)> = items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            update_statement(v, i)
                .map(|(m, f)| (Modify { vars: statement_vars(vars.as_ref(), &f), ..m }, f))
        })
        .collect::<Result<_, _>>()?;
    let expected_uuid = uuid::expected(ctx.cmd, UPDATE)?;
    let mut w = Writer::new(ctx, UPDATE)?;
    let (mut n, mut n_modified) = (0i64, 0i64);
    let (mut upserted, mut errors): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
    // A time series refuses updates of its own before the general checks (015 FR-007).
    let series = match ctx.backend.collection(&ns)? {
        Some(c) => super::timeseries::spec_of(&c)?,
        None => None,
    };
    // The users and roles of Group 0 and `config.settings` run the whole command as one batch.
    let checked = || -> Vec<Result<Modify, CommandError>> {
        statements.iter().map(|st| check_update_statement(st).map(|()| st.0.clone())).collect()
    };
    let mut direct = direct_modify(ctx, &ns, checked, ordered, bypass)?.map(Vec::into_iter);
    for (i, statement) in statements.iter().enumerate() {
        let r = match direct.as_mut() {
            Some(results) => match results.next() {
                Some(r) => r,
                None => break,
            },
            None => super::timeseries::check_statement(series.as_ref(), &statement.0)
                .and_then(|()| check_update_statement(statement))
                .and_then(|()| check_uuid(ctx, &ns, expected_uuid))
                .and_then(|()| modify(ctx, &mut w, &ns, &statement.0, bypass, stmt_id(ctx.cmd, i))),
        };
        match r {
            Err(e) if fails_command(&e) => return Err(e),
            Ok(r) => {
                n += r.n;
                n_modified += r.n_modified;
                if let Some(id) = r.upserted {
                    n += 1;
                    upserted.push(Value::Document(
                        Document::new().with("index", i as i32).with("_id", id),
                    ));
                }
            }
            Err(e) => {
                errors.push(write_error(i, &e));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = Document::new().with("nModified", count(n_modified)).with("n", count(n));
    if !upserted.is_empty() {
        reply.push("upserted", upserted);
    }
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    Ok(w.finish(reply))
}

/// The fields of a statement in `deletes` (`DeleteOpEntry`, measured in `refrouter` 9.0.4).
const DELETE_STATEMENT_FIELDS: [&str; 5] = ["q", "limit", "collation", "hint", "sampleId"];

pub(crate) fn delete_statement(v: &Value, i: usize) -> Result<Modify, CommandError> {
    let Value::Document(d) = v else {
        return Err(wrong_type(&format!("delete.deletes.{i}"), v, "object"));
    };
    let name = "delete.deletes";
    unknown_field(d, name, &DELETE_STATEMENT_FIELDS)?;
    let filter = args::doc(d, name, "q")?.cloned().ok_or_else(|| missing(name, "q"))?;
    let limit = d.get("limit").ok_or_else(|| missing(name, "limit"))?;
    // RefDb reads the limit as a double, and any other type as 0.
    let limit = crate::types::number(limit).unwrap_or(0.0);
    if limit != 0.0 && limit != 1.0 {
        return Err(CommandError::new(
            FAILED_TO_PARSE,
            format!(
                "The limit field in delete objects must be 0 or 1. Got {}",
                teseladb_mql::stream_double(limit)
            ),
        ));
    }
    Ok(Modify {
        filter,
        update: None,
        array_filters: None,
        multi: limit == 0.0,
        upsert: false,
        collation: args::doc(d, name, "collation")?.cloned(),
        sort: None,
        image: Image::None,
        vars: None,
        hint: d.get("hint").cloned(),
    })
}

/// Checks the `collectionUUID` a write command gave, if any.
fn check_uuid<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    expected: Option<[u8; 16]>,
) -> Result<(), CommandError> {
    expected.map_or(Ok(()), |u| uuid::check(ctx.backend, ns, u))
}

/// `delete`.
pub fn delete<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    // A required field that is missing fails before the collection name (measured).
    let items = batch(ctx.cmd, DELETE, "deletes")?.to_vec();
    let ns = target(ctx)?;
    let ordered = args::bool_field(ctx.cmd, DELETE, "ordered")?.unwrap_or(true);
    // A statement the parser rejects fails the whole command before any delete runs.
    let vars = super::query::command_let(ctx.backend, ctx.cmd, DELETE)?;
    let statements: Vec<Modify> = items
        .iter()
        .enumerate()
        .map(|(i, v)| delete_statement(v, i).map(|m| Modify { vars: vars.clone(), ..m }))
        .collect::<Result<_, _>>()?;
    let expected_uuid = uuid::expected(ctx.cmd, DELETE)?;
    let mut w = Writer::new(ctx, DELETE)?;
    let mut n = 0i64;
    let mut errors: Vec<Value> = Vec::new();
    let all = || statements.iter().cloned().map(Ok).collect();
    let mut direct = direct_modify(ctx, &ns, all, ordered, false)?.map(Vec::into_iter);
    for (i, m) in statements.iter().enumerate() {
        let r = match direct.as_mut() {
            Some(results) => match results.next() {
                Some(r) => r,
                None => break,
            },
            None => check_uuid(ctx, &ns, expected_uuid)
                .and_then(|()| modify(ctx, &mut w, &ns, m, false, stmt_id(ctx.cmd, i))),
        };
        match r {
            Err(e) if fails_command(&e) => return Err(e),
            Ok(r) => n += r.n,
            Err(e) => {
                errors.push(write_error(i, &e));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = Document::new().with("n", count(n));
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    Ok(w.finish(reply))
}

/// `findAndModify`.
pub fn find_and_modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let name = FIND_AND_MODIFY;
    let cmd = ctx.cmd;
    let remove = args::safe_bool(cmd, name, "remove", &WRITE_SAFE_BOOL_TYPES)?.unwrap_or(false);
    let update = cmd.get("update").cloned();
    let new = args::safe_bool(cmd, name, "new", &WRITE_SAFE_BOOL_TYPES)?.unwrap_or(false);
    let upsert_flag =
        args::safe_bool(cmd, name, "upsert", &WRITE_SAFE_BOOL_TYPES)?.unwrap_or(false);
    match (remove, &update) {
        (true, Some(_)) => {
            return Err(CommandError::new(
                FAILED_TO_PARSE,
                "Cannot specify both an 'update' and 'remove'=true",
            ));
        }
        (false, None) => {
            return Err(CommandError::new(
                FAILED_TO_PARSE,
                "Either an update or remove=true must be specified",
            ));
        }
        (true, None) if upsert_flag => {
            return Err(CommandError::new(
                FAILED_TO_PARSE,
                "Cannot specify both 'upsert'=true and 'remove'=true ",
            ));
        }
        (true, None) if new => {
            return Err(CommandError::new(
                FAILED_TO_PARSE,
                "Cannot specify both 'new'=true and 'remove'=true; 'remove' always returns the deleted document",
            ));
        }
        _ => {}
    }
    let fields = args::doc(cmd, name, "fields")?.cloned();
    // The projection is read before anything is written (measured).
    if let Some(p) = fields.as_ref().filter(|p| !p.is_empty()) {
        let pb = raw(p)?;
        teseladb_mql::projection::Projection::parse(RawDoc::from_bytes_with(
            &pb,
            command_limits(),
        )?)
        .map_err(from_mql)?;
    }
    let m = Modify {
        filter: args::doc(cmd, name, "query")?.cloned().unwrap_or_default(),
        update,
        array_filters: args::array(cmd, name, "arrayFilters")?.map(<[Value]>::to_vec),
        multi: false,
        upsert: upsert_flag,
        collation: args::doc(cmd, name, "collation")?.cloned(),
        sort: match args::doc(cmd, name, "sort")? {
            Some(s) if !s.is_empty() => Some(SortSpec::parse(s)?),
            _ => None,
        },
        image: if new { Image::After } else { Image::Before },
        vars: super::query::command_let(ctx.backend, cmd, name)?,
        hint: cmd.get("hint").cloned(),
    };
    let bypass = args::safe_bool(cmd, name, "bypassDocumentValidation", &WRITE_SAFE_BOOL_TYPES)?
        .unwrap_or(false);
    let mut w = Writer::new(ctx, name)?;
    let r = match direct_modify(ctx, &ns, || vec![Ok(m.clone())], true, bypass)? {
        Some(mut results) => results.pop().unwrap_or_else(|| Ok(MutationResult::default()))?,
        None => modify(ctx, &mut w, &ns, &m, bypass, stmt_id(ctx.cmd, 0)).map_err(routing_error)?,
    };
    let mut last = Document::new().with("n", if r.upserted.is_some() { 1 } else { r.n as i32 });
    if !remove {
        last.push("updatedExisting", r.n > 0);
        if let Some(id) = &r.upserted {
            last.push("upserted", id.clone());
        }
    }
    let value = match (r.value, fields) {
        (Some(v), Some(p)) if !p.is_empty() => project(&v, &p)?,
        (Some(v), _) => Value::Document(v),
        (None, _) => Value::Null,
    };
    Ok(w.finish(Document::new().with("lastErrorObject", last).with("value", value)))
}

pub(crate) fn project(doc: &Document, spec: &Document) -> Result<Value, CommandError> {
    let (db, sb) = (raw(doc)?, raw(spec)?);
    let p = teseladb_mql::projection::Projection::parse(RawDoc::from_bytes_with(
        &sb,
        command_limits(),
    )?)
    .map_err(from_mql)?;
    let ctx = teseladb_mql::projection::ProjectionContext {
        matcher: MatchContext { collation: None, ext: &super::engines::NodeExtensions, vars: None },
        position: None,
        sort_key: None,
    };
    Ok(Value::Document(p.apply(RawDoc::from_bytes(&db)?, &ctx).map_err(from_mql)?))
}

/// `bulkWrite` (on `admin`): inserts, updates and deletes over several namespaces, with one
/// result per operation in a cursor.
pub fn bulk_write<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = BULK_WRITE;
    let ops = args::array(ctx.cmd, name, "ops")?.ok_or_else(|| missing(name, "ops"))?.to_vec();
    let ns_info =
        args::array(ctx.cmd, name, "nsInfo")?.ok_or_else(|| missing(name, "nsInfo"))?.to_vec();
    let ordered = args::bool_field(ctx.cmd, name, "ordered")?.unwrap_or(true);
    let errors_only = args::bool_field(ctx.cmd, name, "errorsOnly")?.unwrap_or(false);
    let bypass = args::safe_bool(ctx.cmd, name, "bypassDocumentValidation", &BULK_SAFE_BOOL_TYPES)?
        .unwrap_or(false);
    let mut spaces = Vec::new();
    for (i, v) in ns_info.iter().enumerate() {
        let Value::Document(d) = v else {
            return Err(wrong_type(&format!("bulkWrite.nsInfo.{i}"), v, "object"));
        };
        let full_name = args::string(d, "NamespaceInfoEntry", "ns")?
            .ok_or_else(|| missing("NamespaceInfoEntry", "ns"))?;
        let ns = names::parse_full(full_name).ok_or_else(|| {
            CommandError::new(
                crate::error::INVALID_NAMESPACE,
                format!("Invalid namespace specified '{full_name}'"),
            )
        })?;
        names::check(&ns)?;
        spaces.push(ns);
    }
    let vars = super::query::command_let(ctx.backend, ctx.cmd, name)?;
    let with_vars = |(kind, ns, body): (BulkKind, Namespace, BulkBody)| match body {
        BulkBody::Modify(m) => {
            (kind, ns, BulkBody::Modify(Box::new(Modify { vars: vars.clone(), ..*m })))
        }
        insert => (kind, ns, insert),
    };
    let parsed: Vec<(BulkKind, Namespace, BulkBody)> = ops
        .iter()
        .enumerate()
        .map(|(idx, op)| parse_bulk_op(op, idx, &spaces).map(with_vars))
        .collect::<Result<_, _>>()?;
    // Every database is created before any operation runs (D-077, measured on 9.0.4).
    for ns in &spaces {
        if ctx.backend.collection(ns)?.is_none() {
            super::catalog::ensure_database(ctx.backend, &ns.db).map_err(routing_error)?;
        }
    }
    let mut w = Writer::new(ctx, name)?;
    let mut results = Vec::new();
    let (
        mut n_inserted,
        mut n_matched,
        mut n_modified,
        mut n_upserted,
        mut n_deleted,
        mut n_errors,
    ) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for (idx, op) in parsed.iter().enumerate() {
        let r = bulk_op(ctx, &mut w, op, bypass, stmt_id(ctx.cmd, idx));
        let mut res = Document::new().with("ok", 1.0).with("idx", idx as i32);
        match r {
            Ok((kind, r)) => {
                match kind {
                    BulkKind::Insert => n_inserted += r.n,
                    BulkKind::Update => {
                        n_matched += r.n;
                        n_modified += r.n_modified;
                        if r.upserted.is_some() {
                            n_upserted += 1;
                        }
                    }
                    BulkKind::Delete => n_deleted += r.n,
                }
                if errors_only {
                    continue;
                }
                res.push("n", (r.n + i64::from(r.upserted.is_some())) as i32);
                if kind == BulkKind::Update {
                    res.push("nModified", r.n_modified as i32);
                    if let Some(id) = r.upserted {
                        res.push("upserted", Document::new().with("_id", id));
                    }
                }
                results.push(Value::Document(res));
            }
            Err(e) => {
                n_errors += 1;
                let mut res = Document::new()
                    .with("ok", 0.0)
                    .with("idx", idx as i32)
                    .with("code", e.code.code)
                    .with("codeName", e.code.name)
                    .with("errmsg", e.errmsg.as_str())
                    .with("n", 0);
                for (k, v) in e.extra.iter() {
                    res.push(k, v.clone());
                }
                results.push(Value::Document(res));
                if ordered {
                    break;
                }
            }
        }
    }
    let cursor = Document::new()
        .with("id", 0i64)
        .with("firstBatch", results)
        .with("ns", "admin.$cmd.bulkWrite");
    let reply = Document::new()
        .with("cursor", cursor)
        .with("nErrors", n_errors as i32)
        .with("nInserted", n_inserted as i32)
        .with("nMatched", n_matched as i32)
        .with("nModified", n_modified as i32)
        .with("nUpserted", n_upserted as i32)
        .with("nDeleted", n_deleted as i32)
        // Always, also empty (measured on a `refrouter` 9.0.4).
        .with(RETRIED_STMT_IDS, w.retried_value());
    Ok(w.finish(reply))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BulkKind {
    Insert,
    Update,
    Delete,
}

/// The fields each kind of `bulkWrite` operation takes (measured in `refrouter` 9.0.4).
const BULK_INSERT_FIELDS: [&str; 2] = ["insert", "document"];
const BULK_UPDATE_FIELDS: [&str; 12] = [
    "update",
    "filter",
    "multi",
    "updateMods",
    "upsert",
    "arrayFilters",
    "hint",
    "sort",
    "constants",
    "collation",
    "sampleId",
    "upsertSupplied",
];
const BULK_DELETE_FIELDS: [&str; 6] =
    ["delete", "filter", "multi", "hint", "collation", "sampleId"];
/// The numeric types of an operation's `nsInfo` index, in a `refrouter` 9.0.4's order.
const BULK_INDEX_TYPES: [&str; 4] = ["int", "double", "long", "decimal"];

/// A `bulkWrite` operation once the parser has read it.
enum BulkBody {
    Insert(Document),
    Modify(Box<Modify>),
}

/// Reads operation `idx` of a `bulkWrite`; what it refuses fails the whole command.
fn parse_bulk_op(
    op: &Value,
    idx: usize,
    spaces: &[Namespace],
) -> Result<(BulkKind, Namespace, BulkBody), CommandError> {
    let Value::Document(d) = op else {
        return Err(wrong_type(&format!("bulkWrite.ops.{idx}"), op, "object"));
    };
    let Some((kind, which)) = d.iter().next() else {
        return Err(CommandError::new(
            IDL_UNKNOWN_FIELD,
            "BSON field 'bulkWrite' is an unknown field.",
        ));
    };
    let (kind, fields): (BulkKind, &[&str]) = match kind {
        "insert" => (BulkKind::Insert, &BULK_INSERT_FIELDS),
        "update" => (BulkKind::Update, &BULK_UPDATE_FIELDS),
        "delete" => (BulkKind::Delete, &BULK_DELETE_FIELDS),
        other => {
            let msg = format!("BSON field 'bulkWrite.{other}' is an unknown field.");
            return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
        }
    };
    let name = "bulkWrite.ops";
    unknown_field(d, name, fields)?;
    let field = d.iter().next().map_or("", |(k, _)| k);
    let mut one = Document::new();
    one.push(field, which.clone());
    let index = match args::int_of(&one, name, field, &BULK_INDEX_TYPES)? {
        Some(i) if i < 0 => {
            let msg = format!("BSON field '{field}' value must be >= 0, actual value '{i}'");
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        i => i.and_then(|i| usize::try_from(i).ok()).unwrap_or(usize::MAX),
    };
    let modify =
        |update: Option<Value>, multi: bool, upsert: bool| -> Result<Modify, CommandError> {
            Ok(Modify {
                filter: args::doc(d, name, "filter")?
                    .cloned()
                    .ok_or_else(|| missing(name, "filter"))?,
                update,
                array_filters: args::array(d, name, "arrayFilters")?.map(<[Value]>::to_vec),
                multi,
                upsert,
                collation: args::doc(d, name, "collation")?.cloned(),
                sort: None,
                image: Image::None,
                vars: None,
                hint: d.get("hint").cloned(),
            })
        };
    let body = match kind {
        BulkKind::Insert => BulkBody::Insert(
            args::doc(d, name, "document")?.cloned().ok_or_else(|| missing(name, "document"))?,
        ),
        BulkKind::Update => {
            let mods = d.get("updateMods").cloned().ok_or_else(|| missing(name, "updateMods"))?;
            let multi = args::bool_field(d, name, "multi")?.unwrap_or(false);
            let upsert = args::bool_field(d, name, "upsert")?.unwrap_or(false);
            BulkBody::Modify(Box::new(modify(Some(mods), multi, upsert)?))
        }
        BulkKind::Delete => {
            let multi = args::bool_field(d, name, "multi")?.unwrap_or(false);
            BulkBody::Modify(Box::new(modify(None, multi, false)?))
        }
    };
    let ns = spaces.get(index).cloned().ok_or_else(|| {
        CommandError::new(
            BAD_VALUE,
            format!("BulkWrite ops entry {} has an invalid nsInfo index.", super::display(op)),
        )
    })?;
    Ok((kind, ns, body))
}

fn bulk_op<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    w: &mut Writer,
    (kind, ns, body): &(BulkKind, Namespace, BulkBody),
    bypass: bool,
    stmt: u32,
) -> Result<(BulkKind, MutationResult), CommandError> {
    match body {
        BulkBody::Insert(doc) => {
            names::check_writable(ns)?;
            let coll = ensure_collection(ctx, ns)?;
            let doc = with_id(ctx.backend, ctx.shared, doc)?;
            let key = insert_key(&coll, doc.get("_id").unwrap_or(&Value::Null))?;
            let rules = rules_of(ns, &coll, bypass, None, index_rules(ctx.backend, &coll)?)?;
            let keys = if key.is_empty() { Vec::new() } else { vec![key] };
            let bound = Bound {
                mutation: DocMutation::Insert { docs: vec![doc], ordered: true, rules },
                keys,
            };
            let r = run(ctx.backend, ctx.shared, w, &coll, &bound, stmt)?;
            if r.retried {
                w.note_retried([stmt]);
            }
            match r.errors.into_iter().next() {
                Some(e) => Err(e.error),
                None => {
                    Ok((BulkKind::Insert, MutationResult { n: r.n, ..MutationResult::default() }))
                }
            }
        }
        BulkBody::Modify(m) => {
            let bypass = bypass && *kind == BulkKind::Update;
            Ok((*kind, modify(ctx, w, ns, m, bypass, stmt)?))
        }
    }
}
