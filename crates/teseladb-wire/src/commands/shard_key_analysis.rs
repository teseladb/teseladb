//! `analyzeShardKey` (018 FR-017): the characteristics of a candidate shard key over the
//! collection's documents and how the sampled queries ([`super::query_analysis`]) would be
//! routed with it.
//!
//! - `keyCharacteristics` needs an index that starts with the key: the documents, their average
//!   size, the distinct values, the five most common ones (by frequency, then in key order) and
//!   the monotonicity, the correlation between the order of the key and the order in which the
//!   documents were inserted. TeselaDB keeps no record id, so the order of `_id` stands for it.
//!   The coefficient is rounded up to nine decimals and is monotonic from 0.7, as on a
//!   `refrouter` 9.0.4.
//! - `readWriteDistribution` cuts the key's values into 100 ranges at evenly spaced documents
//!   (a `refrouter` takes them from a random sample, so its counts per range change from run to
//!   run) and routes each sample by the conditions of its filter on the key's first field: one
//!   range, several, or all (scatter-gather) when the filter does not bound it. Percentages are
//!   rounded up to ten decimals, as on a `refrouter`.
//!
//! The replies and messages are those of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026, `t5.js` to `t8.js` in the lane's notes).

use std::collections::BTreeMap;

use teseladb_meta::catalog::CollectionEntry;
use teseladb_plan::logical::intervals;
use teseladb_bson::{Document, Value};

use super::query_analysis::{READS, WRITES, analyzed_ns, no_namespace, samples_of};
use super::sharding::{ShardKey, document_key, shard_key_of, shard_values};
use super::{Backend, Ctx, admin, reshard};
use crate::error::{CommandError, ILLEGAL_OPERATION, INVALID_OPTIONS};
use crate::types::wrong_type;

/// `analyzeShardKey`.
pub const ANALYZE_SHARD_KEY: &str = "analyzeShardKey";
/// The ranges the read and write distribution cuts the key into (measured).
const RANGES: i64 = 100;
/// The most common values listed.
const MOST_COMMON: usize = 5;
/// From this correlation on, the key is monotonic.
const MONOTONIC: f64 = 0.7;
const NOTE: &str = "Due to performance reasons, the analyzeShardKey command does not filter out orphan documents when calculating metrics about the characteristics of the shard key. Therefore, if \"numOrphanDocs\" is large relative to \"numDocsTotal\", you may want to rerun the command at some other time to get more accurate \"numDistinctValues\" and \"mostCommonValues\" metrics.";

/// A document of the collection: its key under the candidate, its values, `_id` key, size.
struct Row {
    key: Vec<u8>,
    values: Vec<Value>,
    id: Vec<u8>,
    bytes: i64,
}

fn round_up(x: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (x * scale).ceil() / scale
}

/// `analyzeShardKey`.
pub fn analyze_shard_key<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = ANALYZE_SHARD_KEY;
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    let key = match ctx.cmd.get("key") {
        Some(Value::Document(k)) => k.clone(),
        Some(v) => return Err(wrong_type(&format!("{name}.key"), v, "object")),
        None => return Err(super::args::missing(name, "key")),
    };
    let asked_kc = super::args::bool_field(ctx.cmd, name, "keyCharacteristics")?;
    let asked_rw = super::args::bool_field(ctx.cmd, name, "readWriteDistribution")?;
    admin::admin_only(name, ctx.db)?;
    let (kc, rw) = (asked_kc.unwrap_or(true), asked_rw.unwrap_or(true));
    if !kc && !rw {
        return Err(CommandError::new(INVALID_OPTIONS, "Cannot skip analyzing all metrics"));
    }
    if ctx.cmd.get("sampleRate").is_some() && ctx.cmd.get("sampleSize").is_some() {
        let msg = "Cannot specify both 'sampleRate' and 'sampleSize'";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    super::sharding::check_key(&ShardKey { key: key.clone(), unique: false })?;
    let ns = analyzed_ns(&text)?;
    let coll = ctx.backend.collection(&ns)?.ok_or_else(no_namespace)?;
    let rows = rows(ctx.backend, &coll, &key)?;
    if rows.is_empty() {
        return Err(CommandError::new(
            ILLEGAL_OPERATION,
            "Cannot analyze a shard key for an empty collection: CollectionIsEmptyLocally: Cannot analyze the characteristics of a shard key for an empty collection",
        ));
    }
    let indexed = reshard::has_prefix_index(ctx.backend, &coll, &key)?;
    let explicit = asked_kc == Some(true) || asked_rw == Some(false);
    if kc && !indexed && explicit {
        return Err(CommandError::new(
            ILLEGAL_OPERATION,
            "Cannot analyze the characteristics of a shard key that does not have a supporting index",
        ));
    }
    let distribution = match rw {
        true => {
            reshard::check_cardinality(ctx.backend, &coll, &key, RANGES).map_err(|mut e| {
                e.errmsg = format!(
                    "Failed to find split points that partition the data into {RANGES} chunks with roughly equal number of documents using the shard key being analyzed :: caused by :: {}",
                    e.errmsg
                );
                e
            })?;
            let now = ctx.backend.now_ms();
            let samples = samples_of(ctx.shared, &ns, coll.uuid, now);
            Some(distributions(&coll, &key, &rows, &samples)?)
        }
        false => None,
    };
    let mut reply = Document::new();
    if kc && indexed {
        let sampled = sampled_count(ctx.cmd, rows.len());
        let sharded = shard_key_of(&coll)?.is_some();
        let unique = is_unique(ctx.backend, &coll, &key)?;
        reply.push("keyCharacteristics", characteristics(&key, &rows, sampled, sharded, unique));
    }
    if let Some((reads, writes)) = distribution {
        reply.push("readDistribution", reads);
        reply.push("writeDistribution", writes);
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

/// The documents of `coll` under candidate `key`, in its order.
fn rows<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    key: &Document,
) -> Result<Vec<Row>, CommandError> {
    let id_key = Document::new().with("_id", 1);
    let mut out = Vec::new();
    reshard::for_each_document(backend, coll, None, |_, doc| {
        let mut probe = doc.clone();
        probe.insert("_id", Value::MinKey);
        let only_id = Document::new().with("_id", doc.get("_id").cloned().unwrap_or(Value::Null));
        out.push(Row {
            key: document_key(coll, key, &probe)?,
            values: shard_values(key, doc)?,
            id: document_key(coll, &id_key, &only_id)?,
            bytes: i64::try_from(doc.to_bytes()?.len()).unwrap_or(i64::MAX),
        });
        Ok(())
    })?;
    out.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// The documents `sampleSize` or `sampleRate` asks to read, all without them.
fn sampled_count(cmd: &Document, n: usize) -> usize {
    let number = |v: &Value| match v {
        Value::Int32(x) => Some(f64::from(*x)),
        Value::Int64(x) => Some(*x as f64),
        Value::Double(x) => Some(*x),
        _ => None,
    };
    match (cmd.get("sampleSize").and_then(number), cmd.get("sampleRate").and_then(number)) {
        (Some(size), _) => n.min(size.max(0.0) as usize),
        (None, Some(rate)) => n.min((n as f64 * rate).ceil() as usize),
        (None, None) => n,
    }
}

/// Whether a unique index has exactly the fields of `key` (the `_id` index for `{_id: 1}`).
fn is_unique<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    key: &Document,
) -> Result<bool, CommandError> {
    let fields: Vec<&str> = key.iter().map(|(f, _)| f).collect();
    if fields == ["_id"] {
        return Ok(true);
    }
    for e in backend.indexes(coll.id)? {
        let spec = Document::from(teseladb_bson::RawDoc::from_bytes(&e.spec)?);
        let unique = matches!(spec.get("unique"), Some(Value::Boolean(true)));
        // Every index spec has its key document.
        let same_key = match spec.get("key") {
            Some(Value::Document(k)) => k.iter().map(|(f, _)| f).eq(fields.iter().copied()),
            _ => false,
        };
        if unique && same_key {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `keyCharacteristics`.
fn characteristics(
    key: &Document,
    rows: &[Row],
    sampled: usize,
    sharded: bool,
    unique: bool,
) -> Document {
    let n = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let bytes: i64 = rows.iter().map(|r| r.bytes).sum();
    let mut groups: Vec<(&Row, i64)> = Vec::new();
    for r in rows {
        match groups.last_mut() {
            Some((first, count)) if first.key == r.key => *count += 1,
            _ => groups.push((r, 1)),
        }
    }
    let mut common = groups.clone();
    common.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.key.cmp(&b.0.key)));
    let most: Vec<Value> = common
        .iter()
        .take(MOST_COMMON)
        .map(|(r, f)| {
            let value = key
                .iter()
                .zip(r.values.iter())
                .fold(Document::new(), |d, ((field, _), v)| d.with(field, v.clone()));
            Value::Document(Document::new().with("value", value).with("frequency", *f))
        })
        .collect();
    let coefficient = correlation(rows);
    let kind = if coefficient.abs() >= MONOTONIC { "monotonic" } else { "not monotonic" };
    let mut d = Document::new().with("numDocsTotal", n);
    if sharded {
        d.push("numOrphanDocs", 0i64);
    }
    d.push("avgDocSizeBytes", bytes / n.max(1));
    d.push("numDocsSampled", i64::try_from(sampled).unwrap_or(i64::MAX));
    d.push("isUnique", unique);
    d.push("numDistinctValues", i64::try_from(groups.len()).unwrap_or(i64::MAX));
    d.push("mostCommonValues", Value::Array(most));
    d.push(
        "monotonicity",
        Document::new()
            .with("recordIdCorrelationCoefficient", round_up(coefficient, 9))
            .with("type", kind),
    );
    if sharded {
        d.push("note", NOTE);
    }
    d
}

/// The correlation between the position of each document in key order and the rank of its
/// `_id`.
fn correlation(rows: &[Row]) -> f64 {
    let mut ids: Vec<(usize, &Vec<u8>)> = rows.iter().map(|r| &r.id).enumerate().collect();
    ids.sort_by(|a, b| a.1.cmp(b.1));
    let mut rank = vec![0f64; rows.len()];
    for (r, (pos, _)) in ids.iter().enumerate() {
        rank[*pos] = r as f64;
    }
    let n = rows.len() as f64;
    let mean = (n - 1.0) / 2.0;
    let (mut cov, mut var) = (0.0, 0.0);
    for (i, r) in rank.iter().enumerate() {
        cov += (i as f64 - mean) * (r - mean);
        var += (i as f64 - mean).powi(2);
    }
    if var == 0.0 { 1.0 } else { cov / var }
}

/// How a sample is routed: to these ranges, or to all (`None`).
type Routed = Option<(usize, usize)>;

/// `readDistribution` and `writeDistribution` of `samples`.
fn distributions(
    coll: &CollectionEntry,
    key: &Document,
    rows: &[Row],
    samples: &[Document],
) -> Result<(Document, Document), CommandError> {
    let bounds = bounds(rows);
    let ranges = bounds.len() + 1;
    let chunk_of = |k: &[u8]| bounds.iter().take_while(|b| b.as_slice() <= k).count();
    let route = |filter: &Document| -> Result<Routed, CommandError> {
        Ok(first_field_span(coll, key, filter)?.map(|(lo, hi)| (chunk_of(&lo), chunk_of(&hi))))
    };
    let mut reads = Tally::new(&READS, ranges);
    let mut writes = Tally::new(&WRITES, ranges);
    let mut extra = [0i64; 3];
    for s in samples {
        // The node writes every sample with its command and its name.
        let (cmd_name, cmd) = match (s.get("cmdName"), s.get("cmd")) {
            (Some(Value::String(n)), Some(Value::Document(c))) => (n, c),
            _ => continue,
        };
        let (filter, multi, update) = statement(cmd_name, cmd);
        let routed = route(&filter)?;
        if READS.contains(&cmd_name.as_str()) {
            reads.add(cmd_name, routed);
            continue;
        }
        writes.add(cmd_name, routed);
        if update.as_ref().is_some_and(|u| changes_key(u, key)) {
            extra[0] += 1;
        }
        if !has_full_key(&filter, key) {
            extra[if multi { 2 } else { 1 }] += 1;
        }
    }
    let mut w = writes.document("Writes");
    if writes.total > 0 {
        let names = [
            "percentageOfShardKeyUpdates",
            "percentageOfSingleWritesWithoutShardKey",
            "percentageOfMultiWritesWithoutShardKey",
        ];
        for (name, n) in names.iter().zip(extra) {
            w.push(*name, writes.percent(n));
        }
    }
    Ok((reads.document("Reads"), w))
}

/// The keys where the 100 ranges start, after the first: every tenth of evenly spaced documents.
fn bounds(rows: &[Row]) -> Vec<Vec<u8>> {
    let want = usize::try_from(RANGES).unwrap_or(100) * 10;
    let sample: Vec<&Vec<u8>> = if rows.len() <= want {
        rows.iter().map(|r| &r.key).collect()
    } else {
        (0..want).map(|i| &rows[i * rows.len() / want].key).collect()
    };
    let mut points: Vec<Vec<u8>> = sample.iter().step_by(10).map(|k| (*k).clone()).collect();
    points.dedup();
    points.into_iter().skip(1).take(usize::try_from(RANGES - 1).unwrap_or(99)).collect()
}

/// Counts of a distribution.
struct Tally<'a> {
    names: &'a [&'a str],
    by_name: BTreeMap<String, i64>,
    total: i64,
    kinds: [i64; 3],
    by_range: Vec<i64>,
}

impl<'a> Tally<'a> {
    fn new(names: &'a [&'a str], ranges: usize) -> Tally<'a> {
        Tally {
            names,
            by_name: BTreeMap::new(),
            total: 0,
            kinds: [0; 3],
            by_range: vec![0; ranges],
        }
    }

    fn add(&mut self, name: &str, routed: Routed) {
        *self.by_name.entry(name.to_owned()).or_default() += 1;
        self.total += 1;
        let (lo, hi) = routed.unwrap_or((0, self.by_range.len() - 1));
        let kind = match routed {
            None => 2,
            Some((a, b)) if a == b => 0,
            Some(_) => 1,
        };
        self.kinds[kind] += 1;
        for r in &mut self.by_range[lo..=hi] {
            *r += 1;
        }
    }

    fn percent(&self, n: i64) -> f64 {
        round_up(n as f64 * 100.0 / self.total.max(1) as f64, 10)
    }

    fn document(&self, what: &str) -> Document {
        let mut size = Document::new().with("total", self.total);
        for n in self.names {
            size.push(*n, self.by_name.get(*n).copied().unwrap_or(0));
        }
        let mut d = Document::new().with("sampleSize", size);
        if self.total == 0 {
            return d;
        }
        let kinds = ["SingleShard", "MultiShard", "ScatterGather"];
        for (k, n) in kinds.iter().zip(self.kinds) {
            d.push(format!("percentageOf{k}{what}"), self.percent(n));
        }
        let by: Vec<Value> = self.by_range.iter().map(|n| Value::Int64(*n)).collect();
        d.push(format!("num{what}ByRange"), Value::Array(by));
        d
    }
}

/// A sample's filter, whether it is a multi write and its update.
fn statement(name: &str, cmd: &Document) -> (Document, bool, Option<Value>) {
    let doc_of = |v: Option<&Value>| match v {
        Some(Value::Document(d)) => d.clone(),
        _ => Document::new(),
    };
    let first = |list: &str| match cmd.get(list) {
        Some(Value::Array(a)) => doc_of(a.first()),
        _ => Document::new(),
    };
    match name {
        "update" => {
            let s = first("updates");
            let multi = matches!(s.get("multi"), Some(Value::Boolean(true)));
            (doc_of(s.get("q")), multi, s.get("u").cloned())
        }
        "delete" => {
            let s = first("deletes");
            let multi = matches!(s.get("limit"), Some(Value::Int32(0) | Value::Int64(0)));
            (doc_of(s.get("q")), multi, None)
        }
        "findAndModify" => (doc_of(cmd.get("query")), false, cmd.get("update").cloned()),
        _ => (doc_of(cmd.get("filter")), false, None),
    }
}

/// Whether update `u` sets or removes a field of `key`.
fn changes_key(u: &Value, key: &Document) -> bool {
    let Value::Document(u) = u else { return false };
    let touches = |d: &Document| key.iter().any(|(f, _)| d.get(f).is_some());
    let operators = u.iter().any(|(k, _)| k.starts_with('$'));
    match operators {
        true => u.iter().any(|(_, v)| matches!(v, Value::Document(d) if touches(d))),
        false => false,
    }
}

/// Whether `filter` names every field of `key` with one value.
fn has_full_key(filter: &Document, key: &Document) -> bool {
    key.iter().all(|(f, _)| match filter.get(f) {
        Some(Value::Document(d)) => {
            d.iter().next().is_some_and(|(op, _)| op == "$eq") && d.len() == 1
        }
        Some(_) => true,
        None => false,
    })
}

/// The keys between which `filter` keeps the documents by the key's first field, `None` when
/// it does not bound it (only top-level and `$and` conditions count).
fn first_field_span(
    coll: &CollectionEntry,
    key: &Document,
    filter: &Document,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, CommandError> {
    let Some((field, _)) = key.iter().next() else { return Ok(None) };
    let mut span: Option<(Value, Value)> = None;
    let mut narrow = |lo: Value, hi: Value| {
        span = Some(match span.take() {
            None => (lo, hi),
            Some((a, b)) => (max_of(a, lo), min_of(b, hi)),
        });
    };
    for clause in conditions(filter, field) {
        match clause {
            Value::Document(ops) if ops.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
                for (op, arg) in ops.iter() {
                    let Some((bounds, _)) = intervals(op, arg) else { continue };
                    let lo = bounds.iter().map(|b| b.low.clone()).reduce(min_of);
                    let hi = bounds.iter().map(|b| b.high.clone()).reduce(max_of);
                    if let (Some(lo), Some(hi)) = (lo, hi) {
                        narrow(lo, hi);
                    }
                }
            }
            v => narrow(v.clone(), v),
        }
    }
    let Some((lo, hi)) = span else { return Ok(None) };
    let probe = |v: Value, edge: Value| {
        key.iter()
            .enumerate()
            .fold(Document::new(), |d, (i, (f, _))| {
                d.with(f, if i == 0 { v.clone() } else { edge.clone() })
            })
            .with("_id", edge)
    };
    let low = document_key(coll, key, &probe(lo, Value::MinKey))?;
    let high = document_key(coll, key, &probe(hi, Value::MaxKey))?;
    Ok(Some((low, high)))
}

/// The conditions of `filter` on `field`, at its top level and under `$and`.
fn conditions(filter: &Document, field: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (k, v) in filter.iter() {
        match (k, v) {
            (f, v) if f == field => out.push(v.clone()),
            ("$and", Value::Array(items)) => {
                for i in items {
                    if let Value::Document(d) = i {
                        out.extend(conditions(d, field));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn order(a: &Value, b: &Value) -> std::cmp::Ordering {
    teseladb_plan::logical::compare_values(a, b)
}

fn min_of(a: Value, b: Value) -> Value {
    if order(&a, &b).is_le() { a } else { b }
}

fn max_of(a: Value, b: Value) -> Value {
    if order(&a, &b).is_ge() { a } else { b }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn d(pairs: &[(&str, Value)]) -> Document {
        pairs.iter().fold(Document::new(), |d, (k, v)| d.with(*k, v.clone()))
    }

    #[test]
    fn the_sample_takes_the_size_or_the_rate() {
        let n = 10;
        assert_eq!(sampled_count(&Document::new(), n), 10);
        assert_eq!(sampled_count(&d(&[("sampleSize", Value::Int32(3))]), n), 3);
        assert_eq!(sampled_count(&d(&[("sampleSize", Value::Int64(30))]), n), 10);
        assert_eq!(sampled_count(&d(&[("sampleSize", Value::Double(-1.0))]), n), 0);
        assert_eq!(sampled_count(&d(&[("sampleRate", Value::Double(0.25))]), n), 3);
        assert_eq!(sampled_count(&d(&[("sampleRate", Value::from("x"))]), n), 10);
    }

    #[test]
    fn a_statement_gives_its_filter_its_multi_and_its_update() {
        let q = d(&[("a", Value::Int32(1))]);
        let one = |list: &str, s: Document| d(&[(list, Value::Array(vec![Value::Document(s)]))]);
        let upd = one(
            "updates",
            d(&[
                ("q", Value::Document(q.clone())),
                ("u", Value::Document(Document::new())),
                ("multi", Value::Boolean(true)),
            ]),
        );
        assert_eq!(
            statement("update", &upd),
            (q.clone(), true, Some(Value::Document(Document::new())))
        );
        let del =
            one("deletes", d(&[("q", Value::Document(q.clone())), ("limit", Value::Int64(0))]));
        assert_eq!(statement("delete", &del), (q.clone(), true, None));
        let del1 = one("deletes", d(&[("q", Value::Int32(1)), ("limit", Value::Int32(1))]));
        assert_eq!(statement("delete", &del1), (Document::new(), false, None));
        assert_eq!(statement("update", &Document::new()).0, Document::new());
        let fam = d(&[("query", Value::Document(q.clone())), ("update", Value::Int32(1))]);
        assert_eq!(statement("findAndModify", &fam), (q.clone(), false, Some(Value::Int32(1))));
        let find = d(&[("filter", Value::Document(q.clone()))]);
        assert_eq!(statement("find", &find), (q, false, None));
    }

    #[test]
    fn only_operators_on_a_key_field_change_the_key() {
        let key = d(&[("a", Value::Int32(1))]);
        let set =
            |f: &str| Value::Document(d(&[("$set", Value::Document(d(&[(f, Value::Int32(2))])))]));
        assert!(changes_key(&set("a"), &key));
        assert!(!changes_key(&set("b"), &key));
        assert!(!changes_key(&Value::Document(d(&[("a", Value::Int32(2))])), &key));
        assert!(!changes_key(&Value::Array(Vec::new()), &key));
        assert!(!changes_key(&Value::Document(d(&[("$inc", Value::Int32(1))])), &key));
    }

    #[test]
    fn a_full_key_names_every_field_with_one_value() {
        let key = d(&[("a", Value::Int32(1)), ("b", Value::Int32(1))]);
        let eq = |v: Value| Value::Document(d(&[("$eq", v)]));
        assert!(has_full_key(&d(&[("a", Value::Int32(1)), ("b", eq(Value::Int32(2)))]), &key));
        assert!(!has_full_key(&d(&[("a", Value::Int32(1))]), &key));
        let gt = Value::Document(d(&[("$gt", Value::Int32(2))]));
        assert!(!has_full_key(&d(&[("a", Value::Int32(1)), ("b", gt)]), &key));
        let two = Value::Document(d(&[("$eq", Value::Int32(2)), ("$ne", Value::Int32(3))]));
        assert!(!has_full_key(&d(&[("a", Value::Int32(1)), ("b", two)]), &key));
        assert!(!has_full_key(
            &d(&[("a", Value::Int32(1)), ("b", Value::Document(Document::new()))]),
            &key
        ));
    }

    #[test]
    fn conditions_come_from_the_top_level_and_and() {
        let inner = Value::Document(d(&[("a", Value::Int32(2))]));
        let f = d(&[
            ("a", Value::Int32(1)),
            ("$and", Value::Array(vec![inner, Value::Int32(3)])),
            ("$or", Value::Array(vec![Value::Document(d(&[("a", Value::Int32(4))]))])),
        ]);
        assert_eq!(conditions(&f, "a"), vec![Value::Int32(1), Value::Int32(2)]);
    }

    fn row(key: u8, id: u8) -> Row {
        Row { key: vec![key], values: Vec::new(), id: vec![id], bytes: 0 }
    }

    #[test]
    fn the_correlation_follows_the_ids() {
        let up: Vec<Row> = (0..5).map(|i| row(i, i)).collect();
        assert!((correlation(&up) - 1.0).abs() < 1e-9);
        let down: Vec<Row> = (0..5).map(|i| row(i, 9 - i)).collect();
        assert!((correlation(&down) + 1.0).abs() < 1e-9);
        assert!((correlation(&[row(0, 0)]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_bounds_space_the_documents() {
        let few: Vec<Row> = (0..30).map(|i| row(i, i)).collect();
        assert_eq!(bounds(&few).len(), 2);
        let many: Vec<Row> = (0..3000u32).map(|i| row((i / 12) as u8, 0)).collect();
        assert!(bounds(&many).len() <= usize::try_from(RANGES).unwrap_or(100));
    }
}
