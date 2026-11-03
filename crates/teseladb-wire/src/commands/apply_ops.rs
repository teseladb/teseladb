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
//! - an insert or update into a namespace that does not exist fails the command with 26, and an
//!   unknown `op` with `BadValue`: the entries before it stay applied and the reply gives
//!   `applied`, the error and `results` up to it (measured on 03-11-2026); `preCondition` is
//!   refused (`Location6711600`).

use teseladb_bson::{Document, Value};
use teseladb_topology::diff::DiffError;
use teseladb_topology::oplog::{OpType, Operation};

use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, Code, CommandError, IDL_FAILED_TO_PARSE, ILLEGAL_OPERATION, INVALID_UUID,
    NAMESPACE_NOT_FOUND, TYPE_MISMATCH,
};

/// `applyOps`.
pub const APPLY_OPS: &str = "applyOps";
/// `UnknownError`, of an entry that did not apply.
const UNKNOWN_ERROR: Code = Code { code: 8, name: "UnknownError" };
/// The refusal of `preCondition`.
const NO_PRECONDITION: Code = Code { code: 6_711_600, name: "Location6711600" };

/// `NoSuchKey`: an update entry without `o2`, or an entry without `op`.
const NO_SUCH_KEY: Code = Code { code: 4, name: "NoSuchKey" };
/// A `stmtId` that is not an `int`.
const INVALID_STMT_ID: Code = Code { code: 8_109_800, name: "Location8109800" };
/// A `prevOpTime` that is not a document.
const BOUND_NOT_OBJECT: Code = Code { code: 10_065, name: "Location10065" };
/// A key of a diff that is no section.
const UNEXPECTED_SECTION: Code = Code { code: 4_770_503, name: "Location4770503" };
/// A section of a diff that is not a document.
const SECTION_NOT_OBJECT: Code = Code { code: 4_770_507, name: "Location4770507" };
/// How the error of an update starts.
const UPDATE_ERROR: &str = "Plan executor error during update :: caused by :: ";
/// The subtype and length of a UUID.
const UUID_SUBTYPE: u8 = 4;
const UUID_LEN: usize = 16;
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

/// Entry `i`, checked before any entry applies, as RefDb 9.0.4 does: a document whose `op` and
/// `ns` are strings, and a `c` entry's command one that an oplog carries.
fn precheck((i, v): (usize, &Value)) -> Result<&Document, CommandError> {
    let type_name = crate::types::type_name;
    let Value::Document(d) = v else {
        let msg = format!("Wrong type for \"{i}\". Expected a object, got a {}.", type_name(v));
        return Err(CommandError::new(TYPE_MISMATCH, msg));
    };
    match d.get("op") {
        None => {
            let msg = "Wrong type for \"\". Expected a string, got a missing.";
            return Err(CommandError::new(NO_SUCH_KEY, msg));
        }
        Some(Value::String(_)) => {}
        Some(o) => {
            let msg = format!("Wrong type for \"op\". Expected a string, got a {}.", type_name(o));
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
    }
    match d.get("ns") {
        None => {
            let msg = format!("op does not contain required \"ns\" field: {i}");
            return Err(CommandError::new(ILLEGAL_OPERATION, msg));
        }
        Some(Value::String(_)) => {}
        Some(_) => {
            let msg = format!("\"ns\" field is not a string: {i}");
            return Err(CommandError::new(ILLEGAL_OPERATION, msg));
        }
    }
    let name = match (d.get("op"), d.get("o")) {
        (Some(Value::String(op)), Some(Value::Document(o))) if op == "c" => {
            o.iter().next().map_or("", |(k, _)| k)
        }
        _ => return Ok(d),
    };
    if !ENTRY_COMMANDS.contains(&name) {
        let msg =
            format!("Enumeration value '{name}' for field 'commandString' is not a valid value.");
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    Ok(d)
}

/// Field `field` of an entry with the wrong type, as the parser of RefDb 9.0.4 says it.
fn wrong_type(field: &str, v: &Value, expected: &str) -> CommandError {
    let t = crate::types::type_name(v);
    let msg =
        format!("BSON field 'OplogEntryBase.{field}' is the wrong type '{t}', expected {expected}");
    CommandError::new(TYPE_MISMATCH, msg)
}

/// Field `field` of an entry, checked as RefDb 9.0.4 reads it when the entry's turn comes
/// (measured on 03-11-2026, `ap9.js` and `ap10.js`).
fn check_field(field: &str, v: &Value) -> Result<(), CommandError> {
    const NUMBER: &str = "types '[double, long, int, decimal]'";
    let missing = |path: &str| {
        let msg = format!("BSON field '{path}' is missing but a required field");
        CommandError::new(IDL_FAILED_TO_PARSE, msg)
    };
    let number =
        matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_));
    match (field, v) {
        ("op", Value::String(op)) if OpType::from_code(op).is_none() => Err(CommandError::new(
            BAD_VALUE,
            format!("Enumeration value '{op}' for field 'OplogEntryBase.op' is not a valid value."),
        )),
        ("o" | "o2", v) if !matches!(v, Value::Document(_)) => {
            Err(wrong_type(field, v, "type 'object'"))
        }
        ("ts", v) if !matches!(v, Value::Timestamp { .. }) => {
            Err(wrong_type(field, v, "type 'timestamp'"))
        }
        ("t" | "v", v) if !number => Err(wrong_type(field, v, NUMBER)),
        ("wall", v) if !matches!(v, Value::DateTime(_)) => Err(wrong_type(field, v, "type 'date'")),
        ("needsRetryImage", v) if !matches!(v, Value::String(_)) => {
            Err(wrong_type(field, v, "type 'string'"))
        }
        ("multiOpType", v) if !matches!(v, Value::Int32(_)) => {
            Err(wrong_type(field, v, "type 'int'"))
        }
        ("stmtId", v) if !matches!(v, Value::Int32(_)) => Err(CommandError::new(
            INVALID_STMT_ID,
            format!("Field 'stmtId: {}' is not valid.", super::display(v)),
        )),
        ("ui", Value::Binary { subtype: UUID_SUBTYPE, bytes }) if bytes.len() != UUID_LEN => {
            Err(CommandError::new(
                INVALID_UUID,
                "uuid must be a 16-byte binary field with UUID (4) subtype",
            ))
        }
        ("ui", Value::Binary { subtype, .. }) if *subtype != UUID_SUBTYPE => {
            let name = crate::cluster_time::subtype_name(*subtype);
            let msg = format!(
                "BSON field 'OplogEntryBase.ui' is the wrong binData type '{name}', expected type 'UUID'"
            );
            Err(CommandError::new(TYPE_MISMATCH, msg))
        }
        ("ui", v) if !matches!(v, Value::Binary { .. }) => {
            Err(wrong_type(field, v, "type 'binData'"))
        }
        ("prevOpTime", Value::Document(p)) if p.get("ts").is_none() => {
            Err(missing("OpTimeBase.ts"))
        }
        ("prevOpTime", v) if !matches!(v, Value::Document(_)) => Err(CommandError::new(
            BOUND_NOT_OBJECT,
            "invalid parameter: expected an object (prevOpTime)",
        )),
        ("lsid", Value::Document(l)) if l.get("uid").is_none() => {
            Err(missing("OplogEntryBase.lsid.uid"))
        }
        _ => Ok(()),
    }
}

/// The operation of entry `d`, read when its turn comes.
fn parse(d: &Document) -> Result<Operation, CommandError> {
    for (field, v) in d.iter() {
        check_field(field, v)?;
    }
    if d.get("o").is_none() {
        let msg = "BSON field 'OplogEntryBase.o' is missing but a required field";
        return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
    }
    // Every field it reads was checked above.
    Operation::from_document(d).map_err(|e| CommandError::new(BAD_VALUE, format!("{e:?}")))
}

/// The error of a diff RefDb does not write (measured on 03-11-2026, `ap7.js` and `ap8.js`).
fn diff_error(e: DiffError) -> CommandError {
    match e {
        DiffError::Unexpected(k) => CommandError::new(
            UNEXPECTED_SECTION,
            format!("Unexpected section: {k} in document diff"),
        ),
        DiffError::NotObject(s) => {
            CommandError::new(SECTION_NOT_OBJECT, format!("Expected {s} section to be type object"))
        }
    }
}

/// Whether the reply of a write or command says it went through; its first write error ends the
/// command.
fn ok(reply: Document) -> Result<bool, CommandError> {
    // An insert entry is written as an upsert, but its error is the insert's, as RefDb gives it.
    let reply = super::fle_write::checked(reply).map_err(|mut e| {
        let plain = e.errmsg.strip_prefix(UPDATE_ERROR).map(str::to_owned);
        e.errmsg = plain.unwrap_or(e.errmsg);
        e
    })?;
    Ok(reply.get("ok") == Some(&Value::Double(1.0)))
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
    op: &Operation,
) -> Result<bool, CommandError> {
    let (db, coll) = op.ns.split_once('.').unwrap_or((op.ns.as_str(), ""));
    if op.op == OpType::Command {
        return ok(run_on(ctx, db, &op.o)?);
    }
    let ns = super::names::ns(db, coll);
    let entry = ctx.backend.collection(&ns)?;
    if op.op != OpType::Delete && entry.is_none() {
        return Err(CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!(
                "cannot apply insert or update operation on a non-existent namespace {}: {}",
                op.ns,
                super::display(&Value::Document(op.to_document()))
            ),
        ));
    }
    // An entry whose `ui` is not the collection's does not apply (measured on 03-11-2026).
    if op.ui.is_some_and(|ui| entry.map(|c| c.uuid) != Some(ui)) {
        return Ok(false);
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
                    teseladb_topology::diff::apply(&current, diff).map_err(diff_error)?
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
    ok(run_on(ctx, db, &write)?)
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
    let docs = entries.iter().enumerate().map(precheck).collect::<Result<Vec<_>, _>>()?;
    let mut results = Vec::new();
    for d in docs {
        // An error ends the command with what applied so far and the entry as not applied.
        let applied = parse(d).and_then(|op| match op.op {
            OpType::Noop => Ok(None),
            _ => apply_one(ctx, &op).map(Some),
        });
        match applied {
            Ok(None) => {}
            Ok(Some(done)) => results.push(Value::Boolean(done)),
            Err(e) => {
                results.push(Value::Boolean(false));
                return Ok(stopped(results, &e));
            }
        }
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

/// The reply of a command an entry's error ended: what applied, the error and its fields.
fn stopped(results: Vec<Value>, e: &CommandError) -> Document {
    let applied = i32::try_from(results.len()).unwrap_or(i32::MAX);
    let mut d = Document::new()
        .with("applied", applied)
        .with("code", e.code.code)
        .with("codeName", e.code.name)
        .with("errmsg", e.errmsg.as_str())
        .with("results", results)
        .with("ok", 0.0);
    for (k, v) in e.extra.iter() {
        d.push(k, v.clone());
    }
    d
}
