//! The server side of Queryable Encryption in the commands (017): `create` with
//! `encryptedFields`, the filters with find payloads of `find`, `count`, `distinct`, `aggregate`,
//! `delete`, `update` and `findAndModify`, and the writes with insert payloads, which keep the
//! document, its `__safeContent__` tags and the ESC and ECOC entries in one transaction (FR-003).
//! A command is only processed when it carries `encryptionInformation`, as a `refnode` 9.0.4 does.

use teseladb_fle::FleError;
use teseladb_fle::esc::EscReader;
use teseladb_fle::fields::{self, EncryptedFieldConfig};
use teseladb_fle::query::Rewriter;
use teseladb_fle::tokens::Token;
use teseladb_fle::write::WriteError;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_bson::{Document, RawDoc, Value};

use super::{Backend, Ctx};
use crate::error::{Code, CommandError};

/// `encryptionInformation`.
pub const ENCRYPTION_INFORMATION: &str = "encryptionInformation";
/// `encryptedFields`.
pub const ENCRYPTED_FIELDS: &str = "encryptedFields";

/// The command error of an error of the encryption crate.
pub fn from_fle(e: FleError) -> CommandError {
    CommandError::new(Code { code: e.code, name: e.name }, e.message)
}

fn from_write(e: WriteError<CommandError>) -> CommandError {
    match e {
        WriteError::Fle(e) => from_fle(e),
        WriteError::Read(e) => e,
    }
}

/// Whether a command carries Queryable Encryption payloads or tokens, which neither the profiler
/// nor the log records (FR-008).
pub fn is_secret(cmd: &Document) -> bool {
    let name = cmd.iter().next().map_or("", |(k, _)| k);
    name == COMPACT || name == CLEANUP || cmd.get(ENCRYPTION_INFORMATION).is_some()
}

/// The encrypted fields of collection `c`, if it has them.
pub fn config_of(c: &CollectionEntry) -> Result<Option<EncryptedFieldConfig>, CommandError> {
    let options = RawDoc::from_bytes(&c.options)?;
    match options.get(ENCRYPTED_FIELDS) {
        Some(teseladb_bson::RawValue::Document(d)) => {
            fields::read(d, ENCRYPTED_FIELDS).map(Some).map_err(from_fle)
        }
        _ => Ok(None),
    }
}

/// The encrypted fields of collection `ns`, if it exists and has them.
pub fn config<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Option<EncryptedFieldConfig>, CommandError> {
    match backend.collection(ns)? {
        Some(c) => config_of(&c),
        None => Ok(None),
    }
}

/// The encryption keywords of `$jsonSchema`.
const ENCRYPT_KEYWORD: &str = "encrypt";
const ENCRYPT_METADATA_KEYWORD: &str = "encryptMetadata";

/// Whether a validator uses the encryption keywords of `$jsonSchema`.
fn has_encrypt_keyword(v: &Value) -> bool {
    match v {
        Value::Document(d) => d.iter().any(|(k, x)| {
            k == ENCRYPT_KEYWORD || k == ENCRYPT_METADATA_KEYWORD || has_encrypt_keyword(x)
        }),
        Value::Array(a) => a.iter().any(has_encrypt_keyword),
        _ => false,
    }
}

/// Checks the `encryptedFields` of `create` on `coll` and replaces it with what the catalog
/// keeps (FR-002), with the conflicts a `refnode` 9.0.4 refuses (measured).
pub fn check_create(coll: &str, options: &mut Document) -> Result<(), CommandError> {
    let Some(given) = options.get(ENCRYPTED_FIELDS) else { return Ok(()) };
    let Value::Document(given) = given else {
        return Err(crate::types::wrong_type(
            &format!("create.{ENCRYPTED_FIELDS}"),
            given,
            "object",
        ));
    };
    let bytes = given.to_bytes()?;
    let config = fields::for_create(coll, RawDoc::from_bytes(&bytes)?).map_err(from_fle)?;
    if options.get("capped").is_some_and(|v| v == &Value::Boolean(true)) {
        return Err(from_fle(teseladb_fle::error::err(
            teseladb_fle::error::CAPPED,
            "Encrypted fields cannot be used with capped collections",
        )));
    }
    if options.get("viewOn").is_some() || options.get("timeseries").is_some() {
        return Err(from_fle(teseladb_fle::error::err(
            teseladb_fle::error::VIEW_OR_TIMESERIES,
            "Encrypted fields cannot be used with views or timeseries collections",
        )));
    }
    if options.get("validator").is_some_and(has_encrypt_keyword) {
        return Err(from_fle(teseladb_fle::error::err(
            teseladb_fle::error::QUERY_FEATURE_NOT_ALLOWED,
            "Encryption-related validator keywords are not allowed in this context",
        )));
    }
    options.insert(ENCRYPTED_FIELDS, config.to_document());
    Ok(())
}

/// Reads the ESC of a collection by `_id`: by the document's key, as a `find` on `_id` reads it,
/// with the ESC's catalog entry read once. An ESC that is a view, has a collation or is no plain
/// collection is read with `find` itself. What the search reads ahead comes in one round of the
/// backend and the probes take it from there (017 SC-002).
pub struct CtxEsc<'c, 'a, B: Backend + ?Sized> {
    ctx: &'c mut Ctx<'a, B>,
    esc: String,
    /// The ESC's entry, once read: `Some(None)` if there is none.
    entry: Option<Option<CollectionEntry>>,
    /// The documents read ahead by `_id` (`None`: no document), until a probe reads them.
    ahead: std::collections::HashMap<[u8; 32], Option<Vec<u8>>>,
}

impl<'c, 'a, B: Backend + ?Sized> CtxEsc<'c, 'a, B> {
    /// A reader of ESC `esc` of the database of `ctx`.
    pub fn new(ctx: &'c mut Ctx<'a, B>, esc: &str) -> CtxEsc<'c, 'a, B> {
        CtxEsc { ctx, esc: esc.to_string(), entry: None, ahead: std::collections::HashMap::new() }
    }

    /// The ESC's entry, read the first time.
    fn entry(&mut self) -> Result<Option<CollectionEntry>, CommandError> {
        if let Some(e) = &self.entry {
            return Ok(e.clone());
        }
        let ns = Namespace { db: self.ctx.db.to_string(), coll: self.esc.clone() };
        let e = self.ctx.backend.collection(&ns)?;
        self.entry = Some(e.clone());
        Ok(e)
    }

    /// The ESC's entry if its documents are read by key: `Ok(None)` without an ESC, `Err(None)`
    /// when only `find` reads them as RefDb does (a view, a collation, a time series or capped
    /// collection, a sharded one).
    fn by_key_entry(&mut self) -> Result<Option<CollectionEntry>, Option<CommandError>> {
        let Some(coll) = self.entry()? else { return Ok(None) };
        let plain = matches!(
            super::kinds::kind_of(&coll)?,
            teseladb_meta::collection_types::CollectionKind::Normal
                | teseladb_meta::collection_types::CollectionKind::Clustered { .. }
        );
        if !plain
            || super::query::options_collation(&coll).is_some()
            || super::capped::rule_of(&coll)?.is_some()
            || super::sharding::is_sharded(&coll)
        {
            return Err(None);
        }
        Ok(Some(coll))
    }

    /// The document with `_id` `id` read by its key; `Err(None)` when only `find` reads it.
    fn by_key(&mut self, id: &Token) -> Result<Option<Document>, Option<CommandError>> {
        let Some(coll) = self.by_key_entry()? else { return Ok(None) };
        // The read goes through the index of `_id` (`$indexStats` counts it), as `find`'s.
        self.ctx.backend.note_plan(super::plan_exec::used_id_index(false));
        let bytes = match self.ahead.remove(&id.0) {
            Some(read) => read,
            None => {
                let key = super::crud::doc_key(&coll, &binary(id.0.to_vec()))?;
                let mut end = key.clone();
                end.push(0);
                let concern = super::args::read_concern(&Document::new(), super::query::FIND)?;
                let rows = self.ctx.backend.scan(&key, &end, 1, concern)?;
                rows.into_iter().next().map(|(_, bytes)| bytes)
            }
        };
        match bytes {
            Some(bytes) => {
                Ok(Some(Document::from(RawDoc::from_bytes(&bytes).map_err(CommandError::from)?)))
            }
            None => Ok(None),
        }
    }

    /// The document with `_id` `id` read with `find`.
    fn by_find(&mut self, id: Value) -> Result<Option<Document>, CommandError> {
        let cmd = Document::new()
            .with("find", self.esc.as_str())
            .with("filter", Document::new().with("_id", id))
            .with("limit", 1)
            .with("singleBatch", true);
        let reply = sub_command(self.ctx, &cmd)?;
        Ok(first_batch(&reply).into_iter().next())
    }
}

thread_local! {
    /// Whether the command running on this thread is one the encryption step sends.
    static IN_SUB_COMMAND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `cmd` on the database of `ctx` as a command of the node, without the encryption step.
pub fn sub_command<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    cmd: &Document,
) -> Result<Document, CommandError> {
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: ctx.db, cmd };
    let outer = IN_SUB_COMMAND.replace(true);
    let reply = super::command(&mut sub);
    IN_SUB_COMMAND.set(outer);
    reply
}

/// The encrypted fields an insert without `encryptionInformation` into `c` must respect (no
/// `__safeContent__`, the implicit schema): `None` if `c` has none, or if the insert is one the
/// encryption step sends with the values it already processed. The insert reads `c` anyway, so
/// a collection without encrypted fields costs nothing more.
pub fn plain_insert_config(
    c: &CollectionEntry,
) -> Result<Option<EncryptedFieldConfig>, CommandError> {
    if IN_SUB_COMMAND.get() {
        return Ok(None);
    }
    config_of(c)
}

/// The documents of the first batch of a cursor reply.
pub fn first_batch(reply: &Document) -> Vec<Document> {
    match reply.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(b)) => b.iter().filter_map(|v| v.as_document().cloned()).collect(),
        _ => Vec::new(),
    }
}

impl<B: Backend + ?Sized> EscReader for CtxEsc<'_, '_, B> {
    type Error = CommandError;

    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, CommandError> {
        let found = match self.by_key(id) {
            Ok(d) => d,
            Err(Some(e)) => return Err(e),
            Err(None) => self.by_find(binary(id.0.to_vec()))?,
        };
        Ok(found.map(|d| match d.get("value") {
            Some(Value::Binary { bytes, .. }) => bytes.clone(),
            _ => Vec::new(),
        }))
    }

    fn read_ahead(&mut self, ids: &[Token]) -> Result<(), CommandError> {
        let coll = match self.by_key_entry() {
            Ok(Some(coll)) => coll,
            Ok(None) | Err(None) => return Ok(()),
            Err(Some(e)) => return Err(e),
        };
        let wanted: Vec<&Token> = ids.iter().filter(|id| !self.ahead.contains_key(&id.0)).collect();
        let keys = wanted
            .iter()
            .map(|id| super::crud::doc_key(&coll, &binary(id.0.to_vec())))
            .collect::<Result<Vec<_>, _>>()?;
        let concern = super::args::read_concern(&Document::new(), super::query::FIND)?;
        let found = self.ctx.backend.points(&keys, concern)?;
        self.ahead.extend(wanted.into_iter().map(|id| id.0).zip(found));
        Ok(())
    }
}

/// A general binary.
pub fn binary(bytes: Vec<u8>) -> Value {
    Value::Binary { subtype: teseladb_bson::types::BINARY_GENERIC, bytes }
}

/// The command with the find payloads of its filters rewritten, without `encryptionInformation`.
pub fn rewrite_command<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
    cmd: &Document,
) -> Result<Document, CommandError> {
    let mut esc = CtxEsc::new(ctx, &config.esc);
    let mut rw = Rewriter::new(config, &mut esc);
    let mut out = Document::new();
    for (k, v) in cmd.iter() {
        let v = match (k, v) {
            (ENCRYPTION_INFORMATION, _) => continue,
            ("filter" | "query", Value::Document(f)) => {
                Value::Document(rw.filter(f).map_err(from_write)?)
            }
            ("pipeline", Value::Array(stages)) => {
                let mut list = Vec::with_capacity(stages.len());
                for s in stages {
                    list.push(match s {
                        Value::Document(stage) => match stage.get("$match") {
                            Some(Value::Document(m)) if stage.len() == 1 => Value::Document(
                                Document::new().with("$match", rw.filter(m).map_err(from_write)?),
                            ),
                            _ => s.clone(),
                        },
                        other => other.clone(),
                    });
                }
                Value::Array(list)
            }
            ("updates" | "deletes", Value::Array(statements)) => {
                let mut list = Vec::with_capacity(statements.len());
                for s in statements {
                    list.push(match s {
                        Value::Document(st) => {
                            let mut st = st.clone();
                            if let Some(Value::Document(q)) = st.get("q") {
                                let q = rw.filter(q).map_err(from_write)?;
                                st.insert("q", q);
                            }
                            Value::Document(st)
                        }
                        other => other.clone(),
                    });
                }
                Value::Array(list)
            }
            _ => v.clone(),
        };
        out.push(k, v);
    }
    Ok(out)
}

/// The fields of `encryptionInformation` (measured on a `refnode` 9.0.4).
const INFORMATION_FIELDS: [&str; 3] = ["type", "schema", "crudProcessed"];

/// Reads `encryptionInformation` with the IDL rules of a `refnode` 9.0.4 (FR-006).
fn check_information(name: &str, v: &Value) -> Result<(), CommandError> {
    let path = format!("{name}.{ENCRYPTION_INFORMATION}");
    let Value::Document(d) = v else { return Err(crate::types::wrong_type(&path, v, "object")) };
    if let Some((k, _)) = d.iter().find(|(k, _)| !INFORMATION_FIELDS.contains(k)) {
        return Err(CommandError::new(
            crate::error::IDL_UNKNOWN_FIELD,
            format!("BSON field '{path}.{k}' is an unknown field."),
        ));
    }
    if d.get("schema").is_none() {
        return Err(CommandError::new(
            crate::error::IDL_FAILED_TO_PARSE,
            format!("BSON field '{path}.schema' is missing but a required field"),
        ));
    }
    Ok(())
}

/// The commands whose filters carry find payloads.
const QUERIES: [&str; 4] = ["find", "count", "distinct", "aggregate"];

/// The namespace a command names in its first field.
fn target_ns<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Option<Namespace> {
    match ctx.first() {
        Value::String(c) => Some(Namespace { db: ctx.db.to_string(), coll: c.clone() }),
        _ => None,
    }
}

/// The encryption step of a command, if it has one: the commands with `encryptionInformation` on
/// a collection with encrypted fields.
pub fn intercept<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Option<Result<Document, CommandError>> {
    // Only a command a driver encrypted has a step here: the rest pay nothing.
    let information = ctx.cmd.get(ENCRYPTION_INFORMATION)?;
    let name = ctx.name().to_string();
    if let Err(e) = check_information(&name, information) {
        return Some(Err(e));
    }
    let is_query = QUERIES.contains(&name.as_str());
    let is_write = WRITES.contains(&name.as_str());
    if !is_query && !is_write {
        return None;
    }
    let ns = target_ns(ctx)?;
    // A catalog the node cannot read now (a command refused before its arguments) leaves the
    // command to its own path, which answers first with its argument errors.
    let config = config(ctx.backend, &ns).ok().flatten()?;
    if is_write {
        return Some(write(ctx, &ns, &config));
    }
    let cmd = ctx.cmd;
    Some(rewrite_command(ctx, &config, cmd).and_then(|rewritten| sub_command(ctx, &rewritten)))
}

/// The write commands of the encryption step.
pub const WRITES: [&str; 5] = ["insert", "update", "delete", "findAndModify", "findandmodify"];

/// A write on a collection with encrypted fields.
fn write<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    super::fle_write::run(ctx, ns, config)
}

/// Times a write with encrypted fields starts again after a write conflict in its transaction.
const CONFLICT_ROUNDS: u32 = 100;
/// The number of the node's transaction on its session.
const INTERNAL_TXN_NUMBER: i64 = 1;

/// Whether `e` asks to run the transaction again.
fn transient(e: &CommandError) -> bool {
    e.code == crate::error::WRITE_CONFLICT || e.labels.contains(&"TransientTransactionError")
}

/// The array of statements of each write command.
fn statements_field(name: &str) -> Option<&'static str> {
    match name {
        "insert" => Some("documents"),
        "update" => Some("updates"),
        "delete" => Some("deletes"),
        _ => None,
    }
}

/// Adds the reply of statement `at` to the reply of the command.
fn merge(total: &mut Document, part: &Document, at: usize) {
    let offset = i32::try_from(at).unwrap_or(i32::MAX);
    for k in ["n", "nModified"] {
        if let Some(Value::Int32(x)) = part.get(k) {
            let sum = match total.get(k) {
                Some(Value::Int32(y)) => x + y,
                _ => *x,
            };
            total.insert(k, sum);
        }
    }
    for k in ["upserted", "writeErrors"] {
        let Some(Value::Array(items)) = part.get(k) else { continue };
        // Each item is a document with the statement's `index` (a command's reply).
        let shifted = items.iter().filter_map(Value::as_document).map(|d| {
            let mut d = d.clone();
            if let Some(Value::Int32(i)) = d.get("index") {
                let i = i + offset;
                d.insert("index", i);
            }
            Value::Document(d)
        });
        match total.get_mut(k) {
            Some(Value::Array(a)) => a.extend(shifted),
            _ => {
                total.insert(k, Value::Array(shifted.collect()));
            }
        }
    }
}

/// Runs a write command on a collection with encrypted fields outside a client transaction, each
/// statement in a transaction of the node (FR-003), as a `refnode` 9.0.4 runs each in an internal
/// transaction: a statement with a write error is not committed. `statement` runs a command inside
/// the transaction. `None` if the command is not one: no write, or a collection without encrypted
/// fields.
pub fn in_internal_txn(
    backend: &mut dyn Backend,
    shared: &super::Shared,
    (name, db, cmd): (&str, &str, &Document),
    mut statement: impl FnMut(
        &mut super::txn::TxnBackend<'_>,
        &Document,
    ) -> Option<Result<Document, CommandError>>,
) -> Option<Result<Document, CommandError>> {
    if !WRITES.contains(&name) {
        return None;
    }
    cmd.get(ENCRYPTION_INFORMATION)?;
    let ns = match cmd.iter().next() {
        Some((_, Value::String(c))) => Namespace { db: db.to_string(), coll: c.clone() },
        _ => return None,
    };
    config(backend, &ns).ok().flatten()?;
    // An empty list gets the command's own error for its batch size.
    let Some((field, Value::Array(items))) = statements_field(name)
        .and_then(|f| cmd.get(f).map(|v| (f, v)))
        .filter(|(_, v)| !matches!(v, Value::Array(a) if a.is_empty()))
    else {
        return Some(one_txn(backend, shared, (name, db, cmd), &mut statement));
    };
    let ordered = !matches!(cmd.get("ordered"), Some(Value::Boolean(false)));
    let mut total = Document::new().with("n", 0);
    // The statements without the list, which each one gets alone.
    let mut rest = cmd.clone();
    rest.remove(field);
    for (i, item) in items.iter().enumerate() {
        // Each statement reads what the previous one committed: its intents go first, as a
        // `refnode` commits a statement's transaction before the next, instead of each read of the
        // next one pushing a transaction that already committed (017 SC-002).
        if i > 0 {
            super::txn::resolve_queued(backend, shared);
        }
        let mut part = rest.clone();
        part.insert(field, vec![item.clone()]);
        let reply = match one_txn(backend, shared, (name, db, &part), &mut statement) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        merge(&mut total, &reply, i);
        if ordered && reply.get("writeErrors").is_some() {
            break;
        }
    }
    total.push("ok", 1.0);
    Some(Ok(total))
}

/// Runs `cmd` in one transaction of the node, again after a write conflict; a reply with write
/// errors is not committed.
fn one_txn(
    backend: &mut dyn Backend,
    shared: &super::Shared,
    (name, db, cmd): (&str, &str, &Document),
    statement: &mut impl FnMut(
        &mut super::txn::TxnBackend<'_>,
        &Document,
    ) -> Option<Result<Document, CommandError>>,
) -> Result<Document, CommandError> {
    let coll = cmd.iter().next().map(|(_, v)| v.clone()).unwrap_or(Value::Null);
    let begin = Document::new().with(name, coll);
    let mut round = 0;
    loop {
        let lsid = u128::from_be_bytes(super::new_uuid(backend));
        let start = super::txn::TxnFields::internal(lsid, INTERNAL_TXN_NUMBER, true);
        // An insert reads the ESC before it writes and never what it wrote: its writes go with
        // the commit, in one phase when one range holds them (017 SC-002).
        let start = match name {
            super::crud::INSERT => start.deferring(),
            _ => start,
        };
        let then = super::txn::TxnFields::internal(lsid, INTERNAL_TXN_NUMBER, false);
        let result = super::txn::run_in_txn(backend, shared, (name, db, &begin), start, |b| {
            statement(b, cmd)
        });
        let commit = matches!(&result, Some(Ok(r)) if r.get("writeErrors").is_none());
        let end = if commit { "commitTransaction" } else { "abortTransaction" };
        let ended = super::txn::run_in_txn(
            backend,
            shared,
            (end, "admin", &Document::new().with(end, 1)),
            then,
            |_| None,
        );
        super::lock(shared).txns.remove(&lsid);
        let outcome = match (result, ended) {
            (Some(Ok(_)), Some(Err(e))) if commit => Err(e),
            (Some(Ok(mut reply)), _) => {
                reply.remove("recoveryToken");
                Ok(reply)
            }
            (Some(Err(mut e)), _) => {
                e.extra.remove("recoveryToken");
                Err(e)
            }
            (None, _) => Ok(Document::new().with("ok", 1.0)),
        };
        match outcome {
            Err(e) if transient(&e) && round < CONFLICT_ROUNDS => {
                round += 1;
                if !backend.txn_pause() {
                    return Err(e);
                }
            }
            other => return other,
        }
    }
}

/// `compactStructuredEncryptionData`.
pub const COMPACT: &str = "compactStructuredEncryptionData";
/// `cleanupStructuredEncryptionData`.
pub const CLEANUP: &str = "cleanupStructuredEncryptionData";
/// The documents of the ECOC one read takes.
const ECOC_BATCH: i32 = 10_000;

/// Reads every ECOC document of `config`.
fn read_ecoc<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
) -> Result<Vec<Document>, CommandError> {
    let cmd = Document::new()
        .with("find", config.ecoc.as_str())
        .with("sort", Document::new().with("_id", 1))
        .with("batchSize", ECOC_BATCH)
        .with("singleBatch", true)
        .with("limit", ECOC_BATCH);
    let mut all = first_batch(&sub_command(ctx, &cmd)?);
    // A log longer than one read: the rest from the last `_id`.
    while all.len().is_multiple_of(usize::try_from(ECOC_BATCH).unwrap_or(1)) && !all.is_empty() {
        let last = all.last().and_then(|d| d.get("_id")).cloned().unwrap_or(Value::Null);
        let more = cmd
            .clone()
            .with("filter", Document::new().with("_id", Document::new().with("$gt", last)));
        let next = first_batch(&sub_command(ctx, &more)?);
        if next.is_empty() {
            break;
        }
        all.extend(next);
    }
    Ok(all)
}

/// `compactStructuredEncryptionData` and `cleanupStructuredEncryptionData` (FR-005): the ESC
/// changes of [`teseladb_fle::compact::run`], then the ECOC entries it read are removed.
pub fn compact<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name = ctx.name().to_string();
    let cleanup = name == CLEANUP;
    let tokens_field = if cleanup { "cleanupTokens" } else { "compactionTokens" };
    let allowed: &[&str] = if cleanup {
        &[CLEANUP, "cleanupTokens", ENCRYPTION_INFORMATION]
    } else {
        &[COMPACT, "compactionTokens", "anchorPaddingFactor", ENCRYPTION_INFORMATION]
    };
    if let Some((k, _)) = ctx.cmd.iter().find(|(k, _)| {
        !allowed.contains(k) && !crate::generic::is_generic(k) && !k.starts_with('$')
    }) {
        return Err(CommandError::new(
            crate::error::IDL_UNKNOWN_FIELD,
            format!("BSON field '{name}.{k}' is an unknown field."),
        ));
    }
    let tokens = match ctx.cmd.get(tokens_field) {
        Some(Value::Document(d)) => {
            let b = d.to_bytes()?;
            teseladb_fle::compact::read_tokens(
                RawDoc::from_bytes(&b)?,
                &format!("{name}.{tokens_field}"),
            )
            .map_err(from_fle)?
        }
        Some(v) => {
            return Err(crate::types::wrong_type(&format!("{name}.{tokens_field}"), v, "object"));
        }
        None => {
            return Err(CommandError::new(
                crate::error::IDL_FAILED_TO_PARSE,
                format!("BSON field '{name}.{tokens_field}' is missing but a required field"),
            ));
        }
    };
    let factor = match ctx.cmd.get("anchorPaddingFactor") {
        None => teseladb_fle::compact::DEFAULT_PADDING_FACTOR,
        Some(Value::Double(f)) => *f,
        Some(v) => {
            return Err(crate::types::wrong_type(
                &format!("{name}.anchorPaddingFactor"),
                v,
                "double",
            ));
        }
    };
    let ns = match ctx.first() {
        Value::String(c) => Namespace { db: ctx.db.to_string(), coll: c.clone() },
        v => return Err(crate::types::wrong_type(&name, v, "string")),
    };
    let Some(config) = config(ctx.backend, &ns)? else {
        return Err(CommandError::new(
            crate::error::BAD_VALUE,
            format!("Target namespace {}.{} is not an encrypted collection", ns.db, ns.coll),
        ));
    };
    let ecoc = read_ecoc(ctx, &config)?;
    let entries: Vec<(String, Vec<u8>)> = ecoc
        .iter()
        .filter_map(|d| match (d.get("fieldName"), d.get("value")) {
            (Some(Value::String(f)), Some(Value::Binary { bytes, .. })) => {
                Some((f.clone(), bytes.clone()))
            }
            _ => None,
        })
        .collect();
    let refs: Vec<teseladb_fle::compact::EcocEntry<'_>> = entries
        .iter()
        .map(|(f, v)| teseladb_fle::compact::EcocEntry { field: f, value: v })
        .collect();
    let mut ivs = Vec::new();
    for _ in 0..(refs.len() * 2 + 1024) {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&ctx.backend.random().to_le_bytes());
        iv[8..].copy_from_slice(&ctx.backend.random().to_le_bytes());
        ivs.push(iv);
    }
    let mut next = ivs.into_iter();
    let mut draw = || next.next().unwrap_or([0u8; 16]);
    let esc_name = config.esc.clone();
    let (ops, stats) = {
        let mut reader = CtxEsc::new(ctx, &esc_name);
        teseladb_fle::compact::run(
            &config,
            &tokens,
            &refs,
            &mut reader,
            (cleanup, factor),
            &mut draw,
        )?
    };
    apply(ctx, &config, ops)?;
    let ids: Vec<Value> = ecoc.iter().filter_map(|d| d.get("_id").cloned()).collect();
    if !ids.is_empty() {
        let del = Document::new().with("delete", config.ecoc.as_str()).with(
            "deletes",
            vec![Value::Document(
                Document::new()
                    .with("q", Document::new().with("_id", Document::new().with("$in", ids)))
                    .with("limit", 0),
            )],
        );
        super::fle_write::checked(sub_command(ctx, &del)?)?;
    }
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let ecoc_stats = Document::new().with("read", count(ecoc.len() as u64)).with("deleted", 0i64);
    let esc_stats = Document::new()
        .with("read", count(stats.read))
        .with("inserted", count(stats.inserted))
        .with("updated", count(stats.updated))
        .with("deleted", count(stats.deleted));
    Ok(Document::new()
        .with("stats", Document::new().with("ecoc", ecoc_stats).with("esc", esc_stats))
        .with("ok", 1.0))
}

/// Makes the ESC changes of a compaction.
fn apply<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
    ops: Vec<teseladb_fle::compact::EscOp>,
) -> Result<(), CommandError> {
    use teseladb_fle::compact::EscOp;
    let (mut inserts, mut deletes, mut updates) = (Vec::new(), Vec::new(), Vec::new());
    for op in ops {
        match op {
            EscOp::Insert(id, value) => {
                // A compaction inserts anchors, which always have a value.
                let d = Document::new().with("_id", binary(id.0.to_vec()));
                let d = value.into_iter().fold(d, |d, v| d.with("value", binary(v)));
                inserts.push(Value::Document(d));
            }
            EscOp::Update(id, value) => updates.push(Value::Document(
                Document::new().with("q", Document::new().with("_id", binary(id.0.to_vec()))).with(
                    "u",
                    Document::new().with("$set", Document::new().with("value", binary(value))),
                ),
            )),
            EscOp::Delete(id) => deletes.push(binary(id.0.to_vec())),
        }
    }
    if !deletes.is_empty() {
        let q = Document::new().with("_id", Document::new().with("$in", deletes));
        let cmd = Document::new()
            .with("delete", config.esc.as_str())
            .with("deletes", vec![Value::Document(Document::new().with("q", q).with("limit", 0))]);
        super::fle_write::checked(sub_command(ctx, &cmd)?)?;
    }
    if !updates.is_empty() {
        let cmd = Document::new().with("update", config.esc.as_str()).with("updates", updates);
        super::fle_write::checked(sub_command(ctx, &cmd)?)?;
    }
    if !inserts.is_empty() {
        let cmd = Document::new().with("insert", config.esc.as_str()).with("documents", inserts);
        super::fle_write::checked(sub_command(ctx, &cmd)?)?;
    }
    Ok(())
}
