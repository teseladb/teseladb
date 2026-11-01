//! The cluster's write gates (007 FR-029): `fsync` with `lock` and `fsyncUnlock` count the locks
//! that hold every write back, and `setUserWriteBlockMode` refuses the writes to user databases,
//! as on a `refrouter` 9.0.4 (measured). Both live in Group 0 as cluster parameters that only this
//! module reads and writes, so every node sees the same gate.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::space::KeyParts;

use super::admin::{ADMIN_DB, admin_only};
use super::config_db::CONFIG_DB;
use super::mutation::Bound;
use super::names::LOCAL_DB;
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, CommandError, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, ILLEGAL_OPERATION,
    MAX_TIME_MS_EXPIRED, USER_WRITES_BLOCKED,
};

/// `fsyncUnlock`.
pub const FSYNC_UNLOCK: &str = "fsyncUnlock";
/// `setUserWriteBlockMode`.
pub const SET_USER_WRITE_BLOCK_MODE: &str = "setUserWriteBlockMode";
/// The cluster parameter with the count of `fsync` locks (never listed by `getClusterParameter`).
pub const FSYNC_LOCKS: &str = "fsyncLock";
/// The cluster parameter that says whether user writes are blocked.
pub const USER_WRITE_BLOCK: &str = "userWriteBlockMode";
/// The field of both parameters' documents.
const VALUE: &str = "value";
/// `setUserWriteBlockMode.global`.
const GLOBAL: &str = "global";
/// `setUserWriteBlockMode.reason`.
const REASON: &str = "reason";
/// The reasons `setUserWriteBlockMode` takes (measured).
const REASONS: [&str; 3] =
    ["Unspecified", "DiskUseThresholdExceeded", "ClusterToClusterMigrationInProgress"];
/// What a blocked write of a document answers: a `refrouter` 9.0.4 names no other reason, whatever
/// `reason` said (measured).
pub const BLOCKED_WRITE: &str = "User writes blocked, reason: Unspecified";
/// What a blocked change of the catalog answers (measured on `drop`).
pub const BLOCKED_DDL: &str = "User writes blocked";
const LOCKED_INFO: &str = "now locked against writes, use db.fsyncUnlock() to unlock";
const SEE_ALSO: &str = "http://dochub.refdb.org/core/fsynccommand";
const UNLOCKED_INFO: &str = "fsyncUnlock completed";
const NOT_LOCKED: &str = "fsyncUnlock called when not locked";

/// The gates as Group 0 has them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteGate {
    /// `fsync` locks not yet unlocked: every write waits while there is one.
    pub fsync_locks: u32,
    /// Writes to user databases fail.
    pub user_writes_blocked: bool,
}

fn stored(all: &BTreeMap<String, Vec<u8>>, name: &str) -> Option<Value> {
    let d = RawDoc::from_bytes(all.get(name)?).ok().map(Document::from)?;
    d.get(VALUE).cloned()
}

/// The gates among the cluster parameters `all`.
pub fn gate_of(all: &BTreeMap<String, Vec<u8>>) -> WriteGate {
    let fsync_locks = match stored(all, FSYNC_LOCKS) {
        Some(Value::Int64(n)) => u32::try_from(n).unwrap_or(u32::MAX),
        _ => 0,
    };
    let user_writes_blocked = matches!(stored(all, USER_WRITE_BLOCK), Some(Value::Boolean(true)));
    WriteGate { fsync_locks, user_writes_blocked }
}

/// The gates as Group 0 has them now, read from it: a count changed from this copy is never one
/// the node's gate has not seen yet.
fn current<B: Backend + ?Sized>(b: &mut B) -> Result<WriteGate, CommandError> {
    Ok(gate_of(&b.cluster_parameters()?))
}

/// Stores gate `name` with `v`, then waits until the node's gate is `want`, so that the writes
/// this node takes after the answer see it.
fn store<B: Backend + ?Sized>(
    b: &mut B,
    (name, v): (&str, Value),
    want: WriteGate,
) -> Result<(), CommandError> {
    b.set_cluster_parameter(name, Document::new().with(VALUE, v).to_bytes()?)?;
    while b.write_gate()? != want {
        if !b.txn_pause() {
            return Err(CommandError::new(MAX_TIME_MS_EXPIRED, crate::ops::TIME_LIMIT_MESSAGE));
        }
    }
    Ok(())
}

/// Whether `db` holds users' data: every database but `admin`, `config` and `local`.
fn user_db(db: &str) -> bool {
    ![ADMIN_DB, CONFIG_DB, LOCAL_DB].contains(&db)
}

/// Waits while an `fsync` lock holds the writes back; `MaxTimeMSExpired` once the operation's
/// time runs out.
fn unlocked<B: Backend + ?Sized>(b: &mut B) -> Result<WriteGate, CommandError> {
    loop {
        let g = b.write_gate()?;
        if g.fsync_locks == 0 {
            return Ok(g);
        }
        if !b.txn_pause() {
            return Err(CommandError::new(MAX_TIME_MS_EXPIRED, crate::ops::TIME_LIMIT_MESSAGE));
        }
    }
}

/// Lets write `bound` go: it waits for the locks, and fails if user writes are blocked, it is a
/// write to a collection of a user database and it changes something (a delete or an update
/// that matches nothing goes, as on a `refrouter` 9.0.4).
pub fn data<B: Backend + ?Sized>(b: &mut B, bound: &Bound) -> Result<(), CommandError> {
    if !unlocked(b)?.user_writes_blocked {
        return Ok(());
    }
    let keys = bound.latch_keys();
    let collection = keys.first().and_then(|k| match KeyParts::of(k) {
        Ok(KeyParts::Data { collection, .. }) => Some(collection),
        _ => None,
    });
    let Some(id) = collection else { return Ok(()) };
    // A collection already dropped has no database: its documents go (an internal purge).
    let user = b.collections()?.iter().any(|(ns, c)| c.id == id && user_db(&ns.db));
    // A write that cannot be evaluated here fails the same way when it runs.
    let changes = user
        && super::cross_range::evaluate(b, bound).map_or(true, |(_, writes, _)| !writes.is_empty());
    match changes {
        true => Err(CommandError::new(USER_WRITES_BLOCKED, BLOCKED_WRITE)),
        false => Ok(()),
    }
}

/// Lets a change of the catalog of database `db` go, as [`data`] does.
pub fn ddl<B: Backend + ?Sized>(b: &mut B, db: &str) -> Result<(), CommandError> {
    match unlocked(b)?.user_writes_blocked && user_db(db) {
        true => Err(CommandError::new(USER_WRITES_BLOCKED, BLOCKED_DDL)),
        false => Ok(()),
    }
}

/// The answer of every node, as a `refrouter` gives each shard's: `raw` by `<shard>/<host>`.
fn by_node<B: Backend + ?Sized>(b: &mut B, each: &Document) -> Result<Document, CommandError> {
    Ok(super::cluster_admin::raw_names(b)?
        .into_iter()
        .fold(Document::new(), |d, n| d.with(n, each.clone())))
}

/// `fsync` with `lock`: one more lock.
pub fn lock<B: Backend + ?Sized>(b: &mut B) -> Result<Document, CommandError> {
    let now = current(b)?;
    let count = now.fsync_locks.saturating_add(1);
    let want = WriteGate { fsync_locks: count, ..now };
    store(b, (FSYNC_LOCKS, Value::Int64(i64::from(count))), want)?;
    let each = Document::new()
        .with("info", LOCKED_INFO)
        .with("lockCount", i64::from(count))
        .with("seeAlso", SEE_ALSO)
        .with("ok", 1);
    let raw = by_node(b, &each)?;
    Ok(Document::new()
        .with("numFiles", 1)
        .with("all", Document::new().with("raw", raw))
        .with("ok", 1.0))
}

/// `fsyncUnlock`: one lock less.
pub fn fsync_unlock<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(FSYNC_UNLOCK, ctx.db)?;
    let b = &mut *ctx.backend;
    let now = current(b)?;
    let count = now.fsync_locks;
    if count == 0 {
        let each = Document::new()
            .with("ok", 0)
            .with("errmsg", NOT_LOCKED)
            .with("code", ILLEGAL_OPERATION.code)
            .with("codeName", ILLEGAL_OPERATION.name);
        let raw = by_node(b, &each)?;
        let e = CommandError::new(ILLEGAL_OPERATION, NOT_LOCKED);
        return Err(CommandError { extra: Document::new().with("raw", raw), ..e });
    }
    let want = WriteGate { fsync_locks: count - 1, ..now };
    store(b, (FSYNC_LOCKS, Value::Int64(i64::from(count - 1))), want)?;
    let each = Document::new()
        .with("info", UNLOCKED_INFO)
        .with("lockCount", i64::from(count - 1))
        .with("ok", 1);
    Ok(Document::new().with("raw", by_node(b, &each)?).with("ok", 1.0))
}

/// `setUserWriteBlockMode`: on any database (measured), `global` a bool, `reason` one of
/// [`REASONS`].
pub fn set_user_write_block_mode<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = SET_USER_WRITE_BLOCK_MODE;
    let mut global = None;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            (GLOBAL, Value::Boolean(on)) => global = Some(*on),
            (GLOBAL, v) => {
                return Err(crate::types::wrong_type(&format!("{name}.{GLOBAL}"), v, "bool"));
            }
            (REASON, Value::String(r)) if REASONS.contains(&r.as_str()) => {}
            (REASON, Value::String(r)) => {
                let msg = format!(
                    "Enumeration value '{r}' for field '{name}.{REASON}' is not a valid value."
                );
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            (REASON, v) => {
                return Err(crate::types::wrong_type(&format!("{name}.{REASON}"), v, "string"));
            }
            (k, _) if crate::generic::GENERIC_ARGUMENTS.contains(&k) => {}
            (k, _) => {
                let msg = format!("BSON field '{name}.{k}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    let Some(on) = global else {
        let msg = format!("BSON field '{name}.{GLOBAL}' is missing but a required field");
        return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
    };
    // It waits for an `fsync` lock as a write does (measured).
    unlocked(ctx.backend)?;
    let want = WriteGate { user_writes_blocked: on, ..current(ctx.backend)? };
    store(ctx.backend, (USER_WRITE_BLOCK, Value::Boolean(on)), want)?;
    Ok(Document::new().with("ok", 1.0))
}
