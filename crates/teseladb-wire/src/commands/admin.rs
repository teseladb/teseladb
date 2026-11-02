//! Diagnostic and administration commands of a node (006 FR-020, FR-023): `currentOp` and
//! `killOp` over the node's table of operations in progress.
//!
//! A command runs on the node its client is connected to, so the node's table holds every
//! operation of its clients. `currentOp` names each one `node<N>:<id>` under the node's shard
//! name (D-102), as a `refrouter` names a shard's; `killOp` takes that, or the number alone.

use teseladb_bson::{Document, Value};

use super::catalog::passes;
use super::session::KillScope;
use super::{Backend, Ctx, lock};
use crate::error::{
    BAD_VALUE, CommandError, FAILED_TO_PARSE, KILL_OP_FORMAT, KILL_OP_MISSING_OP, KILL_OP_OP_TYPE,
    KILL_OP_RANGE, OVERFLOW, SHARD_NOT_FOUND, UNAUTHORIZED,
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
    if super::presentation::replica_set(ctx.backend)?.is_some() {
        return super::replset::get_status(ctx);
    }
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

/// What `killOp`'s `op` names.
enum Target {
    /// An operation of this node, by number: `None` for a valid number that cannot be running
    /// (it does not fit the table's ids), which `killOp` accepts and ignores, as RefDb does.
    Local(Option<u32>),
    /// Operation `id` of shard `shard` (`shardid:opid`): a node (`node<N>`, D-102) or `config`.
    Shard { shard: String, node: Option<u64>, id: i32 },
}

/// The name a `refrouter` gives its config server, which `killOp` takes as a shard (measured).
pub const CONFIG_SHARD: &str = "config";

/// The node a shard name (`node<N>`) of the cluster names, if it is one.
fn named_node<B: Backend + ?Sized>(
    backend: &mut B,
    shard: &str,
) -> Result<Option<u64>, CommandError> {
    let names = backend.config_documents(super::config_db::SHARDS)?;
    let listed = names.iter().any(|d| matches!(d.get("_id"), Some(Value::String(n)) if n == shard));
    let node = shard.strip_prefix(super::SHARD_PREFIX).and_then(|n| n.parse::<u64>().ok());
    let mine = super::shard_name(backend.node());
    Ok(node.filter(|_| listed || shard == mine))
}

/// The number of a `shardid:opid`, read as RefDb reads a 32-bit integer (measured): an
/// optional sign and decimal digits, nothing else.
fn shard_op_id(text: &str) -> Result<i32, CommandError> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CommandError::new(FAILED_TO_PARSE, "Did not consume whole string."));
    }
    text.trim_start_matches('+').parse::<i32>().map_err(|_| CommandError::new(OVERFLOW, "Overflow"))
}

/// What `killOp`'s `op` names.
fn op_target<B: Backend + ?Sized>(backend: &mut B, v: &Value) -> Result<Target, CommandError> {
    let n = match v {
        Value::String(s) => {
            let (shard, id) = match s.split_once(':') {
                Some((shard, id)) if !shard.is_empty() && !id.is_empty() => (shard, id),
                _ => {
                    let msg = format!(
                        "The op argument to killOp must be of the format shardid:opid but found \"{s}\""
                    );
                    return Err(CommandError::new(KILL_OP_FORMAT, msg));
                }
            };
            let node = named_node(backend, shard)?;
            if node.is_none() && shard != CONFIG_SHARD {
                return Err(CommandError::new(SHARD_NOT_FOUND, format!("Shard {shard} not found")));
            }
            return Ok(Target::Shard { shard: shard.to_owned(), node, id: shard_op_id(id)? });
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
        Ok(id) => Ok(Target::Local(u32::try_from(id).ok())),
        Err(_) => {
            let msg = format!("invalid op : {n}. Op ID cannot be represented with 32 bits");
            Err(CommandError::new(KILL_OP_RANGE, msg))
        }
    }
}

/// `killOp`: marks the operation; it stops at its next step with `Interrupted`. A
/// `shardid:opid` of another node goes to that node, and the reply names the shard and the
/// number, as a `refrouter` gives them (DV-031, D-102).
pub fn kill_op<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(KILL_OP, ctx.db)?;
    let Some(v) = ctx.cmd.get("op") else {
        return Err(CommandError::new(KILL_OP_MISSING_OP, "Did not provide \"op\" field"));
    };
    match op_target(ctx.backend, v)? {
        Target::Local(id) => {
            if let Some(id) = id {
                lock(ctx.shared).ops.kill(id);
            }
            Ok(Document::new().with("ok", 1.0))
        }
        Target::Shard { shard, node, id } => {
            let mine = ctx.backend.node();
            match (node, u32::try_from(id)) {
                (Some(n), Ok(op)) if n == mine => {
                    lock(ctx.shared).ops.kill(op);
                }
                (Some(n), Ok(op)) => ctx.backend.kill_everywhere(&KillScope::Op { node: n, op })?,
                _ => {}
            }
            Ok(Document::new().with("shard", shard).with("shardid", id).with("ok", 1.0))
        }
    }
}

/// One operation as `currentOp` shows it, on node `shard` at `host` (as a `refrouter` shows a
/// shard's, DV-031, D-102).
fn describe(id: u32, op: &Op, now_ms: u64, (shard, host): (&str, &str)) -> Document {
    let mut d = Document::new().with("shard", shard).with("type", "op").with("host", host);
    match &op.desc {
        // An operation of the node itself (an index build) has no connection.
        Some(desc) => d.push("desc", desc.clone()),
        None => {
            d.push("desc", format!("conn{}", op.conn));
            d.push("connectionId", op.conn);
        }
    }
    if let Some(c) = &op.client {
        d.insert("client_s", c.clone());
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
    d.insert("opid", format!("{shard}:{id}"));
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
    for (k, v) in op.metrics.iter().flat_map(Document::iter) {
        d.insert(k, v.clone());
    }
    d.insert("numYields", 0);
    d.insert("locks", Document::new());
    d.insert("waitingForLock", false);
    if op.killed {
        d.insert("killPending", true);
    }
    d
}

/// The operations running on node `node` at `host` (from its `shared` registry) at `now_ms`, as
/// `currentOp` describes them; each node answers the others with these (010 T033).
pub fn node_operations(
    shared: &super::Shared,
    node: u64,
    host: &str,
    now_ms: u64,
) -> Vec<Document> {
    let shard = super::shard_name(node);
    lock(shared).ops.iter().map(|(id, op)| describe(id, op, now_ms, (&shard, host))).collect()
}

/// The operations of every node of the cluster, this one's first, as `currentOp` describes
/// them (010 T033, FR-016): a `refrouter` lists those of all its shards.
pub(crate) fn current_operations<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
) -> Result<Vec<Document>, CommandError> {
    let now = backend.now_ms();
    let host = super::host_port(backend);
    let mut ops = node_operations(shared, backend.node(), &host, now);
    ops.extend(backend.other_operations()?);
    Ok(ops)
}

/// `currentOp`: the operations in progress on every node that match the command's other fields.
pub fn current_op<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(CURRENT_OP, ctx.db)?;
    let mut filter = Document::new();
    for (k, v) in ctx.cmd.iter() {
        if !CURRENT_OP_OPTIONS.contains(&k) {
            filter.insert(k, v.clone());
        }
    }
    let ops = current_operations(ctx.backend, ctx.shared)?;
    let mut inprog = Vec::new();
    for d in ops {
        if passes(Some(&filter), &d)? {
            inprog.push(Value::Document(d));
        }
    }
    Ok(Document::new().with("inprog", Value::Array(inprog)).with("ok", 1.0))
}
