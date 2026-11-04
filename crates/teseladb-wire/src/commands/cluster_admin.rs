//! The cluster's default concerns (007 FR-018, measured on a `refrouter` 9.0.4):
//! `setDefaultRWConcern` keeps them in Group 0 (as the internal cluster parameter
//! [`RW_DEFAULTS`], which `getClusterParameter` does not list) and `getDefaultRWConcern` shows
//! them over the implicit ones (`level: "local"`, `w: "majority"`). A command without its own
//! `readConcern` or `writeConcern` takes them ([`Defaults`], cached on each node for at most
//! [`DEFAULTS_REFRESH_MS`], as a `refrouter` caches them); errors then print the write concern with
//! `provenance: "customDefault"`.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};

use super::admin::admin_only;
use super::{Backend, Ctx, lock};
use crate::concerns::{Level, ReadConcernArgs, WArg, WriteConcernArgs};
use crate::error::{BAD_VALUE, Code, CommandError, ILLEGAL_OPERATION, UNKNOWN_REPL_WRITE_CONCERN};
use crate::types::wrong_type;

/// `getDefaultRWConcern`.
pub const GET_DEFAULT_RW_CONCERN: &str = "getDefaultRWConcern";
/// `setDefaultRWConcern`.
pub const SET_DEFAULT_RW_CONCERN: &str = "setDefaultRWConcern";
/// The cluster parameter, internal, that keeps the defaults (RefDb's `config.settings`
/// document of the same `_id`).
pub const RW_DEFAULTS: &str = "ReadWriteConcernDefaults";
/// How long a node keeps the defaults it read before it reads them again.
pub const DEFAULTS_REFRESH_MS: u64 = 1_000;

const DEFAULT_READ_CONCERN: &str = "defaultReadConcern";
const DEFAULT_WRITE_CONCERN: &str = "defaultWriteConcern";
const UPDATE_OP_TIME: &str = "updateOpTime";
const UPDATE_WALL_CLOCK_TIME: &str = "updateWallClockTime";
const IN_MEMORY: &str = "inMemory";
const GLOBAL: &str = "global";
const IMPLICIT: &str = "implicit";

/// The defaults a node applies, as it last read them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Defaults {
    /// The default read concern's level, if one was set.
    pub read: Option<Level>,
    /// The default write concern, if one was set.
    pub write: Option<WriteConcernArgs>,
    /// When the node read them, on the backend's clock (`None`: never).
    pub read_at_ms: Option<u64>,
}

/// The stored defaults, if any were set.
fn stored<B: Backend + ?Sized>(backend: &mut B) -> Result<Option<Document>, CommandError> {
    let all = backend.cluster_parameters()?;
    Ok(stored_in(&all))
}

fn stored_in(all: &BTreeMap<String, Vec<u8>>) -> Option<Document> {
    let b = all.get(RW_DEFAULTS)?;
    RawDoc::from_bytes(b).ok().map(Document::from)
}

/// The defaults of a stored document.
pub fn defaults_of(stored: Option<&Document>, now_ms: u64) -> Defaults {
    let read = stored
        .and_then(|d| d.get(DEFAULT_READ_CONCERN))
        .and_then(|v| match v {
            Value::Document(d) => d.get("level"),
            _ => None,
        })
        .and_then(|v| match v {
            Value::String(s) => Level::from_name(s),
            _ => None,
        });
    let write = stored.and_then(|d| d.get(DEFAULT_WRITE_CONCERN)).and_then(|v| match v {
        Value::Document(wc) => {
            let cmd = Document::new().with("writeConcern", wc.clone());
            WriteConcernArgs::parse(&cmd, SET_DEFAULT_RW_CONCERN).ok().flatten()
        }
        _ => None,
    });
    Defaults { read, write, read_at_ms: Some(now_ms) }
}

/// The node's defaults, read again from Group 0 when its copy is older than
/// [`DEFAULTS_REFRESH_MS`].
pub fn current<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Defaults, CommandError> {
    let now = ctx.backend.now_ms();
    let cached = lock(ctx.shared).rw_defaults.clone();
    if cached.read_at_ms.is_some_and(|at| now.saturating_sub(at) < DEFAULTS_REFRESH_MS) {
        return Ok(cached);
    }
    let fresh = defaults_of(stored(ctx.backend)?.as_ref(), now);
    lock(ctx.shared).rw_defaults = fresh.clone();
    Ok(fresh)
}

/// `serverStatus`'s `defaultRWConcern`: what `getDefaultRWConcern` answers, without `ok`.
pub fn server_status_section<B: Backend + ?Sized>(
    backend: &mut B,
) -> Result<Document, CommandError> {
    let now = backend.now_ms();
    let mut d = shown(stored(backend)?.as_ref(), now, false);
    d.remove("ok");
    Ok(d)
}

/// The reply of both commands: the defaults over the implicit ones.
fn shown(stored: Option<&Document>, now_ms: u64, in_memory: bool) -> Document {
    let rc = stored.and_then(|d| d.get(DEFAULT_READ_CONCERN)).cloned();
    let wc = stored.and_then(|d| d.get(DEFAULT_WRITE_CONCERN)).cloned();
    let implicit_rc = Value::Document(Document::new().with("level", Level::Local.name()));
    let implicit_wc =
        Value::Document(Document::new().with("w", crate::concerns::MAJORITY).with("wtimeout", 0));
    let mut d = Document::new()
        .with(DEFAULT_READ_CONCERN, rc.clone().unwrap_or(implicit_rc))
        .with(DEFAULT_WRITE_CONCERN, wc.clone().unwrap_or(implicit_wc));
    for k in [UPDATE_OP_TIME, UPDATE_WALL_CLOCK_TIME] {
        if let Some(v) = stored.and_then(|s| s.get(k)) {
            d.push(k, v.clone());
        }
    }
    d.push("defaultWriteConcernSource", if wc.is_some() { GLOBAL } else { IMPLICIT });
    d.push("defaultReadConcernSource", if rc.is_some() { GLOBAL } else { IMPLICIT });
    if in_memory {
        d.push(IN_MEMORY, true);
    }
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    d.push("localUpdateWallClockTime", Value::DateTime(now));
    d.push("ok", 1.0);
    d
}

/// `getDefaultRWConcern`.
pub fn get_default_rw_concern<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(GET_DEFAULT_RW_CONCERN, ctx.db)?;
    let in_memory = match ctx.cmd.get(IN_MEMORY) {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(v) => {
            return Err(wrong_type(&format!("{GET_DEFAULT_RW_CONCERN}.{IN_MEMORY}"), v, "bool"));
        }
    };
    let now = ctx.backend.now_ms();
    let s = stored(ctx.backend)?;
    Ok(shown(s.as_ref(), now, in_memory))
}

fn not_suitable(what: impl std::fmt::Display) -> CommandError {
    CommandError::new(BAD_VALUE, format!("{what} is not suitable for the default read concern"))
}

/// The default read concern given: `None` to unset it (`{}`), or its document.
fn read_default(v: &Value) -> Result<Option<Document>, CommandError> {
    let Value::Document(d) = v else {
        return Err(wrong_type(
            &format!("{SET_DEFAULT_RW_CONCERN}.{DEFAULT_READ_CONCERN}"),
            v,
            "object",
        ));
    };
    let rc =
        ReadConcernArgs::parse(&Document::new().with("readConcern", d.clone()), "readConcern")?
            .unwrap_or_default();
    if rc.after_cluster_time.is_some() {
        return Err(not_suitable("'afterClusterTime'"));
    }
    if rc.at_cluster_time.is_some() {
        return Err(not_suitable("'atClusterTime'"));
    }
    if rc.after_op_time.is_some() {
        return Err(not_suitable("'afterOpTime'"));
    }
    match rc.level {
        None => Ok(None),
        Some(l @ (Level::Snapshot | Level::Linearizable)) => {
            Err(not_suitable(format!("level: '{}'", l.name())))
        }
        Some(l) => Ok(Some(Document::new().with("level", l.name()))),
    }
}

/// The default write concern given, as it is shown: `w`, `j` if given and `wtimeout`.
fn write_default<B: Backend + ?Sized>(
    backend: &mut B,
    v: &Value,
) -> Result<Option<Document>, CommandError> {
    let Value::Document(d) = v else {
        return Err(wrong_type(
            &format!("{SET_DEFAULT_RW_CONCERN}.{DEFAULT_WRITE_CONCERN}"),
            v,
            "object",
        ));
    };
    if d.is_empty() {
        return Ok(None);
    }
    let cmd = Document::new().with("writeConcern", d.clone());
    let Some(wc) = WriteConcernArgs::parse(&cmd, SET_DEFAULT_RW_CONCERN)? else { return Ok(None) };
    match &wc.w {
        Some(WArg::Nodes(0)) => {
            let msg = "Unacknowledged write concern is not suitable for the default write concern";
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        Some(WArg::Mode(_) | WArg::Tags(_)) => {
            // No node has tag sets of write concerns: none defines the value.
            let shards: Vec<String> = backend
                .config_documents("shards")?
                .iter()
                .filter_map(|s| match s.get("_id") {
                    Some(Value::String(n)) => Some(format!("'{n}'")),
                    _ => None,
                })
                .collect();
            let msg = format!(
                "The value of the defaultWriteConcern must be specified on the config server and all shards. The definition of the provided value is missing on: {}.",
                shards.join(", ")
            );
            return Err(CommandError::new(UNKNOWN_REPL_WRITE_CONCERN, msg));
        }
        Some(WArg::Majority | WArg::Nodes(_)) | None => {}
    }
    let mut out = Document::new();
    if let Some(w) = d.get("w") {
        out.push("w", w.clone());
    }
    if let Some(j) = d.get("j") {
        out.push("j", j.clone());
    }
    out.push("wtimeout", wc.wtimeout);
    Ok(Some(out))
}

/// `setDefaultRWConcern`.
pub fn set_default_rw_concern<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(SET_DEFAULT_RW_CONCERN, ctx.db)?;
    let (rc, wc) = (ctx.cmd.get(DEFAULT_READ_CONCERN), ctx.cmd.get(DEFAULT_WRITE_CONCERN));
    if rc.is_none() && wc.is_none() {
        let msg = "At least one of the \"defaultReadConcern\" or \"defaultWriteConcern\" fields must be present";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    let before = stored(ctx.backend)?;
    let old = |k: &str| before.as_ref().and_then(|d| d.get(k)).cloned();
    let read = match rc {
        Some(v) => read_default(v)?.map(Value::Document),
        None => old(DEFAULT_READ_CONCERN),
    };
    let write = match wc {
        Some(v) => match write_default(ctx.backend, v)? {
            Some(d) => Some(Value::Document(d)),
            None if old(DEFAULT_WRITE_CONCERN).is_some() => {
                let msg = "The global default write concern cannot be unset once it is set.";
                return Err(CommandError::new(ILLEGAL_OPERATION, msg));
            }
            None => None,
        },
        None => old(DEFAULT_WRITE_CONCERN),
    };
    let now = ctx.backend.now_ms();
    let mut doc = Document::new();
    if let Some(r) = read {
        doc.push(DEFAULT_READ_CONCERN, r);
    }
    if let Some(w) = write {
        doc.push(DEFAULT_WRITE_CONCERN, w);
    }
    let secs = u32::try_from(now / 1_000).unwrap_or(u32::MAX);
    doc.push(UPDATE_OP_TIME, Value::Timestamp { time: secs, increment: 1 });
    doc.push(UPDATE_WALL_CLOCK_TIME, Value::DateTime(i64::try_from(now).unwrap_or(i64::MAX)));
    ctx.backend.set_cluster_parameter(RW_DEFAULTS, doc.to_bytes()?)?;
    lock(ctx.shared).rw_defaults = defaults_of(Some(&doc), now);
    Ok(shown(Some(&doc), now, false))
}

/// `fsync`.
pub const FSYNC: &str = "fsync";
/// `compact`.
pub const COMPACT: &str = "compact";
/// `autoCompact`.
pub const AUTO_COMPACT: &str = "autoCompact";
/// `shutdown`.
pub const SHUTDOWN: &str = "shutdown";

/// The types `fsync.lock` takes, in a `refrouter` 9.0.4's order (measured).
const LOCK_TYPES: [&str; 5] = ["decimal", "int", "long", "bool", "double"];
/// The types `shutdown.timeoutSecs` takes (measured).
const TIMEOUT_TYPES: [&str; 4] = ["long", "decimal", "double", "int"];

/// Each node as a `refrouter` names a shard in `fsync`'s `raw`: `<shard>/<host>`.
pub(crate) fn raw_names<B: Backend + ?Sized>(backend: &mut B) -> Result<Vec<String>, CommandError> {
    Ok(backend
        .config_documents("shards")?
        .iter()
        .filter_map(|s| match (s.get("_id"), s.get("host")) {
            (Some(Value::String(id)), Some(Value::String(host))) => Some(format!("{id}/{host}")),
            _ => None,
        })
        .collect())
}

/// The failure of a command a `refrouter` sent to every shard, with the shards' answers in `raw`:
/// `raw`, `code`, `codeName`, `ok` and `errmsg`, in that order (measured on 9.0.4 with
/// `fsyncUnlock` and `appendOplogNote`).
pub(crate) fn raw_failure(raw: Document, code: Code, errmsg: &str) -> Document {
    Document::new()
        .with("raw", raw)
        .with("code", code.code)
        .with("codeName", code.name)
        .with("ok", 0.0)
        .with("errmsg", errmsg)
}

/// An error of `fsync` as a `refrouter` 9.0.4 gives it (measured on 03-11-2026): `numFiles`, each
/// shard's answer in `all.raw` with the code beside it, `ok: 0` and `errmsg`, without a code of
/// its own.
fn fsync_error<B: Backend + ?Sized>(backend: &mut B, e: CommandError) -> Document {
    let names = raw_names(backend).unwrap_or_default();
    let each = Document::new()
        .with("ok", 0.0)
        .with("errmsg", e.errmsg.clone())
        .with("code", e.code.code)
        .with("codeName", e.code.name);
    let raw = names.into_iter().fold(Document::new(), |d, n| d.with(n, each.clone()));
    let all =
        Document::new().with("raw", raw).with("code", e.code.code).with("codeName", e.code.name);
    Document::new().with("numFiles", 1).with("all", all).with("ok", 0.0).with("errmsg", e.errmsg)
}

fn unknown_field(name: &str, k: &str) -> CommandError {
    CommandError::new(
        crate::error::IDL_UNKNOWN_FIELD,
        format!("BSON field '{name}.{k}' is an unknown field."),
    )
}

/// Whether `k` is a field every command may carry.
fn generic(k: &str) -> bool {
    crate::generic::GENERIC_ARGUMENTS.contains(&k)
}

/// `fsync`: every write a node acknowledged is already on its disk, so there is nothing to
/// flush; the answer has each node as a `refrouter` has each shard. With `lock` it takes one more of
/// the cluster's locks against writes ([`super::write_block::lock`]).
pub fn fsync<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(FSYNC, ctx.db)?;
    let mut lock = false;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            ("lock", Value::Boolean(b)) => lock = *b,
            (
                "lock",
                Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_),
            ) => {
                lock = crate::types::number(v).is_some_and(|n| n != 0.0);
            }
            ("lock", v) => {
                let e = crate::types::wrong_types(&format!("{FSYNC}.lock"), v, &LOCK_TYPES);
                return Ok(fsync_error(ctx.backend, e));
            }
            (k, _) if generic(k) => {}
            (k, _) => return Ok(fsync_error(ctx.backend, unknown_field(FSYNC, k))),
        }
    }
    if lock {
        return super::write_block::lock(ctx.backend);
    }
    let each = Document::new().with("numFiles", 1).with("ok", 1);
    let raw =
        raw_names(ctx.backend)?.into_iter().fold(Document::new(), |d, n| d.with(n, each.clone()));
    Ok(Document::new()
        .with("numFiles", 1)
        .with("all", Document::new().with("raw", raw))
        .with("ok", 1.0))
}

/// `compact` (DV-017): a `refrouter` refuses it and a `refnode` runs it on itself; TeselaDB takes
/// it on any node with the fields and checks of a `refnode` 9.0.4 (measured: without `force`
/// the node refuses, as a primary does). The node's LSM already compacts as it writes, so
/// nothing is left to free.
pub fn compact<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let mut force = false;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            ("force", Value::Boolean(b)) => force = *b,
            ("force", v) => return Err(wrong_type(&format!("{COMPACT}.force"), v, "bool")),
            ("dryRun", Value::Boolean(_)) | ("freeSpaceTargetMB", _) => {}
            ("dryRun", v) => return Err(wrong_type(&format!("{COMPACT}.dryRun"), v, "bool")),
            (k, _) if generic(k) => {}
            (k, _) => return Err(unknown_field(COMPACT, k)),
        }
    }
    if !force {
        let msg = "will not run compact on an active replica set primary as this will slow down other running operations. use force:true to force";
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    let Value::String(coll) = ctx.first() else {
        return Err(wrong_type(&format!("{COMPACT}.{COMPACT}"), ctx.first(), "string"));
    };
    let ns = super::names::ns(ctx.db, coll);
    if ctx.backend.collection(&ns)?.is_none() {
        let msg = format!("collection does not exist: {}.{}", ns.db, ns.coll);
        return Err(CommandError::new(crate::error::NAMESPACE_NOT_FOUND, msg));
    }
    Ok(Document::new().with("bytesFreed", 0).with("ok", 1.0))
}

/// `autoCompact` (DV-017): the node's LSM always compacts in the background; the switch is
/// taken with a `refnode`'s checks (measured).
pub fn auto_compact<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(AUTO_COMPACT, ctx.db)?;
    if !matches!(ctx.first(), Value::Boolean(_)) {
        return Err(wrong_type(&format!("{AUTO_COMPACT}.{AUTO_COMPACT}"), ctx.first(), "bool"));
    }
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            ("runOnce", Value::Boolean(_)) | ("freeSpaceTargetMB", _) => {}
            ("runOnce", v) => {
                return Err(wrong_type(&format!("{AUTO_COMPACT}.runOnce"), v, "bool"));
            }
            (k, _) if generic(k) => {}
            (k, _) => return Err(unknown_field(AUTO_COMPACT, k)),
        }
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `shutdown`: stops this node, as a `refrouter` stops itself, with its checks (measured).
pub fn shutdown<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(SHUTDOWN, ctx.db)?;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            ("force", Value::Boolean(_)) => {}
            ("force", v) => return Err(wrong_type(&format!("{SHUTDOWN}.force"), v, "bool")),
            (
                "timeoutSecs",
                Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_),
            ) => {}
            ("timeoutSecs", v) => {
                return Err(crate::types::wrong_types(
                    &format!("{SHUTDOWN}.timeoutSecs"),
                    v,
                    &TIMEOUT_TYPES,
                ));
            }
            (k, _) if generic(k) => {}
            (k, _) => return Err(unknown_field(SHUTDOWN, k)),
        }
    }
    ctx.backend.request_shutdown()?;
    Ok(Document::new().with("ok", 1.0))
}
