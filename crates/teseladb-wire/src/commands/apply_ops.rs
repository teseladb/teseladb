//! `applyOps` (018 FR-011): applies entries of `local.oplog.rs` one by one, as RefDb 9.0.4
//! does (measured on 02-11-2026 on a replica set of `ref:9.0`):
//!
//! - `i` writes the document whole (an `_id` already there is replaced);
//! - `u` applies `{$v: 2, diff}`, an update by operators or a replacement to the document `o2`
//!   names; a document that is not there fails that entry;
//! - `d` deletes the document `o` names, also when it is not there;
//! - `c` runs the command `o` on the database of `ns`; `n` is skipped and not counted;
//! - every entry runs even after one fails; the reply gives `applied` and `results`, and with a
//!   failure `ok: 0` and `UnknownError` «applyOps had one or more errors applying ops»;
//! - an insert or update into a namespace that does not exist fails the command with 26;
//!   `preCondition` is refused (`Location6711600`) and an unknown `op` is `BadValue`.

use teseladb_bson::{Document, Value};
use teseladb_topology::oplog::{OpType, OplogEntry, OplogError};

use super::{Backend, Ctx};
use crate::error::{BAD_VALUE, Code, CommandError, NAMESPACE_NOT_FOUND, TYPE_MISMATCH};

/// `applyOps`.
pub const APPLY_OPS: &str = "applyOps";
/// `UnknownError`, of an entry that did not apply.
const UNKNOWN_ERROR: Code = Code { code: 8, name: "UnknownError" };
/// The refusal of `preCondition`.
const NO_PRECONDITION: Code = Code { code: 6_711_600, name: "Location6711600" };

/// `NoSuchKey`: an update entry without `o2`.
const NO_SUCH_KEY: Code = Code { code: 4, name: "NoSuchKey" };
/// The commands a `c` entry may carry (RefDb 9.0.4 refuses any other with 2, measured on
/// 03-11-2026, `ap.js`).
const ENTRY_COMMANDS: [&str; 18] = [
    "create",
    "renameCollection",
    "drop",
    "collMod",
    "applyOps",
    "dbCheck",
    "dropDatabase",
    "emptycapped",
    "createIndexes",
    "startIndexBuild",
    "commitIndexBuild",
    "abortIndexBuild",
    "dropIndexes",
    "deleteIndexes",
    "commitTransaction",
    "abortTransaction",
    "importCollection",
    "createDatabaseMetadata",
];

/// Entry `i` of the command (measured on 9.0.4: one that is not a document is 14).
fn parse((i, v): (usize, &Value)) -> Result<OplogEntry, CommandError> {
    let Value::Document(d) = v else {
        let t = crate::types::type_name(v);
        let msg = format!("Wrong type for \"{i}\". Expected a object, got a {t}.");
        return Err(CommandError::new(TYPE_MISMATCH, msg));
    };
    OplogEntry::from_document(d).map_err(|e| match e {
        OplogError::UnknownOp(op) => CommandError::new(
            BAD_VALUE,
            format!("Enumeration value '{op}' for field 'OplogEntryBase.op' is not a valid value."),
        ),
        OplogError::Missing(f) => CommandError::new(
            crate::error::IDL_FAILED_TO_PARSE,
            format!("BSON field 'OplogEntryBase.{f}' is missing but a required field"),
        ),
        OplogError::WrongType(f) => CommandError::new(
            TYPE_MISMATCH,
            format!("BSON field 'OplogEntryBase.{f}' is the wrong type"),
        ),
    })
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0)) && r.get("writeErrors").is_none()
}

/// Runs `cmd` on database `db` as a command of the node.
fn run_on<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    db: &str,
    cmd: &Document,
) -> Result<Document, CommandError> {
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db, cmd };
    super::command(&mut sub)
}

/// The document `filter` selects in `db.coll`, if any.
fn find_one<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    db: &str,
    coll: &str,
    filter: &Document,
) -> Result<Option<Document>, CommandError> {
    let find = Document::new()
        .with("find", coll)
        .with("filter", filter.clone())
        .with("limit", 1)
        .with("singleBatch", true);
    let r = run_on(ctx, db, &find)?;
    Ok(super::fle::first_batch(&r).into_iter().next())
}

fn update_one(q: Document, u: Document, upsert: bool) -> Document {
    Document::new().with("q", q).with("u", u).with("upsert", upsert)
}

/// Applies entry `e`; `Ok(false)` when it did not apply.
fn apply_one<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    e: &OplogEntry,
) -> Result<bool, CommandError> {
    let op = &e.operation;
    let (db, coll) = op.ns.split_once('.').unwrap_or((op.ns.as_str(), ""));
    if op.op == OpType::Command {
        let name = op.o.iter().next().map_or("", |(k, _)| k);
        if !ENTRY_COMMANDS.contains(&name) {
            let msg = format!(
                "Enumeration value '{name}' for field 'commandString' is not a valid value."
            );
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        return Ok(ok(&run_on(ctx, db, &op.o)?));
    }
    let ns = super::names::ns(db, coll);
    if op.op != OpType::Delete && ctx.backend.collection(&ns)?.is_none() {
        return Err(CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!(
                "cannot apply insert or update operation on a non-existent namespace {}: {}",
                op.ns,
                super::display(&Value::Document(op.to_document()))
            ),
        ));
    }
    let id = || Document::new().with("_id", op.o.get("_id").cloned().unwrap_or(Value::Null));
    let write = match op.op {
        OpType::Insert => {
            let q = op.o2.clone().unwrap_or_else(id);
            Document::new()
                .with("update", coll)
                .with("updates", vec![Value::Document(update_one(q, op.o.clone(), true))])
        }
        OpType::Delete => {
            let one = Document::new().with("q", op.o.clone()).with("limit", 1);
            Document::new().with("delete", coll).with("deletes", vec![Value::Document(one)])
        }
        _ => {
            let Some(q) = op.o2.clone() else {
                // As RefDb 9.0.4 shows the entry: with its null `ts` and `wall`.
                let shown = op
                    .to_document()
                    .with("ts", Value::Timestamp { time: 0, increment: 0 })
                    .with("wall", Value::DateTime(0));
                let msg = format!(
                    "Failed to apply update due to missing _id: {{ oplogEntry: {} }}",
                    super::display(&Value::Document(shown))
                );
                return Err(CommandError::new(NO_SUCH_KEY, msg));
            };
            let u = match (op.o.get("$v"), op.o.get("diff")) {
                (Some(_), Some(Value::Document(diff))) => {
                    let Some(current) = find_one(ctx, db, coll, &q)? else { return Ok(false) };
                    match teseladb_topology::diff::apply(&current, diff) {
                        Ok(d) => d,
                        Err(_) => return Ok(false),
                    }
                }
                _ => op.o.clone(),
            };
            if find_one(ctx, db, coll, &q)?.is_none() {
                return Ok(false);
            }
            Document::new()
                .with("update", coll)
                .with("updates", vec![Value::Document(update_one(q, u, false))])
        }
    };
    Ok(ok(&run_on(ctx, db, &write)?))
}

/// `applyOps`.
pub fn apply_ops<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let entries = match ctx.first() {
        Value::Array(a) => a.clone(),
        other => {
            let msg = format!(
                "Wrong type for \"applyOps\". Expected a array, got a {}.",
                crate::types::type_name(other)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
    };
    if ctx.cmd.get("preCondition").is_some() {
        let msg = format!(
            "Failed to parse applyOps command: {} :: caused by :: applyOps command no longer supports the 'preCondition' option",
            super::display(&Value::Document(ctx.cmd.clone()))
        );
        return Err(CommandError::new(NO_PRECONDITION, msg));
    }
    let parsed = entries.iter().enumerate().map(parse).collect::<Result<Vec<_>, _>>()?;
    let mut results = Vec::new();
    for e in parsed.iter().filter(|e| e.operation.op != OpType::Noop) {
        results.push(Value::Boolean(apply_one(ctx, e)?));
    }
    let applied = i32::try_from(results.len()).unwrap_or(i32::MAX);
    let failed = results.contains(&Value::Boolean(false));
    let d = Document::new().with("applied", applied).with("results", results);
    Ok(match failed {
        false => d.with("ok", 1.0),
        true => d
            .with("ok", 0.0)
            .with("errmsg", "applyOps had one or more errors applying ops")
            .with("code", UNKNOWN_ERROR.code)
            .with("codeName", UNKNOWN_ERROR.name),
    })
}
