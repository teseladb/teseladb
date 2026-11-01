//! The profiler (010 FR-015): `profile` sets each database's level (0, 1 or 2) and filter and
//! the node's `slowms`, `slowinprogms` and `sampleRate`, as a `refnode` 9.0.4 does (measured on a
//! shard; a `refrouter` refuses levels 1 and 2 because it holds no data, and every TeselaDB node
//! holds data). After each command, a profiled database gets a document in its
//! `system.profile` (capped at 1 MiB, created on first use) with the operation, and a command
//! slower than `slowms` writes a «Slow query» line in the node's log.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Backend, Ctx, lock};
use crate::error::{BAD_VALUE, CommandError};
use crate::types::{number, wrong_types};

/// The command's name.
pub const PROFILE: &str = "profile";
/// The profiler's collection.
pub const SYSTEM_PROFILE: &str = "system.profile";
/// The size of a `system.profile` the profiler creates (measured: 1 MiB).
const PROFILE_SIZE: i64 = 1_048_576;
/// RefDb's identifier of a «Slow query» log line.
pub const SLOW_QUERY_ID: i32 = 51803;
/// The note of a reply when a filter decides what is profiled and logged.
const FILTER_NOTE: &str = "When a filter expression is set, slowms and sampleRate are not used for profiling and slow-query log lines.";
/// The numeric types `profile`'s fields take, in RefDb's order.
const NUMBER_TYPES: [&str; 4] = ["int", "double", "decimal", "long"];
/// The `filter` that removes the database's filter.
const UNSET: &str = "unset";

/// The profiler's settings on the node.
#[derive(Clone, Debug)]
pub struct Profiler {
    /// Milliseconds above which an operation is slow.
    pub slowms: i64,
    /// Milliseconds above which `currentOp` calls an operation slow.
    pub slowinprogms: i64,
    /// The share of slow operations profiled and logged.
    pub sample_rate: f64,
    /// Each database's level, 0 when absent.
    levels: BTreeMap<String, i32>,
    /// Each database's filter, as RefDb writes it back.
    filters: BTreeMap<String, Document>,
}

impl Default for Profiler {
    fn default() -> Profiler {
        Profiler {
            slowms: 100,
            slowinprogms: 5000,
            sample_rate: 1.0,
            levels: BTreeMap::new(),
            filters: BTreeMap::new(),
        }
    }
}

impl Profiler {
    /// The level of database `db`.
    pub fn level(&self, db: &str) -> i32 {
        self.levels.get(db).copied().unwrap_or(0)
    }
}

/// A number field of `profile`, truncated toward zero.
fn whole(v: &Value, field: &str) -> Result<i64, CommandError> {
    match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            Ok(number(v).map_or(0, |x| x as i64))
        }
        other => Err(wrong_types(&format!("{PROFILE}.{field}"), other, &NUMBER_TYPES)),
    }
}

/// A filter of `profile`, as RefDb parses it and writes it back.
fn read_filter(v: &Value) -> Result<Option<Document>, CommandError> {
    match v {
        Value::String(s) if s == UNSET => Ok(None),
        Value::Document(d) => {
            if d.iter().any(|(k, _)| k == "$where") {
                return Err(CommandError::new(BAD_VALUE, "$where is not allowed in this context"));
            }
            super::explain::parsed(d, None).map(Some)
        }
        _ => Err(CommandError::new(BAD_VALUE, "Expected an object, or the string 'unset'.")),
    }
}

/// `profile`: the database's level and filter and the node's thresholds before the change, and
/// the change.
pub fn profile<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let level = whole(ctx.first(), PROFILE)?;
    let slowms = ctx.cmd.get("slowms").map(|v| whole(v, "slowms")).transpose()?;
    let slowinprogms = ctx.cmd.get("slowinprogms").map(|v| whole(v, "slowinprogms")).transpose()?;
    let sample_rate = match ctx.cmd.get("sampleRate") {
        None => None,
        Some(v @ (Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))) => {
            let r = number(v).unwrap_or(f64::NAN);
            if !(0.0..=1.0).contains(&r) {
                let msg = "'sampleRate' must be between 0.0 and 1.0 inclusive";
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            Some(r)
        }
        Some(other) => {
            return Err(wrong_types(&format!("{PROFILE}.sampleRate"), other, &NUMBER_TYPES));
        }
    };
    let filter = ctx.cmd.get("filter").map(read_filter).transpose()?;
    let db = ctx.db.to_owned();
    let mut g = lock(ctx.shared);
    let p = &mut g.profiler;
    let old_filter = p.filters.get(&db).cloned();
    let mut reply = Document::new()
        .with("was", p.level(&db))
        .with("slowms", i32::try_from(p.slowms).unwrap_or(i32::MAX))
        .with("slowinprogms", p.slowinprogms)
        .with("sampleRate", p.sample_rate);
    let noted = old_filter.is_some() || matches!(filter, Some(Some(_)));
    if let Some(f) = old_filter {
        reply.push("filter", f);
    }
    if noted {
        reply.push("note", FILTER_NOTE);
    }
    if let Ok(l @ 0..=2) = i32::try_from(level) {
        p.levels.insert(db.clone(), l);
    }
    if let Some(ms) = slowms {
        p.slowms = ms;
    }
    if let Some(ms) = slowinprogms {
        p.slowinprogms = ms;
    }
    if let Some(r) = sample_rate {
        p.sample_rate = r;
    }
    match filter {
        Some(Some(f)) => {
            p.filters.insert(db, f);
        }
        Some(None) => {
            p.filters.remove(&db);
        }
        None => {}
    }
    Ok(reply.with("ok", 1.0))
}

/// What the profiler knows of a command that ended.
pub struct Ended<'a> {
    /// Its database.
    pub db: &'a str,
    /// The command.
    pub command: &'a Document,
    /// Its `op` and namespace, as `currentOp` shows them.
    pub op: &'a str,
    /// Its namespace.
    pub ns: &'a str,
    /// Its reply, or its error.
    pub reply: Option<&'a Result<Document, CommandError>>,
    /// How long it ran, in microseconds.
    pub micros: u64,
    /// The client's address and application.
    pub client: Option<&'a str>,
    /// `client.application.name`.
    pub app_name: Option<&'a str>,
    /// Its user, as `user@db`.
    pub user: Option<String>,
}

/// The commands the profiler never records: itself and the handshake.
const NOT_PROFILED: [&str; 4] = [PROFILE, "hello", "isMaster", "ismaster"];

/// The document of `e` as `system.profile` and the slow query log write it.
fn describe(e: &Ended<'_>, now_ms: i64) -> Document {
    let mut d =
        Document::new().with("op", e.op).with("ns", e.ns).with("command", e.command.clone());
    if let Some(Ok(reply)) = e.reply {
        if let Some(Value::Document(c)) = reply.get("cursor") {
            let batch = c.get("firstBatch").or_else(|| c.get("nextBatch"));
            if let Some(Value::Array(items)) = batch {
                d.push("nreturned", items.len() as i32);
            }
            let exhausted = matches!(c.get("id"), Some(Value::Int64(0)));
            d.push("cursorExhausted", exhausted);
        }
        match (e.op, reply.get("n")) {
            ("insert", Some(n)) => d.push("ninserted", n.clone()),
            ("remove", Some(n)) => d.push("ndeleted", n.clone()),
            ("update", Some(n)) => d.push("nMatched", n.clone()),
            _ => {}
        }
        if let Some(n) = reply.get("nModified") {
            d.push("nModified", n.clone());
        }
        let size = reply.to_bytes().map_or(0, |b| b.len());
        d.push("responseLength", i32::try_from(size).unwrap_or(i32::MAX));
    }
    if let Some(Err(err)) = e.reply {
        d.push("ok", 0.0);
        d.push("errMsg", err.errmsg.clone());
        d.push("errName", err.code.name);
        d.push("errCode", err.code.code);
    }
    d.push("protocol", "op_msg");
    d.push("millis", i64::try_from(e.micros / 1000).unwrap_or(i64::MAX));
    d.push("micros", i64::try_from(e.micros).unwrap_or(i64::MAX));
    d.push("ts", Value::DateTime(now_ms));
    d.push("client", e.client.unwrap_or_default());
    if let Some(a) = e.app_name {
        d.push("appName", a);
    }
    let users: Vec<Value> = e
        .user
        .iter()
        .filter_map(|u| u.split_once('@'))
        .map(|(user, db)| Value::Document(Document::new().with("user", user).with("db", db)))
        .collect();
    d.push("allUsers", users);
    d.push("user", e.user.clone().unwrap_or_default());
    d
}

/// Whether `doc` matches filter `f`.
fn matches(f: &Document, doc: &Document) -> bool {
    super::catalog::passes(Some(f), doc).unwrap_or(false)
}

/// After command `e`: profiles it when its database's level and filter (or `slowms` and
/// `sampleRate`) say so, and logs it when it is slow.
pub fn after_command<B: Backend + ?Sized>(backend: &mut B, shared: &super::Shared, e: &Ended<'_>) {
    let name = e.command.iter().next().map_or("", |(k, _)| k);
    if NOT_PROFILED.contains(&name) {
        return;
    }
    let (level, filter, slowms, rate) = {
        let g = lock(shared);
        let p = &g.profiler;
        (p.level(e.db), p.filters.get(e.db).cloned(), p.slowms, p.sample_rate)
    };
    let millis = i64::try_from(e.micros / 1000).unwrap_or(i64::MAX);
    let sampled = rate >= 1.0 || (backend.random() as f64 / u64::MAX as f64) < rate;
    let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
    let doc = describe(e, now);
    let wanted = match &filter {
        Some(f) => matches(f, &doc),
        None => millis > slowms && sampled,
    };
    if wanted {
        let attr = Document::new()
            .with("type", "command")
            .with("ns", e.ns)
            .with("command", e.command.clone())
            .with("durationMillis", millis);
        backend.log_slow_query(&attr);
    }
    let profiled = match level {
        2 => filter.as_ref().is_none_or(|f| matches(f, &doc)),
        1 => wanted,
        _ => false,
    };
    // Profiling its own collection would never end.
    if profiled && !e.ns.ends_with(SYSTEM_PROFILE) {
        // The profiler records what it can: a failed write of the profile is not the command's.
        let _ = write(backend, shared, e.db, doc);
    }
}

/// Inserts `doc` in `db.system.profile`, created capped at 1 MiB if it does not exist.
fn write<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    db: &str,
    doc: Document,
) -> Result<(), CommandError> {
    let ns = super::names::ns(db, SYSTEM_PROFILE);
    if backend.collection(&ns)?.is_none() {
        let create = Document::new()
            .with("create", SYSTEM_PROFILE)
            .with("capped", true)
            .with("size", PROFILE_SIZE);
        let mut ctx = Ctx { backend, shared, db, cmd: &create };
        super::catalog::create(&mut ctx)?;
    }
    let cmd = Document::new().with("insert", SYSTEM_PROFILE);
    let mut ctx = Ctx { backend, shared, db, cmd: &cmd };
    super::crud::insert_internal(&mut ctx, &ns, vec![doc])
}
