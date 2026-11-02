//! `$queryStats` (010 FR-023, measured on a `refrouter` 9.0.4, 01-11-2026): what the node counted
//! of each query it ran, by query shape and the options that are not part of the shape. An
//! entry's `key` holds the debug shape and those options; its `keyHash` is the base64 SHA-256 of
//! the key's representative form, and `queryShapeHash` the shape's. Its metrics give the runs,
//! their times and the documents they returned, each as a sum, a maximum, a minimum and a sum
//! of squares, and when the shape was first and last seen.

use std::collections::BTreeMap;

use teseladb_bson::{Decimal128, Document, Value};

use super::query_shape::literal::{Policy, literal};
use super::query_shape::{self, Shape};

/// The stage.
pub const STAGE: &str = "$queryStats";

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

/// What one run of a query counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    /// How long it took, in microseconds.
    pub micros: i64,
    /// The documents it returned in its first batch.
    pub docs_returned: i64,
}

/// One entry.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    key: Document,
    key_hash: String,
    shape_hash: String,
    last_micros: i64,
    count: i64,
    exec: Aggregate,
    first_response: Aggregate,
    docs_returned: Aggregate,
    first_seen_ms: i64,
    last_seen_ms: i64,
}

/// The entries of the node, by key hash.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryStats {
    entries: BTreeMap<String, Entry>,
}

/// The options of command `cmd` that are part of an entry's key, under policy `p`.
fn options(cmd: &Document, p: Policy) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some(rc @ Value::Document(_)) = cmd.get("readConcern") {
        out.push(("readConcern", rc.clone()));
    }
    if let Some(Value::Document(c)) = cmd.get("cursor")
        && let Some(b) = c.get("batchSize")
    {
        out.push(("cursor", Value::Document(Document::new().with("batchSize", literal(b, p)))));
    }
    if let Some(n) = cmd.get("batchSize") {
        out.push(("batchSize", literal(n, p)));
    }
    if let Some(n) = cmd.get("maxTimeMS") {
        out.push(("maxTimeMS", literal(n, p)));
    }
    if let Some(c) = cmd.get("comment") {
        out.push(("comment", literal(c, p)));
    }
    out
}

/// The key of command `cmd` with shape form `shape`.
fn key(shape: Document, cmd: &Document, p: Policy) -> Document {
    let mut k = Document::new().with("queryShape", shape);
    for (name, v) in options(cmd, p) {
        k.push(name, v);
    }
    k
}

impl QueryStats {
    /// Counts run `run` of command `cmd` on `db`, whose shape is `shape`, at `now_ms`.
    pub fn record(&mut self, db: &str, cmd: &Document, shape: &Shape, run: Run, now_ms: i64) {
        let Some(rep) = query_shape::representative(db, cmd) else { return };
        let rep_key = key(rep, cmd, Policy::Representative);
        let Ok(bytes) = rep_key.to_bytes() else { return };
        let key_hash = teseladb_bson::extjson::base64::encode(&teseladb_hash::sha256(&bytes));
        let e = self.entries.entry(key_hash.clone()).or_insert_with(|| Entry {
            key: key(shape.debug.clone(), cmd, Policy::Debug),
            key_hash,
            shape_hash: shape.hash.clone(),
            last_micros: 0,
            count: 0,
            exec: Aggregate::default(),
            first_response: Aggregate::default(),
            docs_returned: Aggregate::default(),
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
        });
        e.last_micros = run.micros;
        e.count += 1;
        e.exec.add(run.micros);
        e.first_response.add(run.micros);
        e.docs_returned.add(run.docs_returned);
        e.last_seen_ms = now_ms;
    }

    /// The documents of `$queryStats`, as of `now_ms`.
    pub fn listed(&self, now_ms: i64) -> Vec<Document> {
        self.entries
            .values()
            .map(|e| {
                let query_exec = Document::new().with("docsReturned", e.docs_returned.shown());
                let metrics = Document::new()
                    .with("lastExecutionMicros", e.last_micros)
                    .with("execCount", e.count)
                    .with("totalExecMicros", e.exec.shown())
                    .with(
                        "cursor",
                        Document::new().with("firstResponseExecMicros", e.first_response.shown()),
                    )
                    .with("queryExec", query_exec)
                    .with("firstSeenTimestamp", Value::DateTime(e.first_seen_ms))
                    .with("latestSeenTimestamp", Value::DateTime(e.last_seen_ms));
                Document::new()
                    .with("key", e.key.clone())
                    .with("keyHash", e.key_hash.as_str())
                    .with("queryShapeHash", e.shape_hash.as_str())
                    .with("metrics", metrics)
                    .with("asOf", Value::DateTime(now_ms))
            })
            .collect()
    }
}
