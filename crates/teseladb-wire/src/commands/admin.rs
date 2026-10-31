//! Diagnostic and administration commands of a node (006 FR-020, FR-023): `currentOp` and
//! `killOp` over the node's table of operations in progress.
//!
//! A command runs on the node its client is connected to, so the node's table holds every
//! operation of its clients; operation ids are numbers, as those of a `refrouter`'s own operations
//! (`localOps`), and a `shardid:opid` names a shard, which TeselaDB does not have (DV-031).

use teseladb_bson::{Document, Value};

use super::catalog::passes;
use super::{Backend, Ctx, lock};
use crate::error::{
    BAD_VALUE, CommandError, KILL_OP_FORMAT, KILL_OP_MISSING_OP, KILL_OP_OP_TYPE, KILL_OP_RANGE,
    SHARD_NOT_FOUND, UNAUTHORIZED,
};
use crate::ops::Op;
use crate::types::type_name;

/// `killOp`.
pub const KILL_OP: &str = "killOp";
/// `currentOp`.
pub const CURRENT_OP: &str = "currentOp";
/// The database of the commands that act on the whole node.
pub const ADMIN_DB: &str = "admin";

/// The fields of `currentOp` that are options and not part of its filter: its own, and those
/// every command takes.
const CURRENT_OP_OPTIONS: [&str; 20] = [
    CURRENT_OP,
    "$all",
    "$ownOps",
    "$truncateOps",
    "localOps",
    "allUsers",
    "idleConnections",
    "idleCursors",
    "idleSessions",
    "backtrace",
    "targetAllNodes",
    "$db",
    "lsid",
    "txnNumber",
    "$clusterTime",
    "$readPreference",
    "comment",
    "maxTimeMS",
    "readConcern",
    "writeConcern",
];

/// `replSetGetStatus`.
pub const REPL_SET_GET_STATUS: &str = "replSetGetStatus";

/// `replSetGetStatus` as a `refrouter` 9.0.4 answers it (FR-024): `ok: 0` with no code, since a
/// node that presents itself as a `refrouter` has no replica set (018 adds the replica set mode).
pub fn repl_set_get_status<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(REPL_SET_GET_STATUS, ctx.db)?;
    Ok(Document::new()
        .with("info", "refrouter")
        .with("ok", 0.0)
        .with("errmsg", "replSetGetStatus is not supported through refrouter"))
}

/// `Unauthorized` for a command run outside `admin`.
pub fn admin_only(name: &str, db: &str) -> Result<(), CommandError> {
    if db == ADMIN_DB {
        return Ok(());
    }
    Err(CommandError::new(
        UNAUTHORIZED,
        format!("{name} may only be run against the admin database."),
    ))
}

/// The operation id of `killOp`'s `op`: `None` for a valid id that cannot be running (it does
/// not fit the table's ids), which `killOp` accepts and ignores, as RefDb does.
fn op_id(v: &Value) -> Result<Option<u32>, CommandError> {
    let n = match v {
        Value::String(s) => {
            let shard = match s.split_once(':') {
                Some((shard, id)) if !shard.is_empty() && !id.is_empty() => shard,
                _ => {
                    let msg = format!(
                        "The op argument to killOp must be of the format shardid:opid but found \"{s}\""
                    );
                    return Err(CommandError::new(KILL_OP_FORMAT, msg));
                }
            };
            return Err(CommandError::new(SHARD_NOT_FOUND, format!("Shard {shard} not found")));
        }
        Value::Int32(n) => i64::from(*n),
        Value::Int64(n) => *n,
        Value::Double(_) | Value::Decimal128(_) => {
            let x = crate::types::number(v).unwrap_or(f64::NAN);
            let exact = x.fract() == 0.0 && x >= -(2f64.powi(63)) && x < 2f64.powi(63);
            if !exact {
                let msg = format!(
                    "Expected field \"op\" to have a value exactly representable as a 64-bit integer, but found op: {}",
                    crate::commands::display(v)
                );
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            x as i64
        }
        other => {
            let msg = format!("\"op\" field was of unsupported type {}", type_name(other));
            return Err(CommandError::new(KILL_OP_OP_TYPE, msg));
        }
    };
    match i32::try_from(n) {
        Ok(id) => Ok(u32::try_from(id).ok()),
        Err(_) => {
            let msg = format!("invalid op : {n}. Op ID cannot be represented with 32 bits");
            Err(CommandError::new(KILL_OP_RANGE, msg))
        }
    }
}

/// `killOp`: marks the operation; it stops at its next step with `Interrupted`.
pub fn kill_op<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(KILL_OP, ctx.db)?;
    let Some(v) = ctx.cmd.get("op") else {
        return Err(CommandError::new(KILL_OP_MISSING_OP, "Did not provide \"op\" field"));
    };
    if let Some(id) = op_id(v)? {
        lock(ctx.shared).ops.kill(id);
    }
    Ok(Document::new().with("ok", 1.0))
}

/// One operation as `currentOp` shows it.
fn describe(id: u32, op: &Op, now_ms: u64) -> Document {
    let mut d = Document::new().with("type", "op");
    match &op.desc {
        // An operation of the node itself (an index build) has no connection.
        Some(desc) => d.push("desc", desc.clone()),
        None => {
            d.push("desc", format!("conn{}", op.conn));
            d.push("connectionId", op.conn);
        }
    }
    if let Some(c) = &op.client {
        d.insert("client", c.clone());
    }
    if let Some(a) = &op.app_name {
        d.insert("appName", a.clone());
    }
    if let Some(m) = &op.metadata {
        d.insert("clientMetadata", m.clone());
    }
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let time = teseladb_bson::extjson::to_log_time(now).unwrap_or_default();
    let micros = now_ms.saturating_sub(op.started_ms).saturating_mul(1000);
    // A `refnode` 9.0.4 lists an index build as inactive (measured).
    d.insert("active", op.desc.is_none());
    d.insert("currentOpTime", time);
    d.insert("opid", i32::try_from(id).unwrap_or(i32::MAX));
    if let Some(lsid) = op.command.get("lsid") {
        d.insert("lsid", lsid.clone());
    }
    d.insert("secs_running", i64::try_from(micros / 1_000_000).unwrap_or(i64::MAX));
    d.insert("microsecs_running", i64::try_from(micros).unwrap_or(i64::MAX));
    d.insert("op", op.op);
    d.insert("ns", op.ns.clone());
    d.insert("command", op.command.clone());
    if let Some(p) = op.progress {
        d.insert("msg", p.message());
        let progress = Document::new()
            .with("done", i64::try_from(p.done).unwrap_or(i64::MAX))
            .with("total", i64::try_from(p.total).unwrap_or(i64::MAX));
        d.insert("progress", progress);
    }
    d.insert("numYields", 0);
    d.insert("locks", Document::new());
    d.insert("waitingForLock", false);
    if op.killed {
        d.insert("killPending", true);
    }
    d
}

/// `currentOp`: the node's operations in progress that match the command's other fields.
pub fn current_op<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(CURRENT_OP, ctx.db)?;
    let mut filter = Document::new();
    for (k, v) in ctx.cmd.iter() {
        if !CURRENT_OP_OPTIONS.contains(&k) {
            filter.insert(k, v.clone());
        }
    }
    let now = ctx.backend.now_ms();
    let ops: Vec<Document> =
        lock(ctx.shared).ops.iter().map(|(id, op)| describe(id, op, now)).collect();
    let mut inprog = Vec::new();
    for d in ops {
        if passes(Some(&filter), &d)? {
            inprog.push(Value::Document(d));
        }
    }
    Ok(Document::new().with("inprog", Value::Array(inprog)).with("ok", 1.0))
}
