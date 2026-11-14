//! The maintenance commands of 006 FR-033 the reference router 9.0.4 answers (measured):
//! `dataSize`, `logRotate`, `dropConnections`, `lockInfo`, `connPoolStats` and `isdbgrid`, and
//! `rotateCertificates` (013 FR-006). A node keeps no
//! connection pool to shards nor lock table of its own, so those answer empty; `top` does not
//! exist in the reference router and is not a command here either.

use teseladb_bson::{Document, Value};

use super::admin::admin_only;
use super::query::{documents_of, for_each_match};
use super::{Backend, Ctx, names};
use crate::error::{
    BAD_VALUE, CommandError, IDL_UNKNOWN_FIELD, NAMESPACE_NOT_FOUND, NO_SUCH_KEY, OPERATION_FAILED,
};
use crate::generic::is_generic;
use crate::types::{number, wrong_type, wrong_types};
use teseladb_range::read::ReadConcern;

/// `dataSize`.
pub const DATA_SIZE: &str = "dataSize";
/// `logRotate`.
pub const LOG_ROTATE: &str = "logRotate";
/// `dropConnections`.
pub const DROP_CONNECTIONS: &str = "dropConnections";
/// `lockInfo`.
pub const LOCK_INFO: &str = "lockInfo";
/// `connPoolStats`.
pub const CONN_POOL_STATS: &str = "connPoolStats";
/// `isdbgrid`.
pub const IS_DB_GRID: &str = "isdbgrid";
/// `rotateCertificates`.
pub const ROTATE_CERTIFICATES: &str = "rotateCertificates";
/// The one field of `rotateCertificates`: a note for the log.
const MESSAGE: &str = "message";
/// The log `logRotate` rotates by default (`logRotate: 1` is the same).
const SERVER_LOG: &str = "server";
/// Upper bounds of the buckets of `acquisitionWaitTimes`, in milliseconds, every 50 up to 1000.
const WAIT_BUCKET_MS: u32 = 50;
const WAIT_BUCKETS: u32 = 20;

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// The types of a `dataSize` limit.
const LIMIT_TYPES: &[&str] = &["int", "decimal", "double", "long"];
/// The types of `dataSize.estimate`.
const ESTIMATE_TYPES: &[&str] = &["double", "long", "int", "bool", "decimal"];

/// A `dataSize` limit (`maxSize`, `maxObjects`) as a whole number, truncated toward zero: 0 (or
/// none) is no limit, and a negative one stops the count at the first document (measured on
/// a reference replica set member 9.0.4 on 03-11-2026).
fn limit(cmd: &Document, field: &str) -> Result<i64, CommandError> {
    match cmd.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => match number(v) {
            // NaN is 0 and a value past `i64` saturates.
            Some(x) => Ok(x as i64),
            None => Err(wrong_types(&format!("{DATA_SIZE}.{field}"), v, LIMIT_TYPES)),
        },
    }
}

/// `dataSize.estimate`: a boolean or a number, true when not zero.
fn estimate(cmd: &Document) -> Result<bool, CommandError> {
    match cmd.get("estimate") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(v) => match number(v) {
            Some(x) => Ok(x != 0.0),
            None => Err(wrong_types(&format!("{DATA_SIZE}.estimate"), v, ESTIMATE_TYPES)),
        },
    }
}

/// `dataSize`: bytes and documents of a collection, or of an `_id` range of it, stopping once
/// `maxSize` bytes or more than `maxObjects` documents are counted; `estimate` and `maxReached`
/// only from a member of a replica set (018), as a reference replica set member answers.
pub fn data_size<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let Value::String(text) = ctx.first().clone() else {
        let path = format!("{DATA_SIZE}.{}", ctx.name());
        return Err(wrong_type(&path, ctx.first(), "string"));
    };
    let Some((db, coll)) = text.split_once('.') else {
        return Err(CommandError::new(NAMESPACE_NOT_FOUND, format!("database {text} not found")));
    };
    let ns = names::ns(db, coll);
    names::check(&ns)?;
    let bound = |field: &str| match ctx.cmd.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d.get("_id").cloned().unwrap_or(Value::MinKey))),
        Some(v) => Err(wrong_type(&format!("{DATA_SIZE}.{field}"), v, "object")),
    };
    let filter = match (bound("min")?, bound("max")?) {
        (None, None) => Document::new(),
        (Some(lo), Some(hi)) => {
            Document::new().with("_id", Document::new().with("$gte", lo).with("$lt", hi))
        }
        (Some(_), None) => {
            return Err(CommandError::new(BAD_VALUE, "Max must be set if min is set."));
        }
        (None, Some(_)) => {
            return Err(CommandError::new(BAD_VALUE, "Max must not be set if min is not set."));
        }
    };
    let (max_size, max_objects) = (limit(ctx.cmd, "maxSize")?, limit(ctx.cmd, "maxObjects")?);
    let estimated = estimate(ctx.cmd)?;
    let started = ctx.backend.now_ms();
    let (mut size, mut objects, mut reached) = (0i64, 0i64, false);
    let coll = ctx.backend.collection(&ns)?;
    // Only a collection that exists has indexes to look the pattern up in (measured on a reference
    // replica set member 9.0.4 on 03-11-2026): one that does not exist counts nothing.
    if coll.is_some()
        && let Some(Value::Document(k)) = ctx.cmd.get("keyPattern")
        && !(k.len() == 1 && k.get("_id").is_some())
    {
        let msg = "Couldn't find valid index containing key pattern";
        return Err(CommandError::new(OPERATION_FAILED, msg));
    }
    if let Some(c) = &coll {
        for_each_match(ctx.backend, documents_of(c), &filter, None, ReadConcern::Local, |_, d| {
            size = size.saturating_add(i64::try_from(d.len()).unwrap_or(i64::MAX));
            objects += 1;
            reached =
                (max_size != 0 && size >= max_size) || (max_objects != 0 && objects > max_objects);
            Ok(!reached)
        })?;
    }
    let millis = ctx.backend.now_ms().saturating_sub(started);
    let mut d = Document::new()
        .with("size", size)
        .with("numObjects", objects)
        .with("millis", i64::try_from(millis).unwrap_or(i64::MAX));
    // The reference router 9.0.4 answers without either; a reference replica set member says
    // nothing of the estimate of a collection that does not exist (measured on 03-11-2026).
    let member = super::config_db::as_member(ctx.backend)?;
    if member && coll.is_some() {
        d.push("estimate", estimated);
    }
    if member && reached {
        d.push("maxReached", true);
    }
    Ok(d.with("ok", 1.0))
}

/// `logRotate`: the node writes its log to the standard error, which has nothing to rotate; a
/// log other than the server's does not exist.
pub fn log_rotate<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(LOG_ROTATE, ctx.db)?;
    match ctx.first() {
        Value::String(s) if s != SERVER_LOG => {
            Err(CommandError::new(NO_SUCH_KEY, "Log rotation failed due to one or more errors"))
        }
        _ => Ok(ok()),
    }
}

/// `rotateCertificates`: the node loads its certificates again. Its fields are checked in the
/// order of the document, before the database (measured on 9.0.4).
pub fn rotate_certificates<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let mut message = None;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            (MESSAGE, Value::String(m)) => message = Some(m.as_str()),
            (MESSAGE, other) => {
                return Err(wrong_type(
                    &format!("{ROTATE_CERTIFICATES}.{MESSAGE}"),
                    other,
                    "string",
                ));
            }
            (generic, _) if is_generic(generic) => {}
            (unknown, _) => {
                let msg =
                    format!("BSON field '{ROTATE_CERTIFICATES}.{unknown}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    admin_only(ROTATE_CERTIFICATES, ctx.db)?;
    ctx.backend.rotate_certificates(message)?;
    Ok(ok())
}

/// `dropConnections`: the node keeps no pool of connections to other servers.
pub fn drop_connections<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(DROP_CONNECTIONS, ctx.db)?;
    let field = format!("{DROP_CONNECTIONS}.hostAndPort");
    match ctx.cmd.get("hostAndPort") {
        Some(Value::Array(hosts)) => {
            for (i, h) in hosts.iter().enumerate() {
                if !matches!(h, Value::String(_)) {
                    return Err(wrong_type(&format!("{field}.{i}"), h, "string"));
                }
            }
            Ok(ok())
        }
        Some(other) => Err(wrong_type(&field, other, "array")),
        None => Err(super::args::missing(DROP_CONNECTIONS, "hostAndPort")),
    }
}

/// `lockInfo`: no lock is held between requests.
pub fn lock_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(LOCK_INFO, ctx.db)?;
    Ok(Document::new().with("lockInfo", Vec::<Value>::new()).with("ok", 1.0))
}

fn wait_times() -> Document {
    let mut d = Document::new().with("(-inf, 0 ms)", Document::new().with("count", 0i64));
    for i in 0..WAIT_BUCKETS {
        let (lo, hi) = (i * WAIT_BUCKET_MS, (i + 1) * WAIT_BUCKET_MS);
        d.push(format!("[{lo} ms, {hi} ms)"), Document::new().with("count", 0i64));
    }
    let top = WAIT_BUCKETS * WAIT_BUCKET_MS;
    d.push(format!("[{top} ms, inf)"), Document::new().with("count", 0i64));
    d.push("totalCount", 0i64);
    d
}

/// `connPoolStats`, with the reference server's fields: the node keeps no pool of connections to
/// shards.
pub fn conn_pool_stats<B: Backend + ?Sized>(
    _ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let mut d = Document::new();
    for field in [
        "numClientConnections",
        "numAScopedConnections",
        "totalInUse",
        "totalAvailable",
        "totalLeased",
        "totalCreated",
        "totalRefreshing",
        "totalRefreshed",
        "totalWasNeverUsed",
        "totalWasUsedOnce",
        "totalConnUsageTimeMillis",
        "totalRejectedRequests",
        "totalPendingRequests",
    ] {
        // 32-bit counters, then 64-bit ones (measured on the reference router 9.0.4).
        d.push(field, 0i32);
    }
    for field in ["totalConnectionAcquisitionRequests", "totalConnectionAcquisitionWaitTimeMillis"]
    {
        d.push(field, 0i64);
    }
    d.push("acquisitionWaitTimes", wait_times());
    d.push("replicaSetMatchingStrategy", "matchPrimaryNode");
    d.push("pools", Document::new());
    d.push("hosts", Document::new());
    d.push("numReplicaSetMonitorsCreated", 0i32);
    d.push("replicaSets", Document::new());
    d.push("ok", 1.0);
    Ok(d)
}

/// `isdbgrid`: the node answers as the reference router (D-006).
pub fn is_db_grid<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let host = ctx.backend.process().host.hostname;
    Ok(Document::new().with("isdbgrid", 1).with("hostname", host).with("ok", 1.0))
}
