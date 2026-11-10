//! The profiler (010 FR-015): `profile` sets each database's level (0, 1 or 2) and filter and
//! the node's `slowms`, `slowinprogms` and `sampleRate`, as a `refnode` 9.0.4 does (measured on a
//! shard; a `refrouter` refuses levels 1 and 2 because it holds no data, and every TeselaDB node
//! holds data). After each command, a profiled database gets a document in its
//! `system.profile` (capped at 1 MiB, created on first use) with the operation, and a command
//! slower than `slowms` writes a «Slow query» line in the node's log.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::plan_exec::PLANNING_TIME;
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

impl Profiler {
    /// Milliseconds up to which an operation leaves nothing to the profiler or to the slow
    /// query log: [`Profiler::slowms`] while no database is profiled or has a filter, `-1`
    /// otherwise.
    pub fn quiet_ms(&self) -> i64 {
        let watched = self.levels.values().any(|l| *l > 0) || !self.filters.is_empty();
        if watched { -1 } else { self.slowms }
    }
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
    let quiet = p.quiet_ms();
    g.hot.profiler_quiet_ms.store(quiet, std::sync::atomic::Ordering::Relaxed);
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
    /// The CPU time its thread used, in nanoseconds, when the node can tell.
    pub cpu_nanos: Option<u64>,
    /// What it read and the plan of its read.
    pub report: crate::ops::OpReport,
    /// The client's address and application.
    pub client: Option<&'a str>,
    /// `client.application.name`.
    pub app_name: Option<&'a str>,
    /// Its user, as `user@db`.
    pub user: Option<String>,
    /// Its operation id (`opid`), as `currentOp` and `killOp` know it.
    pub opid: u32,
}

/// The commands the profiler never records: itself, the handshake, and those that carry a
/// password (a `refnode` 9.0.4 profiles no `createUser` nor `updateUser`, measured 01-11-2026).
const NOT_PROFILED: [&str; 9] = [
    PROFILE,
    "hello",
    "isMaster",
    "ismaster",
    "createUser",
    "updateUser",
    "authenticate",
    "saslStart",
    "saslContinue",
];

/// The commands whose `pwd` the slow query log shows as [`HIDDEN_PASSWORD`], as a `refnode`
/// 9.0.4 writes them.
const WITH_PASSWORD: [&str; 2] = ["createUser", "updateUser"];
const HIDDEN_PASSWORD: &str = "xxx";

/// The commands of op `command` that read documents.
const READING_COMMANDS: [&str; 5] =
    ["aggregate", "count", "distinct", "findAndModify", "findandmodify"];

/// Whether operation `op` of command `name` reads documents, and so has the counters and plan of
/// its read.
fn reads(op: &str, name: &str) -> bool {
    match op {
        "query" | "getmore" | "update" | "remove" => true,
        "command" => READING_COMMANDS.contains(&name),
        _ => false,
    }
}

/// Command `cmd` as the slow query log writes it, without its password.
fn logged_command(cmd: &Document) -> Document {
    let name = cmd.iter().next().map_or("", |(k, _)| k);
    if !WITH_PASSWORD.contains(&name) {
        return cmd.clone();
    }
    let mut out = Document::new();
    for (k, v) in cmd.iter() {
        let v = if k == "pwd" { Value::from(HIDDEN_PASSWORD) } else { v.clone() };
        out.push(k, v);
    }
    out
}

/// What the profiler shows of the read of `e`, in the order of a `refnode` 9.0.4 (measured
/// 01-11-2026): the hashes of its shape, then after the times its `planSummary` and the
/// `execStats` of the plan as it ran. A read the planner left to a collection scan shows a
/// `COLLSCAN` with the documents it read.
struct ReadParts {
    hashes: Document,
    summary: Option<Value>,
    stats: Option<Value>,
}

fn read_parts(e: &Ended<'_>, returned: u64) -> ReadParts {
    let mut parts = ReadParts { hashes: Document::new(), summary: None, stats: None };
    let Some(plan) = &e.report.plan else {
        if e.report.docs > 0 {
            let r =
                super::explain::collscan_report(e.ns, &Document::new(), e.report.docs, returned);
            parts.summary = r.get("planSummary").cloned();
            parts.stats = r.get("execStats").cloned();
        }
        return parts;
    };
    for k in ["planCacheShapeHash", "queryHash", "planCacheKey"] {
        if let Some(v) = plan.get(k) {
            parts.hashes.push(k, v.clone());
        }
    }
    match (plan.get("planSummary"), plan.get("filter")) {
        (Some(summary), _) => {
            parts.summary = Some(summary.clone());
            parts.stats = plan.get("execStats").cloned();
        }
        (None, Some(Value::Document(filter))) => {
            let r = super::explain::collscan_report(e.ns, filter, e.report.docs, returned);
            parts.summary = r.get("planSummary").cloned();
            parts.stats = r.get("execStats").cloned();
        }
        (None, _) => {}
    }
    parts
}

/// How the planner chose the plan `plan` of a read, as a `refnode` 9.0.4 writes it after
/// `docsExamined` (measured 02-11-2026): `fromMultiPlanner` when plans competed, then
/// `replanned` and `replanReason` when the cached plan was dropped, or `fromPlanCache` when the
/// cached plan ran; a `getMore` repeats its cursor's.
fn push_planner(d: &mut Document, plan: Option<&Document>) {
    let Some(plan) = plan else { return };
    let yes = |k: &str| plan.get(k) == Some(&Value::Boolean(true));
    if yes(super::plan_exec::MULTI_PLANNED) {
        d.push("fromMultiPlanner", true);
    }
    if let Some(Value::String(reason)) = plan.get(super::plan_exec::REPLANNED) {
        d.push("replanned", true);
        d.push("replanReason", reason.clone());
    }
    if yes(super::plan_exec::FROM_CACHE) {
        d.push("fromPlanCache", true);
    }
}

/// The counters of what `e` read: the planner's for a plan of an index, the operation's own
/// reads otherwise. A write by `_id` reads its documents in their range while it writes them,
/// by their key, which is their `_id`: each document it matched is one key and one document
/// examined, as the `EXPRESS` plan of a `refnode` 9.0.4 counts them (measured).
fn examined(e: &Ended<'_>) -> (Value, Value) {
    let own = |n: u64| Value::Int64(i64::try_from(n).unwrap_or(i64::MAX));
    // A `getMore` carries its cursor's plan, whose counters are those of the command that
    // opened it.
    let planned = e.report.plan.as_ref().filter(|_| e.op != "getmore");
    if let Some(p) = planned.filter(|p| p.get("planSummary").is_some()) {
        return (
            p.get("keysExamined").cloned().unwrap_or(own(0)),
            p.get("docsExamined").cloned().unwrap_or(own(0)),
        );
    }
    let (keys, docs) = (e.report.keys, e.report.docs);
    if keys + docs == 0
        && matches!(e.op, "update" | "remove")
        && let Some(Ok(reply)) = e.reply
        && let Some(n) = reply.get("n").and_then(crate::types::number)
    {
        let n = own(n as u64);
        return (n.clone(), n);
    }
    (own(keys), own(docs))
}

/// The document of `e` as `system.profile` and the slow query log write it, with the fields of a
/// `refnode` 9.0.4 that TeselaDB has (measured 01-11-2026). `locks`, `flowControl`, `queues`,
/// `planRanker` and `isFromPriorityPortConnection` are RefEngine's and the classic engine's and
/// have no counterpart here (DV-038).
fn describe(e: &Ended<'_>, now_ms: i64) -> Document {
    let mut d =
        Document::new().with("op", e.op).with("ns", e.ns).with("command", e.command.clone());
    if let Some(Value::Document(origin)) = origin(e) {
        d.push(super::query::ORIGINATING_COMMAND, origin.clone());
    }
    d.push("opid", i32::try_from(e.opid).unwrap_or(i32::MAX));
    if let Some(id) = cursor_id(e) {
        d.push("cursorid", id);
    }
    let reply = match e.reply {
        Some(Ok(r)) => Some(r),
        _ => None,
    };
    let cursor = match reply.and_then(|r| r.get("cursor")) {
        Some(Value::Document(c)) => Some(c),
        _ => None,
    };
    let returned =
        cursor.and_then(|c| c.get("firstBatch").or_else(|| c.get("nextBatch"))).map(|b| match b {
            Value::Array(items) => items.len(),
            _ => 0,
        });
    let name = e.command.iter().next().map_or("", |(k, _)| k);
    let reading = reads(e.op, name);
    let parts = read_parts(e, returned.unwrap_or(0) as u64);
    if reading {
        let (keys, docs) = examined(e);
        d.push("keysExamined", keys);
        d.push("docsExamined", docs);
        push_planner(&mut d, e.report.plan.as_ref());
        // TeselaDB answers each `find` and `getMore` in one batch of its own.
        if matches!(e.op, "query" | "getmore") {
            d.push("nBatches", 1);
        }
    }
    if let Some(c) = cursor {
        d.push("cursorExhausted", matches!(c.get("id"), Some(Value::Int64(0))));
    }
    if let Some(r) = reply {
        match (e.op, r.get("n")) {
            ("insert", Some(n)) => d.push("ninserted", n.clone()),
            ("remove", Some(n)) => d.push("ndeleted", n.clone()),
            ("update", Some(n)) => d.push("nMatched", n.clone()),
            _ => {}
        }
        if let Some(n) = r.get("nModified") {
            d.push("nModified", n.clone());
        }
        push_key_counts(&mut d, e, r);
    }
    if reading {
        // TeselaDB never yields an operation.
        d.push("numYield", 0);
    }
    if let Some(n) = returned {
        d.push("nreturned", i32::try_from(n).unwrap_or(i32::MAX));
    }
    for (k, v) in parts.hashes.iter() {
        d.push(k, v.clone());
    }
    if let Some(framework) = query_framework(e, name) {
        d.push("queryFramework", framework);
    }
    if let Some(r) = reply {
        let size = r.to_bytes().map_or(0, |b| b.len());
        d.push("responseLength", i32::try_from(size).unwrap_or(i32::MAX));
    }
    if let Some(Err(err)) = e.reply {
        d.push("ok", 0.0);
        d.push("errMsg", err.errmsg.clone());
        d.push("errName", err.code.name);
        d.push("errCode", err.code.code);
    }
    d.push("protocol", "op_msg");
    if let Some(cpu) = e.cpu_nanos {
        d.push("cpuNanos", i64::try_from(cpu).unwrap_or(i64::MAX));
    }
    d.push("millis", i64::try_from(e.micros / 1000).unwrap_or(i64::MAX));
    d.push("micros", i64::try_from(e.micros).unwrap_or(i64::MAX));
    if let Some(summary) = parts.summary {
        d.push("planSummary", summary);
    }
    if plans(e.op, name) {
        // A read or write the planner had nothing to choose for spent no time planning.
        let t = e.report.plan.as_ref().and_then(|p| p.get(PLANNING_TIME));
        d.push(PLANNING_TIME, t.cloned().unwrap_or(Value::Int64(0)));
    }
    if let Some(stats) = parts.stats {
        d.push("execStats", stats);
    }
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

/// `nUpserted`, `keysInserted` and `keysDeleted` of a write that replied `r`, as a `refnode`
/// 9.0.4 writes them (measured 01-11-2026): an insert its keys inserted, a delete its keys
/// deleted, an update and a `findAndModify` how many documents they upserted and the keys
/// inserted, and the keys deleted unless they upserted.
fn push_key_counts(d: &mut Document, e: &Ended<'_>, r: &Document) {
    let (inserted, deleted) = e.report.keys_written;
    let long = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    // The counts show only when a document was written, also when its keys were none (measured
    // on a `refnode` 9.0.4 on 01-11-2026 and 03-11-2026).
    let positive = |v: Option<&Value>| v.and_then(crate::types::number).is_some_and(|n| n > 0.0);
    let modify = e.command.iter().next().is_some_and(|(k, _)| FIND_AND_MODIFY.contains(&k));
    match e.op {
        "insert" if positive(r.get("n")) => d.push("keysInserted", long(inserted)),
        "remove" if positive(r.get("n")) => d.push("keysDeleted", long(deleted)),
        "update" | "command" if e.op == "update" || modify => {
            let last = match r.get("lastErrorObject") {
                Some(Value::Document(l)) => Some(l),
                _ => None,
            };
            let upserted = match (r.get("upserted"), last) {
                (Some(Value::Array(a)), _) => a.len(),
                (_, Some(l)) => usize::from(l.get("upserted").is_some()),
                _ => 0,
            };
            d.push("nUpserted", i64::try_from(upserted).unwrap_or(i64::MAX));
            if upserted > 0
                || positive(r.get("nModified"))
                || positive(last.and_then(|l| l.get("n")))
            {
                d.push("keysInserted", long(inserted));
                if upserted == 0 {
                    d.push("keysDeleted", long(deleted));
                }
            }
        }
        _ => {}
    }
}

/// Whether an operation `op` of command `name` plans its read, so that the profiler shows how
/// long that took (`planningTimeMicros`): `find`, the writes but `insert`, and the commands that
/// read through a query (measured on a `refnode` 9.0.4, 01-11-2026).
fn plans(op: &str, name: &str) -> bool {
    match op {
        "query" | "update" | "remove" => true,
        "command" => {
            FIND_AND_MODIFY.contains(&name) || matches!(name, "aggregate" | "distinct" | "count")
        }
        _ => false,
    }
}

/// The names of `findAndModify`.
const FIND_AND_MODIFY: [&str; 2] = ["findAndModify", "findandmodify"];

/// The framework of a read that profiles one (`queryFramework`): the slot-based engine for a
/// pipeline whose query plan it runs, the classic engine for the other reads with a cursor and
/// `distinct`; none for a write (measured on a `refnode` 9.0.4, 01-11-2026).
fn query_framework(e: &Ended<'_>, name: &str) -> Option<&'static str> {
    let command = match origin(e) {
        Some(Value::Document(o)) => o,
        _ => e.command,
    };
    let name = command.iter().next().map_or(name, |(k, _)| k);
    match (e.op, name) {
        (_, "aggregate") => {
            let stages: Vec<Document> = match command.get("pipeline") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Document(d) => Some(d.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(super::explain::aggregate::query_framework(&stages))
        }
        ("query" | "getmore", _) | (_, "distinct" | "count") => Some("classic"),
        _ => None,
    }
}

/// The command that opened the cursor a `getMore` read.
fn origin<'a>(e: &'a Ended<'_>) -> Option<&'a Value> {
    e.report.plan.as_ref()?.get(super::query::ORIGINATING_COMMAND)
}

/// Whether `doc` matches filter `f`.
fn matches(f: &Document, doc: &Document) -> bool {
    super::catalog::passes(Some(f), doc).unwrap_or(false)
}

/// After command `e`: profiles it when its database's level and filter (or `slowms` and
/// `sampleRate`) say so, and logs it when it is slow.
pub fn after_command<B: Backend + ?Sized>(backend: &mut B, shared: &super::Shared, e: &Ended<'_>) {
    let name = e.command.iter().next().map_or("", |(k, _)| k);
    keep_origin(shared, e);
    // Nothing of Queryable Encryption is profiled or logged: its payloads and tokens (017
    // FR-008, measured on a `refnode` 9.0.4).
    if super::fle::is_secret(e.command) {
        return;
    }
    // A password is never profiled; the slow query log hides it.
    if NOT_PROFILED.contains(&name) && !WITH_PASSWORD.contains(&name) {
        return;
    }
    // Nothing profiled and not slow: nothing to read under the node's lock (H-013).
    let quiet = super::hot::of(shared).profiler_quiet_ms.load(std::sync::atomic::Ordering::Relaxed);
    if i64::try_from(e.micros / 1000).unwrap_or(i64::MAX) <= quiet {
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
    // The description is built only when a filter or the profiler reads it.
    let doc = (filter.is_some() || level > 0).then(|| describe(e, now));
    let wanted = match (&filter, &doc) {
        (Some(f), Some(doc)) => matches(f, doc),
        _ => millis > slowms && sampled,
    };
    if wanted {
        let attr = Document::new()
            .with("type", "command")
            .with("ns", e.ns)
            .with("command", logged_command(e.command))
            .with("durationMillis", millis);
        backend.log_slow_query(&attr);
    }
    if NOT_PROFILED.contains(&name) {
        return;
    }
    let profiled = match level {
        2 => match (&filter, &doc) {
            (Some(f), Some(doc)) => matches(f, doc),
            _ => true,
        },
        1 => wanted,
        _ => false,
    };
    // Profiling its own collection would never end.
    if profiled
        && !e.ns.ends_with(SYSTEM_PROFILE)
        && let Some(doc) = doc
    {
        // The profiler records what it can: a failed write of the profile is not the command's.
        let _ = write(backend, shared, e.db, doc);
    }
}

/// The id of the cursor `e` opened and left open, or of the cursor a `getMore` read.
fn cursor_id(e: &Ended<'_>) -> Option<i64> {
    if e.op == "getmore" {
        return match e.command.iter().next() {
            Some((_, Value::Int64(id))) => Some(*id),
            _ => None,
        };
    }
    let Some(Ok(reply)) = e.reply else { return None };
    match reply.get("cursor") {
        Some(Value::Document(c)) => match c.get("id") {
            Some(Value::Int64(id)) if *id != 0 => Some(*id),
            _ => None,
        },
        _ => None,
    }
}

/// Keeps with the cursor `e` left open the command and its plan, which the profile of each
/// `getMore` of it shows (measured on a `refnode` 9.0.4, 01-11-2026).
/// Whether what runs after a command that took `micros` and answered `reply` reads its plan:
/// the profiler and the slow query log past the quiet threshold, and a cursor that stays open,
/// which keeps it for its `getMore`s ([`after_command`]).
pub fn reads_plan(
    shared: &super::Shared,
    micros: u64,
    reply: Option<&Result<Document, CommandError>>,
) -> bool {
    let quiet = super::hot::of(shared).profiler_quiet_ms.load(std::sync::atomic::Ordering::Relaxed);
    if i64::try_from(micros / 1000).unwrap_or(i64::MAX) > quiet {
        return true;
    }
    let Some(Ok(reply)) = reply else { return false };
    matches!(
        reply.get("cursor"),
        Some(Value::Document(c)) if matches!(c.get("id"), Some(Value::Int64(id)) if *id != 0)
    )
}

fn keep_origin(shared: &super::Shared, e: &Ended<'_>) {
    if e.op == "getmore" {
        return;
    }
    let Some(id) = cursor_id(e) else { return };
    let mut g = lock(shared);
    if let Some(c) = g.cursors.get_mut(id) {
        let origin =
            super::query::Origin { command: e.command.clone(), plan: e.report.plan.clone() };
        c.state.origin = Some(Box::new(origin));
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
