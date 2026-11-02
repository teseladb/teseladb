//! `$queryStats` (010 FR-023, measured on a `refrouter` 9.0.4, 01-11-2026 and 02-11-2026): what the
//! node counted of the queries it sampled, by query shape and the options that are not part of
//! the shape. An entry's `key` holds the debug shape, `comment`, the read concern, the write
//! concern of an `aggregate`, the client's metadata, `maxTimeMS` and the batch size, in that
//! order; its `keyHash` is the base64 SHA-256 of the key's representative form, and
//! `queryShapeHash` the shape's. A query counts once its cursor ends (exhausted or killed), with
//! all its batches: the runs, their times, CPU time, the documents returned and the index keys
//! and documents examined, each as a sum, a maximum, a minimum and a sum of squares, and when the
//! shape was first and last seen.
//!
//! Which queries count (measured): with `internalQueryStatsSampleRate` above 0, each with that
//! probability; at 0, all with `internalQueryStatsRateLimit` -1, none with 0, and at most that
//! many a second otherwise.

use std::collections::BTreeMap;

use teseladb_bson::{Decimal128, Document, Value};

use super::query_shape::literal::{Policy, literal};
use super::query_shape::{self, Shape};
use crate::concerns::{CLIENT_SUPPLIED, READ_CONCERN, WRITE_CONCERN};

/// The stage.
pub const STAGE: &str = "$queryStats";
/// The length of the rate limit's window, in milliseconds.
const RATE_WINDOW_MS: u64 = 1_000;
/// The commands whose key has a write concern (an `aggregate` may write, measured).
const WRITING: &str = "aggregate";

/// A sum, maximum, minimum and sum of squares of one metric.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Aggregate {
    sum: i64,
    max: i64,
    min: i64,
    squares: u128,
    seen: bool,
}

impl Aggregate {
    fn add(&mut self, v: i64) {
        self.sum = self.sum.saturating_add(v);
        self.max = if self.seen { self.max.max(v) } else { v };
        self.min = if self.seen { self.min.min(v) } else { v };
        let s = u128::from(v.unsigned_abs());
        self.squares = self.squares.saturating_add(s.saturating_mul(s));
        self.seen = true;
    }

    fn shown(&self) -> Document {
        let squares = Decimal128::from_parts(false, self.squares, 0)
            .map_or(Value::Int64(i64::MAX), Value::Decimal128);
        Document::new()
            .with("sum", self.sum)
            .with("max", self.max)
            .with("min", self.min)
            .with("sumOfSquares", squares)
    }
}

/// How many runs said yes and no to one question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    yes: i64,
    no: i64,
}

impl Tally {
    fn add(&mut self, v: bool) {
        match v {
            true => self.yes += 1,
            false => self.no += 1,
        }
    }

    fn shown(&self) -> Document {
        Document::new().with("true", self.yes).with("false", self.no)
    }
}

/// The counters of writes in an entry, none for a read (`metrics.writes`).
const WRITE_COUNTERS: [&str; 9] = [
    "nMatched",
    "nUpserted",
    "nModified",
    "nDeleted",
    "nInserted",
    "nUpdateOps",
    "nDeleteOps",
    "keysInserted",
    "keysDeleted",
];

/// The ways RefDb's cost-based ranker estimates, which TeselaDB's planner does not use.
const ESTIMATION_METHODS: [&str; 6] =
    ["Histogram", "Sampling", "Heuristics", "Mixed", "Metadata", "Code"];

/// What one query counted, over all its batches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    /// How long its batches took, in microseconds.
    pub micros: i64,
    /// How long its first batch took, in microseconds.
    pub first_micros: i64,
    /// The documents it returned.
    pub docs_returned: i64,
    /// The index keys it examined.
    pub keys_examined: i64,
    /// The documents it examined.
    pub docs_examined: i64,
    /// The CPU time of its batches, in nanoseconds; `None` where the node cannot tell.
    pub cpu_nanos: Option<i64>,
    /// What its planner did.
    pub planner: Planner,
}

/// What the planner did for one query (`metrics.queryPlanner`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Planner {
    /// Whether it sorted in memory or on disk, its plan reading in another order.
    pub has_sort_stage: bool,
    /// Whether a stage wrote to disk.
    pub used_disk: bool,
    /// Whether several plans competed for it.
    pub from_multi_planner: bool,
    /// Whether its plan came from the plan cache.
    pub from_plan_cache: bool,
    /// How long choosing its plan took, in microseconds.
    pub planning_micros: i64,
}

impl Run {
    /// The run with batch `more` after it.
    #[must_use]
    pub fn then(self, more: Run) -> Run {
        Run {
            micros: self.micros.saturating_add(more.micros),
            first_micros: self.first_micros,
            docs_returned: self.docs_returned.saturating_add(more.docs_returned),
            keys_examined: self.keys_examined.saturating_add(more.keys_examined),
            docs_examined: self.docs_examined.saturating_add(more.docs_examined),
            cpu_nanos: self.cpu_nanos.zip(more.cpu_nanos).map(|(a, b)| a.saturating_add(b)),
            planner: Planner {
                used_disk: self.planner.used_disk || more.planner.used_disk,
                ..self.planner
            },
        }
    }
}

/// The key of a query: its debug form and the hashes.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyed {
    key: Document,
    key_hash: String,
    shape_hash: String,
}

/// A sampled query whose cursor is still open, and what it counted so far.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// Its key.
    pub keyed: Keyed,
    /// What it counted.
    pub run: Run,
}

/// One entry.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    keyed: Keyed,
    last_micros: i64,
    count: i64,
    exec: Aggregate,
    first_response: Aggregate,
    docs_returned: Aggregate,
    keys_examined: Aggregate,
    docs_examined: Aggregate,
    cpu: Aggregate,
    working_ms: Aggregate,
    has_sort_stage: Tally,
    used_disk: Tally,
    from_multi_planner: Tally,
    from_plan_cache: Tally,
    planning: Aggregate,
    first_seen_ms: i64,
    last_seen_ms: i64,
}

impl Entry {
    /// `metrics.queryPlanner`: what the planner did, without the plan shapes RefDb counts
    /// (DV-080).
    fn planner(&self) -> Document {
        let mut methods = Document::new();
        for m in ESTIMATION_METHODS {
            methods.push(m, 0i64);
        }
        let ranker = Document::new()
            .with("cardinalityEstimationMethods", methods)
            .with("nDocsSampled", zeros(self.count));
        Document::new()
            .with("hasSortStage", self.has_sort_stage.shown())
            .with("usedDisk", self.used_disk.shown())
            .with("fromMultiPlanner", self.from_multi_planner.shown())
            .with("fromPlanCache", self.from_plan_cache.shown())
            .with("planningTimeMicros", self.planning.shown())
            .with("costBasedRanker", ranker)
    }
}

/// The aggregate of `runs` zeros.
fn zeros(runs: i64) -> Document {
    let mut a = Aggregate::default();
    if runs > 0 {
        a.add(0);
    }
    a.shown()
}

/// `metrics.writes` of a read: every counter zero.
fn writes() -> Document {
    let mut d = Document::new();
    for k in WRITE_COUNTERS {
        d.push(k, Aggregate::default().shown());
    }
    d
}

/// The entries of the node, by key hash, the sampled queries whose cursors are open, by cursor,
/// and the rate limit's window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryStats {
    entries: BTreeMap<String, Entry>,
    open: BTreeMap<i64, Pending>,
    window_start_ms: u64,
    in_window: u64,
}

/// The read concern of command `cmd` in its key: the client's, or the default.
fn read_concern(cmd: &Document) -> Value {
    match cmd.get(READ_CONCERN) {
        Some(Value::Document(rc)) => {
            Value::Document(rc.clone().with("provenance", CLIENT_SUPPLIED))
        }
        _ => Value::Document(crate::concerns::default_read_concern()),
    }
}

/// The write concern of command `cmd` in its key: the client's, or the default.
fn write_concern(cmd: &Document) -> Option<Value> {
    let wc = match crate::concerns::WriteConcernArgs::parse(cmd, WRITING) {
        Ok(Some(wc)) => wc,
        Ok(None) | Err(_) => crate::concerns::default_write_concern(),
    };
    Some(Value::Document(wc.to_document()))
}

/// The options of command `cmd` that are part of an entry's key, under policy `p`, in the
/// order of a `refrouter` 9.0.4 (measured).
fn options(cmd: &Document, client: Option<&Document>, p: Policy) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some(c) = cmd.get("comment") {
        out.push(("comment", literal(c, p)));
    }
    out.push((READ_CONCERN, read_concern(cmd)));
    if cmd.iter().next().is_some_and(|(name, _)| name == WRITING)
        && let Some(wc) = write_concern(cmd)
    {
        out.push((WRITE_CONCERN, wc));
    }
    if let Some(c) = client {
        out.push(("client", Value::Document(c.clone())));
    }
    if let Some(n) = cmd.get("maxTimeMS") {
        out.push(("maxTimeMS", literal(n, p)));
    }
    if let Some(Value::Document(c)) = cmd.get("cursor")
        && let Some(b) = c.get("batchSize")
    {
        out.push(("cursor", Value::Document(Document::new().with("batchSize", literal(b, p)))));
    }
    if let Some(n) = cmd.get("batchSize") {
        out.push(("batchSize", literal(n, p)));
    }
    out
}

/// The key of command `cmd` with shape form `shape`, sent by client `client`.
fn key(shape: Document, cmd: &Document, client: Option<&Document>, p: Policy) -> Document {
    let mut k = Document::new().with("queryShape", shape);
    for (name, v) in options(cmd, client, p) {
        k.push(name, v);
    }
    k
}

/// The key of command `cmd` on `db`, of shape `shape`, sent by the client of metadata `client`;
/// `None` for a command without a representative form.
pub fn keyed(db: &str, cmd: &Document, shape: &Shape, client: Option<&Document>) -> Option<Keyed> {
    let rep = query_shape::representative(db, cmd)?;
    let bytes = key(rep, cmd, client, Policy::Representative).to_bytes().ok()?;
    Some(Keyed {
        key: key(shape.debug.clone(), cmd, client, Policy::Debug),
        key_hash: teseladb_bson::extjson::base64::encode(&teseladb_hash::sha256(&bytes)),
        shape_hash: shape.hash.clone(),
    })
}

/// Whether a draw `random` in [0, 2^64) falls under probability `rate`.
fn drawn(random: u64, rate: f64) -> bool {
    const MANTISSA: u32 = 53;
    let unit = (random >> (64 - MANTISSA)) as f64 / (1u64 << MANTISSA) as f64;
    unit < rate
}

impl QueryStats {
    /// Whether the query starting at `now_ms` counts, under `internalQueryStatsRateLimit`
    /// `rate_limit` and `internalQueryStatsSampleRate` `sample_rate`, with draw `random`.
    pub fn sample(&mut self, rate_limit: i64, sample_rate: f64, random: u64, now_ms: u64) -> bool {
        if sample_rate > 0.0 {
            return drawn(random, sample_rate);
        }
        let Ok(limit) = u64::try_from(rate_limit) else { return rate_limit == -1 };
        if now_ms.saturating_sub(self.window_start_ms) >= RATE_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.in_window = 0;
        }
        if self.in_window >= limit {
            return false;
        }
        self.in_window += 1;
        true
    }

    /// Keeps query `pending` until its cursor `id` ends; the queries of cursors that are no
    /// longer `alive` (killed with their session, or by an error) count now, at `now_ms`.
    pub fn open(&mut self, id: i64, pending: Pending, alive: impl Fn(i64) -> bool, now_ms: i64) {
        let dead: Vec<i64> = self.open.keys().copied().filter(|c| !alive(*c)).collect();
        for c in dead {
            self.close(c, now_ms);
        }
        self.open.insert(id, pending);
    }

    /// Adds batch `more` to the query of cursor `id`, if it was sampled.
    pub fn more(&mut self, id: i64, more: Run) {
        if let Some(p) = self.open.get_mut(&id) {
            p.run = p.run.then(more);
        }
    }

    /// Counts the query of cursor `id`, which ended at `now_ms`, if it was sampled.
    pub fn close(&mut self, id: i64, now_ms: i64) {
        if let Some(p) = self.open.remove(&id) {
            self.record(p.keyed, p.run, now_ms);
        }
    }

    /// Counts query `keyed`, which counted `run`, at `now_ms`.
    pub fn record(&mut self, keyed: Keyed, run: Run, now_ms: i64) {
        let e = self.entries.entry(keyed.key_hash.clone()).or_insert_with(|| Entry {
            keyed,
            last_micros: 0,
            count: 0,
            exec: Aggregate::default(),
            first_response: Aggregate::default(),
            docs_returned: Aggregate::default(),
            keys_examined: Aggregate::default(),
            docs_examined: Aggregate::default(),
            cpu: Aggregate::default(),
            working_ms: Aggregate::default(),
            has_sort_stage: Tally::default(),
            used_disk: Tally::default(),
            from_multi_planner: Tally::default(),
            from_plan_cache: Tally::default(),
            planning: Aggregate::default(),
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
        });
        e.last_micros = run.micros;
        e.count += 1;
        e.exec.add(run.micros);
        e.first_response.add(run.first_micros);
        e.docs_returned.add(run.docs_returned);
        e.keys_examined.add(run.keys_examined);
        e.docs_examined.add(run.docs_examined);
        if let Some(n) = run.cpu_nanos {
            e.cpu.add(n);
        }
        e.working_ms.add(run.micros / 1000);
        e.has_sort_stage.add(run.planner.has_sort_stage);
        e.used_disk.add(run.planner.used_disk);
        e.from_multi_planner.add(run.planner.from_multi_planner);
        e.from_plan_cache.add(run.planner.from_plan_cache);
        e.planning.add(run.planner.planning_micros);
        e.last_seen_ms = now_ms;
    }

    /// The documents of `$queryStats`, as of `now_ms`.
    pub fn listed(&self, now_ms: i64) -> Vec<Document> {
        self.entries
            .values()
            .map(|e| {
                let query_exec = Document::new()
                    .with("docsReturned", e.docs_returned.shown())
                    .with("keysExamined", e.keys_examined.shown())
                    .with("docsExamined", e.docs_examined.shown());
                let mut metrics = Document::new()
                    .with("lastExecutionMicros", e.last_micros)
                    .with("execCount", e.count)
                    .with("totalExecMicros", e.exec.shown());
                if e.cpu.seen {
                    metrics.push("cpuNanos", e.cpu.shown());
                }
                let metrics = metrics
                    .with("workingTimeMillis", e.working_ms.shown())
                    .with(
                        "cursor",
                        Document::new().with("firstResponseExecMicros", e.first_response.shown()),
                    )
                    .with("queryExec", query_exec)
                    .with("queryPlanner", e.planner())
                    .with("writes", writes())
                    .with("firstSeenTimestamp", Value::DateTime(e.first_seen_ms))
                    .with("latestSeenTimestamp", Value::DateTime(e.last_seen_ms));
                Document::new()
                    .with("key", e.keyed.key.clone())
                    .with("keyHash", e.keyed.key_hash.as_str())
                    .with("queryShapeHash", e.keyed.shape_hash.as_str())
                    .with("metrics", metrics)
                    .with("asOf", Value::DateTime(now_ms))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_of_one_always_draws_and_zero_never() {
        assert!(drawn(u64::MAX, 1.0));
        assert!(!drawn(0, 0.0));
        assert!(drawn(0, 0.5));
        assert!(!drawn(u64::MAX, 0.5));
    }

    #[test]
    fn the_rate_limit_counts_per_second() {
        let mut s = QueryStats::default();
        assert!(s.sample(2, 0.0, 0, 5_000));
        assert!(s.sample(2, 0.0, 0, 5_100));
        assert!(!s.sample(2, 0.0, 0, 5_900));
        assert!(s.sample(2, 0.0, 0, 6_000));
        assert!(s.sample(-1, 0.0, 0, 6_000));
        assert!(!s.sample(0, 0.0, 0, 9_000));
        assert!(s.sample(0, 1.0, 0, 9_000));
    }

    #[test]
    fn batches_add_up_and_cpu_time_needs_every_batch() {
        let a = Run {
            micros: 5,
            first_micros: 5,
            docs_returned: 1,
            cpu_nanos: Some(7),
            ..Run::default()
        };
        let b = Run {
            micros: 3,
            docs_returned: 2,
            docs_examined: 4,
            cpu_nanos: Some(1),
            ..Run::default()
        };
        let r = a.then(b);
        assert_eq!((r.micros, r.first_micros, r.docs_returned, r.docs_examined), (8, 5, 3, 4));
        assert_eq!(r.cpu_nanos, Some(8));
        assert_eq!(a.then(Run::default()).cpu_nanos, None);
    }
}
