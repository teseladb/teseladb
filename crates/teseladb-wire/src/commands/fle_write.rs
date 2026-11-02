//! Writes on a collection with encrypted fields (017 FR-003, FR-006): insert payloads become
//! indexed values with their ESC positions, ECOC entries and `__safeContent__` tags; updates keep
//! `__safeContent__` to the tags of the indexed values the document still has. Behaviour and
//! messages measured on a `refnode` 9.0.4. The caller runs the command inside a transaction: the
//! client's, or one of the node's (`dispatch`).

use teseladb_bson::{Document, Value};
use teseladb_fle::esc::Overlay;
use teseladb_fle::fields::{EncryptedFieldConfig, SAFE_CONTENT};
use teseladb_fle::iev::IndexedValue;
use teseladb_fle::payloads::{InsertUpdatePayload, subtype_of};
use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::Token;
use teseladb_fle::write::{self as fw, Added, EcocEntry};
use teseladb_meta::catalog::Namespace;

use super::crud::write_error;
use super::fle::{CtxEsc, binary, first_batch, from_fle, sub_command};
use super::{Backend, Ctx};
use crate::error::{BAD_VALUE, Code, CommandError};

/// The name a `$set` value takes while its payloads are processed.
const SET_VALUE: &str = "v";
const MULTI_UPDATE: Code = Code { code: 6_371_503, name: "Location6371503" };
const PIPELINE_UPDATE: Code = Code { code: 6_371_517, name: "Location6371517" };
const SAFE_CONTENT_UPDATE: Code = Code { code: 6_666_200, name: "Location6666200" };
const FAM_NEW: Code = Code { code: 6_371_402, name: "Location6371402" };

/// The words a `refnode` 9.0.4 puts before an error of the write it commits in its own transaction.
pub const COMMITTING: &str = "Command error committing internal transaction :: caused by :: ";

/// Runs write command `ctx`, which carries `encryptionInformation`, on `ns`. A write without it
/// takes the plain path: only an insert has checks there (`check_plain`).
pub fn run<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    match ctx.name() {
        "insert" => insert(ctx, ns, config),
        "update" => update(ctx, ns, config),
        "delete" => {
            let cmd = rewritten(ctx, config)?;
            sub_command(ctx, &cmd)
        }
        "findAndModify" | "findandmodify" => find_and_modify(ctx, ns, config),
        _ => {
            let cmd = ctx.cmd.clone();
            sub_command(ctx, &cmd)
        }
    }
}

/// The command with its filters rewritten and without `encryptionInformation`.
fn rewritten<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    let cmd = ctx.cmd.clone();
    super::fle::rewrite_command(ctx, config, &cmd)
}

/// 16 random bytes of the node.
fn iv<B: Backend + ?Sized>(b: &mut B) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&b.random().to_le_bytes());
    out[8..].copy_from_slice(&b.random().to_le_bytes());
    out
}

/// Turns the insert payloads of `doc` into indexed values; the additions to write.
fn transform<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
    doc: &mut Document,
) -> Result<(Added, Vec<Token>), CommandError> {
    let mut ivs: Vec<[u8; 16]> = Vec::new();
    // The IVs are drawn up front: the ESC reads borrow the node meanwhile.
    for _ in 0..iv_budget(config, doc) {
        ivs.push(iv(ctx.backend));
    }
    let mut next = ivs.into_iter();
    let mut draw = move || next.next().unwrap_or([0u8; 16]);
    let mut reader = CtxEsc::new(ctx, &config.esc);
    let mut overlay = Overlay::new(&mut reader);
    let added = fw::transform(config, doc, &mut overlay, &mut draw).map_err(|e| match e {
        fw::WriteError::Fle(e) => from_fle(e),
        fw::WriteError::Read(e) => e,
    })?;
    Ok((added, overlay.into_written()))
}

/// The IVs a document's payloads need: one per value and two per tag.
fn iv_budget(config: &EncryptedFieldConfig, doc: &Document) -> usize {
    config
        .fields
        .iter()
        .filter_map(|f| fw::get_path(doc, &f.path))
        .filter_map(|v| match subtype_of(v) {
            Some((BlobSubtype::Fle2InsertUpdate, b)) => InsertUpdatePayload::parse(b).ok(),
            _ => None,
        })
        .map(|p| 1 + 2 * p.all_sets().len())
        .sum()
}

/// Writes the ESC positions and ECOC entries of a write.
fn write_metadata<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
    positions: &[Token],
    ecoc: &[EcocEntry],
) -> Result<(), CommandError> {
    if !positions.is_empty() {
        let docs: Vec<Value> = positions.iter().map(|p| Value::Document(fw::esc_doc(p))).collect();
        let cmd = Document::new().with("insert", config.esc.as_str()).with("documents", docs);
        checked(sub_command(ctx, &cmd)?)?;
    }
    if !ecoc.is_empty() {
        let mut docs = Vec::with_capacity(ecoc.len());
        for e in ecoc {
            let id = super::new_object_id(ctx.backend, ctx.shared);
            docs.push(Value::Document(fw::ecoc_doc(id, e)));
        }
        let cmd = Document::new().with("insert", config.ecoc.as_str()).with("documents", docs);
        checked(sub_command(ctx, &cmd)?)?;
    }
    Ok(())
}

/// The first write error of a reply as an error.
pub(super) fn checked(reply: Document) -> Result<Document, CommandError> {
    let first = match reply.get("writeErrors") {
        Some(Value::Array(errors)) => errors.first().and_then(Value::as_document),
        _ => None,
    };
    match first {
        Some(e) => Err(error_of(e)),
        None => Ok(reply),
    }
}

/// A write error document as an error.
fn error_of(e: &Document) -> CommandError {
    let code = match e.get("code") {
        Some(Value::Int32(n)) => *n,
        _ => 1,
    };
    let name = crate::error::ALL.iter().find(|c| c.code == code).map_or("UnknownError", |c| c.name);
    let msg = match e.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut err = CommandError::new(Code { code, name }, msg);
    for (k, v) in e.iter().filter(|(k, _)| !matches!(*k, "index" | "code" | "errmsg")) {
        err.extra.push(k, v.clone());
    }
    err
}

/// The checks of a document an insert without `encryptionInformation` brings to a collection
/// with encrypted fields: no `__safeContent__`, and the implicit schema.
pub fn check_plain(config: &EncryptedFieldConfig, doc: &Document) -> Result<(), CommandError> {
    if doc.get(SAFE_CONTENT).is_some() {
        return Err(safe_content_error());
    }
    validate(config, doc)
}

fn safe_content_error() -> CommandError {
    CommandError::new(BAD_VALUE, format!("Cannot insert a document with field name {SAFE_CONTENT}"))
}

/// The base of a reply to a sub-command, without its count.
fn copy_field(from: &Document, to: &mut Document, k: &str) {
    if let Some(v) = from.get(k) {
        to.push(k, v.clone());
    }
}

fn insert<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    // No list, or an empty one, gets the command's own errors.
    let docs = match ctx.cmd.get("documents") {
        Some(Value::Array(d)) if !d.is_empty() => d.clone(),
        _ => {
            let cmd = ctx.cmd.clone();
            return sub_command(ctx, &cmd);
        }
    };
    let ordered = !matches!(ctx.cmd.get("ordered"), Some(Value::Boolean(false)));
    let mut n = 0i32;
    let mut errors = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let Value::Document(d) = d else {
            let cmd = ctx.cmd.clone();
            return sub_command(ctx, &cmd);
        };
        match insert_one(ctx, ns, config, d) {
            Ok(()) => n += 1,
            Err(Failure::Command(e)) => return Err(e),
            Err(Failure::Write(e)) => {
                errors.push(write_error(i, &e));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = Document::new().with("n", n);
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

/// How a statement fails: the whole command, or this statement with a write error.
enum Failure {
    Command(CommandError),
    Write(CommandError),
}

impl From<CommandError> for Failure {
    fn from(e: CommandError) -> Failure {
        Failure::Command(e)
    }
}

fn insert_one<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
    doc: &Document,
) -> Result<(), Failure> {
    if doc.get(SAFE_CONTENT).is_some() {
        return Err(Failure::Write(safe_content_error()));
    }
    let mut doc = doc.clone();
    let (added, positions) = transform(ctx, config, &mut doc)?;
    if !added.tags.is_empty() {
        fw::add_tags(&mut doc, &added.tags);
    }
    validate(config, &doc).map_err(|e| Failure::Write(committing(e)))?;
    write_metadata(ctx, config, &positions, &added.ecoc)?;
    let mut cmd = Document::new()
        .with("insert", ns.coll.as_str())
        .with("documents", vec![Value::Document(doc)]);
    copy_field(ctx.cmd, &mut cmd, "bypassDocumentValidation");
    let reply = sub_command(ctx, &cmd)?;
    checked(reply).map(|_| ()).map_err(|e| Failure::Write(committing(e)))
}

/// `e` as the error of a write the node committed in its own transaction.
fn committing(mut e: CommandError) -> CommandError {
    e.errmsg = format!("{COMMITTING}{}", e.errmsg);
    e
}

/// The implicit schema of the encrypted fields: each one present holds a stored value of its
/// kind and type (FR-006, measured on a `refnode` 9.0.4).
pub fn validate(config: &EncryptedFieldConfig, doc: &Document) -> Result<(), CommandError> {
    match teseladb_fle::schema::implicit_failure(config, doc) {
        None => Ok(()),
        Some(details) => {
            let mut e = CommandError::new(
                crate::error::DOCUMENT_VALIDATION_FAILURE,
                "Document failed validation",
            );
            let mut info = Document::new();
            if let Some(id) = doc.get("_id") {
                info.push("failingDocumentId", id.clone());
            }
            info.push("details", details);
            e.extra.push("errInfo", info);
            Err(e)
        }
    }
}

/// The tags of the indexed values `doc` holds at the encrypted fields.
fn live_tags(config: &EncryptedFieldConfig, doc: &Document) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for f in &config.fields {
        if let Some((s, b)) = fw::get_path(doc, &f.path).and_then(subtype_of)
            && s.is_stored_fle2()
            && let Ok(iev) = IndexedValue::parse(b)
        {
            out.extend(iev.tags().map(|t| t.0.to_vec()));
        }
    }
    out
}

/// Whether an update document names `__safeContent__`.
fn touches_safe_content(u: &Document) -> bool {
    u.iter().any(|(op, arg)| {
        op.starts_with('$')
            && matches!(arg, Value::Document(d) if d.iter().any(|(k, _)| k == SAFE_CONTENT || k.starts_with("__safeContent__.")))
    })
}

/// Turns the payloads of an update document into indexed values. Returns the update to run.
fn transform_update<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    config: &EncryptedFieldConfig,
    u: &Document,
) -> Result<(Document, Added, Vec<Token>), CommandError> {
    let replacement = u.iter().next().is_none_or(|(k, _)| !k.starts_with('$'));
    if replacement {
        let mut doc = u.clone();
        let (added, positions) = transform(ctx, config, &mut doc)?;
        doc.remove(SAFE_CONTENT);
        if !added.tags.is_empty() {
            fw::add_tags(&mut doc, &added.tags);
        }
        return Ok((doc, added, positions));
    }
    let mut out = Document::new();
    let mut added = Added::default();
    let mut positions = Vec::new();
    for (op, arg) in u.iter() {
        let Value::Document(fields) = arg else {
            out.push(op, arg.clone());
            continue;
        };
        if op != "$set" && op != "$setOnInsert" {
            out.push(op, arg.clone());
            continue;
        }
        // Each `$set` path on its own, renamed so that the payloads sit at their fields' paths.
        let mut set = Document::new();
        for (k, v) in fields.iter() {
            let mut nested = config.clone();
            nested.fields.retain_mut(|f| {
                if f.path == k {
                    f.path = SET_VALUE.to_string();
                    true
                } else if let Some(rest) = f.path.strip_prefix(k).and_then(|r| r.strip_prefix('.'))
                {
                    f.path = format!("{SET_VALUE}.{rest}");
                    true
                } else {
                    false
                }
            });
            let mut doc = Document::new().with(SET_VALUE, v.clone());
            let (a, p) = transform(ctx, &nested, &mut doc)?;
            added.tags.extend(a.tags);
            added.ecoc.extend(a.ecoc);
            positions.extend(p);
            set.push(k, doc.remove(SET_VALUE).unwrap_or(Value::Null));
        }
        out.push(op, set);
    }
    if !added.tags.is_empty() {
        let tags: Vec<Value> = added.tags.iter().map(|t| binary(t.0.to_vec())).collect();
        out.push("$push", Document::new().with(SAFE_CONTENT, Document::new().with("$each", tags)));
    }
    Ok((out, added, positions))
}

/// Removes from `__safeContent__` of the document with `_id` `id` the tags its indexed values no
/// longer have.
fn prune_tags<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
    id: &Value,
) -> Result<(), CommandError> {
    let find = Document::new()
        .with("find", ns.coll.as_str())
        .with("filter", Document::new().with("_id", id.clone()))
        .with("limit", 1)
        .with("singleBatch", true);
    let Some(doc) = first_batch(&sub_command(ctx, &find)?).into_iter().next() else {
        return Ok(());
    };
    let live = live_tags(config, &doc);
    let stale: Vec<Value> = match doc.get(SAFE_CONTENT) {
        Some(Value::Array(tags)) => tags
            .iter()
            .filter(|t| matches!(t, Value::Binary { bytes, .. } if !live.contains(bytes)))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    if stale.is_empty() {
        return Ok(());
    }
    let cmd = Document::new().with("update", ns.coll.as_str()).with(
        "updates",
        vec![Value::Document(
            Document::new().with("q", Document::new().with("_id", id.clone())).with(
                "u",
                Document::new().with(
                    "$pull",
                    Document::new().with(SAFE_CONTENT, Document::new().with("$in", stale)),
                ),
            ),
        )],
    );
    checked(sub_command(ctx, &cmd)?).map(|_| ())
}

/// The fields of an update statement a `findAndModify` takes.
const STATEMENT_TO_FAM: [(&str, &str); 6] = [
    ("q", "query"),
    ("upsert", "upsert"),
    ("collation", "collation"),
    ("arrayFilters", "arrayFilters"),
    ("hint", "hint"),
    ("sort", "sort"),
];

fn update<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    let cmd = rewritten(ctx, config)?;
    let statements = match cmd.get("updates") {
        Some(Value::Array(s)) if !s.is_empty() => s.clone(),
        _ => return sub_command(ctx, &cmd),
    };
    // A statement that is no document, or has no document in `q` or no document or pipeline in
    // `u`, gets the command's own argument errors.
    let mut parsed: Vec<(&Document, &Value)> = Vec::new();
    for st in &statements {
        match st {
            Value::Document(d) => match (d.get("q"), d.get("u")) {
                (Some(Value::Document(_)), Some(u @ (Value::Document(_) | Value::Array(_)))) => {
                    parsed.push((d, u));
                }
                _ => return sub_command(ctx, &cmd),
            },
            _ => return sub_command(ctx, &cmd),
        }
    }
    let mut checked: Vec<(&Document, &Document)> = Vec::new();
    for (st, u) in parsed {
        if matches!(st.get("multi"), Some(Value::Boolean(true))) {
            return Err(CommandError::new(
                MULTI_UPDATE,
                "FLE only supports single document updates",
            ));
        }
        match u {
            Value::Document(u) => checked.push((st, u)),
            _ => {
                return Err(CommandError::new(
                    PIPELINE_UPDATE,
                    "FLE only supports modifier and replacement style updates",
                ));
            }
        }
    }
    let ordered = !matches!(cmd.get("ordered"), Some(Value::Boolean(false)));
    let (mut n, mut modified) = (0i32, 0i32);
    let (mut upserted, mut errors) = (Vec::new(), Vec::new());
    for (i, (s, u)) in checked.into_iter().enumerate() {
        match update_one(ctx, ns, config, s, u) {
            Ok(r) => {
                n += r.n;
                modified += r.modified;
                if let Some(id) = r.upserted {
                    upserted.push(Value::Document(
                        Document::new()
                            .with("index", i32::try_from(i).unwrap_or(i32::MAX))
                            .with("_id", id),
                    ));
                }
            }
            Err(Failure::Command(e)) => return Err(e),
            Err(Failure::Write(e)) => {
                errors.push(write_error(i, &e));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = Document::new().with("n", n);
    if !upserted.is_empty() {
        reply.push("upserted", upserted);
    }
    reply.push("nModified", modified);
    if !errors.is_empty() {
        reply.push("writeErrors", errors);
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

struct Updated {
    n: i32,
    modified: i32,
    upserted: Option<Value>,
}

fn update_one<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
    s: &Document,
    u: &Document,
) -> Result<Updated, Failure> {
    if touches_safe_content(u) {
        return Err(Failure::Write(CommandError::new(
            SAFE_CONTENT_UPDATE,
            "Cannot modify __safeContent__ field in document.",
        )));
    }
    let (u, added, positions) = transform_update(ctx, config, u)?;
    write_metadata(ctx, config, &positions, &added.ecoc)?;
    let mut fam = Document::new().with("findAndModify", ns.coll.as_str());
    for (from, to) in STATEMENT_TO_FAM {
        if let Some(v) = s.get(from) {
            fam.push(to, v.clone());
        }
    }
    fam.push("update", u);
    fam.push("new", true);
    let reply = sub_command(ctx, &fam).map_err(Failure::Write)?;
    let last = match reply.get("lastErrorObject") {
        Some(Value::Document(d)) => d.clone(),
        _ => Document::new(),
    };
    let n = match last.get("n") {
        Some(Value::Int32(n)) => *n,
        _ => 0,
    };
    let upserted = last.get("upserted").cloned();
    let existing = matches!(last.get("updatedExisting"), Some(Value::Boolean(true)));
    if let Some(Value::Document(post)) = reply.get("value") {
        validate(config, post).map_err(|e| Failure::Write(committing(e)))?;
        if let Some(id) = post.get("_id") {
            prune_tags(ctx, ns, config, id)?;
        }
    }
    Ok(Updated { n, modified: i32::from(existing), upserted })
}

fn find_and_modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    config: &EncryptedFieldConfig,
) -> Result<Document, CommandError> {
    if matches!(ctx.cmd.get("new"), Some(Value::Boolean(true))) {
        return Err(CommandError::new(
            FAM_NEW,
            "findAndModify with encryption only supports new: false",
        ));
    }
    let mut cmd = rewritten(ctx, config)?;
    let Some(Value::Document(u)) = cmd.get("update").cloned() else {
        return sub_command(ctx, &cmd);
    };
    if touches_safe_content(&u) {
        return Err(CommandError::new(
            SAFE_CONTENT_UPDATE,
            "Cannot modify __safeContent__ field in document.",
        ));
    }
    let (u, added, positions) = transform_update(ctx, config, &u)?;
    write_metadata(ctx, config, &positions, &added.ecoc)?;
    cmd.insert("update", u);
    let reply = sub_command(ctx, &cmd)?;
    let id = match (reply.get("value"), reply.get("lastErrorObject")) {
        (Some(Value::Document(pre)), _) => pre.get("_id").cloned(),
        (_, Some(Value::Document(last))) => last.get("upserted").cloned(),
        _ => None,
    };
    if let Some(id) = id {
        prune_tags(ctx, ns, config, &id)?;
    }
    Ok(reply)
}
