//! The server side of Queryable Encryption in the commands (017): `create` with
//! `encryptedFields`, the filters with find payloads of `find`, `count`, `distinct`, `aggregate`,
//! `delete`, `update` and `findAndModify`, and the writes with insert payloads, which keep the
//! document, its `__safeContent__` tags and the ESC and ECOC entries in one transaction (FR-003).
//! A command is only processed when it carries `encryptionInformation`, as a `refnode` 9.0.4 does.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_fle::FleError;
use teseladb_fle::esc::EscReader;
use teseladb_fle::fields::{self, EncryptedFieldConfig};
use teseladb_fle::query::Rewriter;
use teseladb_fle::tokens::Token;
use teseladb_fle::write::WriteError;
use teseladb_meta::catalog::{CollectionEntry, Namespace};

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

/// Reads the ESC of a collection with `find` on `_id`.
pub struct CtxEsc<'c, 'a, B: Backend + ?Sized> {
    ctx: &'c mut Ctx<'a, B>,
    esc: String,
}

impl<'c, 'a, B: Backend + ?Sized> CtxEsc<'c, 'a, B> {
    /// A reader of ESC `esc` of the database of `ctx`.
    pub fn new(ctx: &'c mut Ctx<'a, B>, esc: &str) -> CtxEsc<'c, 'a, B> {
        CtxEsc { ctx, esc: esc.to_string() }
    }
}

/// Runs `cmd` on the database of `ctx` as a command of the node, without the encryption step.
pub fn sub_command<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    cmd: &Document,
) -> Result<Document, CommandError> {
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: ctx.db, cmd };
    super::command(&mut sub)
}

/// The documents of the first batch of a cursor reply.
pub fn first_batch(reply: &Document) -> Vec<Document> {
    let Some(Value::Document(c)) = reply.get("cursor") else { return Vec::new() };
    let Some(Value::Array(b)) = c.get("firstBatch") else { return Vec::new() };
    b.iter().filter_map(|v| v.as_document().cloned()).collect()
}

impl<B: Backend + ?Sized> EscReader for CtxEsc<'_, '_, B> {
    type Error = CommandError;

    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, CommandError> {
        let cmd = Document::new()
            .with("find", self.esc.as_str())
            .with("filter", Document::new().with("_id", binary(id.0.to_vec())))
            .with("limit", 1)
            .with("singleBatch", true);
        let reply = sub_command(self.ctx, &cmd)?;
        Ok(first_batch(&reply).first().map(|d| match d.get("value") {
            Some(Value::Binary { bytes, .. }) => bytes.clone(),
            _ => Vec::new(),
        }))
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
    let name = ctx.name().to_string();
    let is_query = QUERIES.contains(&name.as_str());
    let is_write = WRITES.contains(&name.as_str());
    if !is_query && !is_write {
        return None;
    }
    if is_query {
        ctx.cmd.get(ENCRYPTION_INFORMATION)?;
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
        let shifted = items.iter().map(|v| match v {
            Value::Document(d) => {
                let mut d = d.clone();
                if let Some(Value::Int32(i)) = d.get("index") {
                    let i = i + offset;
                    d.insert("index", i);
                }
                Value::Document(d)
            }
            other => other.clone(),
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
    let ns = match cmd.iter().next() {
        Some((_, Value::String(c))) => Namespace { db: db.to_string(), coll: c.clone() },
        _ => return None,
    };
    config(backend, &ns).ok().flatten()?;
    let Some((field, Value::Array(items))) =
        statements_field(name).and_then(|f| cmd.get(f).map(|v| (f, v)))
    else {
        return Some(one_txn(backend, shared, (name, db, cmd), &mut statement));
    };
    let ordered = !matches!(cmd.get("ordered"), Some(Value::Boolean(false)));
    let mut total = Document::new().with("n", 0);
    for (i, item) in items.iter().enumerate() {
        let mut part = cmd.clone();
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
    if name == "update" && total.get("nModified").is_none() {
        total.insert("nModified", 0);
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
const ECOC_BATCH: i32 = 100_000;

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
        sub_command(ctx, &del)?;
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
                let mut d = Document::new().with("_id", binary(id.0.to_vec()));
                if let Some(v) = value {
                    d.push("value", binary(v));
                }
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
        sub_command(ctx, &cmd)?;
    }
    if !updates.is_empty() {
        let cmd = Document::new().with("update", config.esc.as_str()).with("updates", updates);
        sub_command(ctx, &cmd)?;
    }
    if !inserts.is_empty() {
        let cmd = Document::new().with("insert", config.esc.as_str()).with("documents", inserts);
        sub_command(ctx, &cmd)?;
    }
    Ok(())
}
