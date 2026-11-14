//! `POST /v1/transactions` (FR-009): a list of reads and writes run as one transaction of 008
//! (`readConcern` `snapshot`, commit with `w: "majority"`) in a server session of its own, all or
//! nothing, with the result of each operation. Like `withTransaction` of the drivers, an error
//! labelled `TransientTransactionError` runs the whole transaction again and a commit that ends
//! in `UnknownTransactionCommitResult` is committed again, while the request's time lasts.
//!
//! The body is `{"operations": [...], "maxCommitTimeMS": n}`; each operation is
//! `{"op": "find" | "insert" | "update" | "delete" | "findAndModify", "db": …, "coll": …}` with
//! the fields of that command (`filter`, `documents`, `updates`, `deletes`, `query`, `update`…).
//! The answer is `{"results": [...]}`: the documents of a `find`, the reply of the others.

use teseladb_bson::{Document, Value};
use teseladb_wire::error::{
    BAD_VALUE, CommandError, FAILED_TO_PARSE, TRANSIENT_TRANSACTION_ERROR,
    UNKNOWN_TRANSACTION_COMMIT_RESULT,
};

use crate::api::Executor;
use crate::commands::check_session_fields;
use crate::problem::WIRE_FIELDS;

/// The operations a transaction takes, by the name of their command.
pub const OPERATIONS: [&str; 5] = ["find", "insert", "update", "delete", "findAndModify"];
/// How long the retries of a transaction may last: the plazo of a request (`limits.rs`).
pub const RETRY_BUDGET_MS: u64 = 120_000;
/// The most operations a transaction takes.
pub const MAX_OPERATIONS: usize = 1_000;

/// One operation: its database and its command.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// Its database.
    pub db: String,
    /// The command, without session fields.
    pub command: Document,
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

fn text<'a>(d: &'a Document, f: &str, i: usize) -> Result<&'a str, CommandError> {
    match d.get(f) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        _ => Err(CommandError::new(
            FAILED_TO_PARSE,
            format!("operations.{i}.{f} must be a non-empty string"),
        )),
    }
}

/// Reads the body: the operations and `maxCommitTimeMS`.
pub fn parse(body: &Document) -> Result<(Vec<Operation>, Option<i64>), CommandError> {
    check_session_fields(body)?;
    let mut max_commit = None;
    let mut list = None;
    for (k, v) in body.iter() {
        match (k, v) {
            ("operations", Value::Array(a)) => list = Some(a),
            ("maxCommitTimeMS", Value::Int32(n)) if *n >= 0 => max_commit = Some(i64::from(*n)),
            ("maxCommitTimeMS", Value::Int64(n)) if *n >= 0 => max_commit = Some(*n),
            ("operations" | "maxCommitTimeMS", _) => {
                return Err(bad(format!("{k} has the wrong type")));
            }
            _ => return Err(bad(format!("unknown field {k}"))),
        }
    }
    let list = list.ok_or_else(|| CommandError::new(FAILED_TO_PARSE, "operations is required"))?;
    if list.is_empty() || list.len() > MAX_OPERATIONS {
        return Err(bad(format!("operations takes 1 to {MAX_OPERATIONS} operations")));
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, v) in list.iter().enumerate() {
        let Value::Document(d) = v else {
            return Err(bad(format!("operations.{i} must be a document")));
        };
        check_session_fields(d)?;
        let op = text(d, "op", i)?;
        let Some(name) = OPERATIONS.iter().find(|n| **n == op) else {
            return Err(bad(format!("operations.{i}.op: {op} cannot run in a transaction")));
        };
        let db = text(d, "db", i)?.to_owned();
        let coll = text(d, "coll", i)?;
        let mut command = Document::new().with(*name, coll);
        for (k, v) in d.iter().filter(|(k, _)| !matches!(*k, "op" | "db" | "coll")) {
            if matches!(k, "readConcern" | "writeConcern") {
                return Err(bad(format!("operations.{i}.{k}: the transaction sets it")));
            }
            command.push(k, v.clone());
        }
        out.push(Operation { db, command });
    }
    Ok((out, max_commit))
}

fn is_ok(reply: &Document) -> bool {
    matches!(reply.get("ok"), Some(Value::Double(x)) if *x == 1.0)
        || matches!(reply.get("ok"), Some(Value::Int32(1)))
}

fn has_label(reply: &Document, label: &str) -> bool {
    matches!(reply.get("errorLabels"), Some(Value::Array(l))
        if l.iter().any(|v| matches!(v, Value::String(s) if s == label)))
}

/// Whether a reply of an operation failed: an error, or a write error.
fn failed(reply: &Document) -> bool {
    !is_ok(reply) || matches!(reply.get("writeErrors"), Some(Value::Array(e)) if !e.is_empty())
}

/// What ended a transaction that does not commit.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Committed: the result of each operation and the commit's reply.
    Committed(Vec<Value>, Document),
    /// Not committed: the reply that failed and the index of its operation (`None`: the
    /// commit).
    Failed(Document, Option<usize>),
}

/// The session fields of the `n`-th statement of transaction `txn` of session `lsid`.
fn in_session(mut cmd: Document, lsid: &[u8; 16], txn: i64, first: bool) -> Document {
    cmd.push(
        "lsid",
        Document::new().with("id", Value::Binary { subtype: 4, bytes: lsid.to_vec() }),
    );
    cmd.push("txnNumber", txn);
    if first {
        cmd.push("startTransaction", true);
        cmd.push("readConcern", Document::new().with("level", "snapshot"));
    }
    cmd.push("autocommit", false);
    cmd
}

/// The documents of a `find` in the transaction: the first batch and the `getMore`s after it.
fn documents<E: Executor + ?Sized>(
    exec: &mut E,
    op: &Operation,
    reply: &Document,
    lsid: &[u8; 16],
    txn: i64,
) -> Result<Vec<Value>, Document> {
    let cursor = |r: &Document, field: &str| -> (Vec<Value>, i64) {
        let c = r.get("cursor").and_then(Value::as_document);
        let batch = match c.and_then(|c| c.get(field)) {
            Some(Value::Array(b)) => b.clone(),
            _ => Vec::new(),
        };
        let id = match c.and_then(|c| c.get("id")) {
            Some(Value::Int64(n)) => *n,
            _ => 0,
        };
        (batch, id)
    };
    let (mut out, mut id) = cursor(reply, "firstBatch");
    let coll = op.command.iter().next().map_or(Value::Null, |(_, v)| v.clone());
    while id != 0 {
        let more = Document::new().with("getMore", id).with("collection", coll.clone());
        let r = exec.run(&op.db, in_session(more, lsid, txn, false));
        if failed(&r) {
            return Err(r);
        }
        let (batch, next) = cursor(&r, "nextBatch");
        out.extend(batch);
        id = next;
    }
    Ok(out)
}

/// The reply of an operation as its result: without the fields of the wire protocol.
fn result_of(reply: &Document) -> Value {
    let mut d = Document::new();
    for (k, v) in reply.iter().filter(|(k, _)| !WIRE_FIELDS.contains(k)) {
        d.push(k, v.clone());
    }
    Value::Document(d)
}

/// One attempt of the transaction, number `txn` of session `lsid`.
fn attempt<E: Executor + ?Sized>(
    exec: &mut E,
    ops: &[Operation],
    max_commit: Option<i64>,
    lsid: &[u8; 16],
    txn: i64,
    deadline: u64,
) -> Outcome {
    let mut results = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let reply = exec.run(&op.db, in_session(op.command.clone(), lsid, txn, i == 0));
        if failed(&reply) {
            abort(exec, lsid, txn);
            return Outcome::Failed(reply, Some(i));
        }
        if op.command.iter().next().is_some_and(|(k, _)| k == "find") {
            match documents(exec, op, &reply, lsid, txn) {
                Ok(docs) => results.push(Value::Array(docs)),
                Err(r) => {
                    abort(exec, lsid, txn);
                    return Outcome::Failed(r, Some(i));
                }
            }
        } else {
            results.push(result_of(&reply));
        }
    }
    let mut commit = Document::new()
        .with("commitTransaction", 1)
        .with("writeConcern", Document::new().with("w", "majority"));
    if let Some(ms) = max_commit {
        commit.push("maxTimeMS", ms);
    }
    let commit = in_session(commit, lsid, txn, false);
    loop {
        let reply = exec.run("admin", commit.clone());
        if is_ok(&reply) && reply.get("writeConcernError").is_none() {
            return Outcome::Committed(results, reply);
        }
        if has_label(&reply, UNKNOWN_TRANSACTION_COMMIT_RESULT) && exec.now_ms() < deadline {
            continue;
        }
        return Outcome::Failed(reply, None);
    }
}

fn abort<E: Executor + ?Sized>(exec: &mut E, lsid: &[u8; 16], txn: i64) {
    let cmd = in_session(Document::new().with("abortTransaction", 1), lsid, txn, false);
    let _ = exec.run("admin", cmd);
}

/// Runs the transaction: `{results, operationTime}` once committed, or the reply that failed
/// with `operationIndex` (absent when the commit failed).
pub fn run<E: Executor + ?Sized>(
    exec: &mut E,
    ops: &[Operation],
    max_commit: Option<i64>,
) -> Document {
    let mut lsid = [0u8; 16];
    exec.random(&mut lsid);
    lsid[6] = (lsid[6] & 0x0f) | 0x40;
    lsid[8] = (lsid[8] & 0x3f) | 0x80;
    let deadline = exec.now_ms().saturating_add(RETRY_BUDGET_MS);
    let mut txn = 0;
    let out = loop {
        txn += 1;
        match attempt(exec, ops, max_commit, &lsid, txn, deadline) {
            Outcome::Committed(results, reply) => {
                let mut d = Document::new().with("results", Value::Array(results));
                if let Some(t) = reply.get("operationTime") {
                    d.push("operationTime", t.clone());
                }
                break d.with("ok", 1.0);
            }
            Outcome::Failed(reply, at) => {
                if has_label(&reply, TRANSIENT_TRANSACTION_ERROR) && exec.now_ms() < deadline {
                    continue;
                }
                let mut d = reply;
                if let Some(i) = at {
                    d.insert("operationIndex", i64::try_from(i).unwrap_or(i64::MAX));
                }
                break d;
            }
        }
    };
    let end = Value::Document(
        Document::new().with("id", Value::Binary { subtype: 4, bytes: lsid.to_vec() }),
    );
    let _ = exec.run("admin", Document::new().with("endSessions", Value::Array(vec![end])));
    out
}
