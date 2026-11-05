//! `special-collections` workload (015 T002, FR-003, FR-011, SC-002): one capped collection of
//! `size` [`SIZE`] and `max` [`MAX`] on the first range of the transaction workloads, written by
//! clients through the code a node runs for a capped write (`teseladb-wire`'s `Bound`, evaluated
//! on the range's leader with the collection's state). Each turn first reads, at one mark, the
//! collection's state and the documents its last inserts can have; an insert then goes with the
//! oldest documents it read, a delete with the document it names, and a check writes nothing.
//! Every committed read goes through the `capped` checks: the state counts exactly the documents
//! read, the collection holds no more than `max` documents and, after its last insert, no more
//! than `size` bytes, every document is one a client sent, once, in insertion order.
//!
//! Other turns write and read a time series of two meta values over three windows of an hour
//! (FR-006, FR-011): a measurement goes into the bucket of its window and meta value through the
//! leader's evaluation of a time series insert, and every committed read of the buckets goes
//! through the `timeseries` checks: each bucket unpacks into as many measurements as its count,
//! each one a measurement a client sent, once, of the bucket's meta value and window, within the
//! bucket's least and greatest time.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_keys::KeyBuilder;
use teseladb_mql::timeseries_write::BucketSpec;
use teseladb_range::capped::{CappedLimits, CappedState, ID_INDEX, doc_key, seq_of, state_key};
use teseladb_range::host::RangeError;
use teseladb_txn::coordinator::Outcome;
use teseladb_wire::commands::index_maintenance::IndexRule;
use teseladb_wire::commands::mutation::{
    Bound, CappedRule, DocMutation, Image, MutationResult, Rules, TimeSeriesRule,
};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::workloads::txn::{Finished, Op, Script, Step, TxnWorkload};
use crate::world::World;

/// Name of the checks.
pub const CHECKER: &str = "capped";
/// Name of the checks of the time series.
pub const TS_CHECKER: &str = "timeseries";
/// The time series.
const SERIES: u32 = 8;
/// Its meta values and windows.
const METAS: u64 = 2;
const WINDOWS: u64 = 3;
/// Share of the turns on the time series.
const TS_SHARE: f64 = 0.4;
/// Start of the first window (2026-01-01T00:00:00Z), and a window's length, in ms.
const T0: i64 = 1_767_225_600_000;
const HOUR: i64 = 3_600_000;
/// The collection.
const COLLECTION: u32 = 7;
/// Its limits: bytes and documents.
const SIZE: u64 = 220;
const MAX: u64 = 4;
/// Clients.
const CLIENTS: u32 = 4;
/// Shares of the turns that check and that delete; the others insert.
const CHECK_SHARE: f64 = 0.2;
const DELETE_SHARE: f64 = 0.15;
/// Most bytes of padding a document carries: from about 30 to 70 bytes each.
const MAX_PAD: u64 = 40;

/// What a client is doing.
#[derive(Clone, Debug)]
enum Turn {
    /// A check.
    Check,
    /// An insert of the document with `_id` `id` and `pad` bytes of padding.
    Insert { id: String, pad: usize },
    /// A delete of the document with `_id` `id`.
    Delete { id: String },
    /// A check of the time series.
    TsCheck,
    /// An insert of measurement `id` of meta value `meta` at `t` (ms).
    TsInsert { id: String, meta: u64, t: i64 },
}

/// How the time series lays out its buckets.
fn series() -> BucketSpec {
    BucketSpec {
        time_field: "t".into(),
        meta_field: Some("m".into()),
        max_span_seconds: 3600,
        rounding_seconds: 60,
    }
}

/// The first bucket of meta value `meta` and window `window`: its `_id` and key.
fn bucket(meta: u64, window: u64) -> ([u8; 12], Vec<u8>) {
    let t = T0 + window as i64 * HOUR;
    let meta = Value::Int64(meta as i64);
    let id = series().bucket_id(t, Some(&meta), 0).expect("a small meta value");
    let holder = Document::new().with("_id", Value::ObjectId(id)).to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&holder).expect("just written");
    let key = KeyBuilder::document_clustered(0, SERIES, raw.get("_id").expect("just written"));
    (id, key.as_bytes().to_vec())
}

/// Every bucket the workload writes, with its meta value and window.
fn buckets() -> Vec<(u64, u64, [u8; 12], Vec<u8>)> {
    (0..METAS)
        .flat_map(|m| (0..WINDOWS).map(move |w| (m, w)))
        .map(|(m, w)| {
            let (id, key) = bucket(m, w);
            (m, w, id, key)
        })
        .collect()
}

/// The script of `special-collections`.
pub struct SpecialCollections {
    turns: BTreeMap<u32, Turn>,
    /// Each client's inserts so far, for the `_id` of the next one.
    sent_by: BTreeMap<u32, u64>,
    /// Every `_id` a client sent in an insert.
    sent: BTreeSet<String>,
    /// Every `_id` a client sent in a measurement.
    sent_ts: BTreeSet<String>,
    /// The fault window ended.
    healed: bool,
    checked: u64,
    inserted: u64,
    /// Measurements inserted and checks of the time series that committed.
    measured: u64,
    series_checked: u64,
    violation: Option<Violation>,
}

impl SpecialCollections {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        let script = SpecialCollections {
            turns: BTreeMap::new(),
            sent_by: BTreeMap::new(),
            sent: BTreeSet::new(),
            sent_ts: BTreeSet::new(),
            healed: false,
            checked: 0,
            inserted: 0,
            measured: 0,
            series_checked: 0,
            violation: None,
        };
        Box::new(TxnWorkload::new("special-collections", (nodes, cores), CLIENTS, script))
    }
}

/// The rules of a write that read `oldest` of the collection's oldest documents.
fn rules(oldest: usize) -> Rules {
    let spec = Document::new()
        .with("v", 2)
        .with("key", Document::new().with("_id", 1))
        .with("name", "_id_")
        .with("unique", true);
    let limits = CappedLimits { size: SIZE, max: Some(MAX) };
    Rules {
        ns: "sim.capped".into(),
        collection: COLLECTION,
        indexes: vec![IndexRule { id: ID_INDEX, spec, buckets: 0, delete_only: false }],
        capped: Some(CappedRule { limits, oldest }),
        ..Rules::default()
    }
}

/// The state among a turn's reads (an empty collection without one).
fn state_of(read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Result<CappedState, Violation> {
    match read.get(&state_key(0, COLLECTION)).cloned().flatten() {
        Some(v) => CappedState::decode(&v)
            .map_err(|e| Violation::safety(CHECKER, format!("an unreadable state: {e:?}"))),
        None => Ok(CappedState::default()),
    }
}

/// The documents the collection can hold after state `s`: the last `2 × max` insertion numbers
/// (with `max` enforced, every document is among the last `max`).
fn window(s: CappedState) -> std::ops::Range<u64> {
    s.next.saturating_sub(2 * MAX)..s.next
}

/// The documents a turn read, `(seq, bytes)` in insertion order.
fn documents(read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<(u64, Vec<u8>)> {
    read.iter().filter_map(|(k, v)| Some((seq_of(0, COLLECTION, k)?, v.clone()?))).collect()
}

/// The `_id` of a stored document.
fn id_of(bytes: &[u8]) -> Option<String> {
    match RawDoc::from_bytes(bytes).ok()?.get("_id")? {
        RawValue::String(s) => Some(s.to_owned()),
        _ => None,
    }
}

impl SpecialCollections {
    /// The `capped` checks on what one committed turn read at its mark.
    fn check(&self, done: &Finished) -> Result<(), Violation> {
        let read: BTreeMap<Vec<u8>, Option<Vec<u8>>> = done
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Read(k, v) => Some((k.clone(), v.clone())),
                Op::Write(..) => None,
            })
            .collect();
        let fail =
            |what: String| Err(Violation::safety(CHECKER, format!("{what} at {:?}", done.read_ts)));
        let s = state_of(&read)?;
        let docs = documents(&read);
        let bytes: u64 = docs.iter().map(|(_, d)| d.len() as u64).sum();
        if (docs.len() as u64, bytes) != (s.docs, s.bytes) {
            return fail(format!("state {s:?} for {} documents of {bytes} bytes", docs.len()));
        }
        if s.docs > MAX {
            return fail(format!("{} documents with max {MAX}", s.docs));
        }
        // Documents keep their size here, so only the newest alone may go over `size`.
        if s.bytes > SIZE && s.docs > 1 {
            return fail(format!("{} bytes in {} documents with size {SIZE}", s.bytes, s.docs));
        }
        let mut ids = BTreeSet::new();
        for (seq, d) in &docs {
            if *seq >= s.next {
                return fail(format!("document {seq} at or past the next one {}", s.next));
            }
            let Some(id) = id_of(d).filter(|id| self.sent.contains(id)) else {
                return fail(format!("document {seq} is no insert a client sent"));
            };
            if !ids.insert(id.clone()) {
                return fail(format!("_id {id} twice"));
            }
        }
        Ok(())
    }

    /// The `timeseries` checks on what one committed turn read of the buckets at its mark.
    fn check_series(&self, done: &Finished) -> Result<(), Violation> {
        let spec = series();
        let fail = |what: String| {
            Err(Violation::safety(TS_CHECKER, format!("{what} at {:?}", done.read_ts)))
        };
        let mut seen = BTreeSet::new();
        for (meta, window, _, key) in buckets() {
            let read = done.ops.iter().find_map(|op| match op {
                Op::Read(k, v) if *k == key => Some(v.clone()),
                _ => None,
            });
            let Some(Some(bytes)) = read else { continue };
            let Ok(raw) = RawDoc::from_bytes(&bytes) else {
                return fail("an unreadable bucket".into());
            };
            let doc = Document::from(raw);
            let control = |side: &str, f: &str| match doc.get("control") {
                Some(Value::Document(c)) => match c.get(side) {
                    Some(Value::Document(d)) => d.get(f).cloned(),
                    other => other.cloned(),
                },
                _ => None,
            };
            // Unpacking gives exactly the bucket's `control.count` measurements.
            let Ok(rows) = spec.measurements(raw) else {
                return fail("a bucket that does not unpack".into());
            };
            if doc.get("meta") != Some(&Value::Int64(meta as i64)) {
                return fail(format!("bucket of meta {meta} holds meta {:?}", doc.get("meta")));
            }
            let (Some(Value::DateTime(lo)), Some(Value::DateTime(hi))) =
                (control("min", "t"), control("max", "t"))
            else {
                return fail("a bucket without its times".into());
            };
            let start = T0 + window as i64 * HOUR;
            for r in &rows {
                let (Some(Value::String(id)), Some(Value::DateTime(t))) =
                    (r.get("_id"), r.get("t"))
                else {
                    return fail(format!("a measurement without _id or time: {r:?}"));
                };
                if !self.sent_ts.contains(id) || !seen.insert(id.clone()) {
                    return fail(format!("measurement {id} not sent, or twice"));
                }
                if *t < start || *t >= start + HOUR || *t < lo || *t > hi {
                    return fail(format!(
                        "measurement {id} at {t} in the bucket of [{start}, +1h) with times [{lo}, {hi}]"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The write of `turn` once the collection's state and documents were read; `None` when
    /// there is nothing to write.
    fn write(&self, turn: &Turn, read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Option<Bound> {
        let docs = documents(read);
        let keys_of = |seqs: &mut dyn Iterator<Item = u64>| -> Vec<Vec<u8>> {
            std::iter::once(state_key(0, COLLECTION))
                .chain(seqs.map(|s| doc_key(0, COLLECTION, s)))
                .collect()
        };
        match turn {
            Turn::Check | Turn::TsCheck => None,
            Turn::TsInsert { id, meta, t } => {
                let window = (*t - T0).div_euclid(HOUR) as u64;
                let (bucket, key) = bucket(*meta, window);
                let doc = Document::new()
                    .with("_id", id.as_str())
                    .with("t", Value::DateTime(*t))
                    .with("m", Value::Int64(*meta as i64))
                    .with("v", *t % 1000);
                let rules = Rules {
                    ns: "sim.series".into(),
                    collection: SERIES,
                    timeseries: Some(TimeSeriesRule { spec: series(), bucket }),
                    ..Rules::default()
                };
                let mutation = DocMutation::Insert { docs: vec![doc], ordered: true, rules };
                Some(Bound { mutation, keys: vec![key] })
            }
            Turn::Insert { id, pad } => {
                let doc = Document::new().with("_id", id.as_str()).with("p", "x".repeat(*pad));
                let mutation = DocMutation::Insert {
                    docs: vec![doc],
                    ordered: true,
                    rules: rules(docs.len()),
                };
                let keys = keys_of(&mut docs.iter().map(|(s, _)| *s));
                Some(Bound { mutation, keys })
            }
            Turn::Delete { id } => {
                let (seq, _) = docs.iter().find(|(_, d)| id_of(d).as_deref() == Some(id))?;
                let mutation = DocMutation::Delete {
                    filter: Document::new().with("_id", id.as_str()),
                    multi: false,
                    image: Image::None,
                    rules: rules(0),
                };
                Some(Bound { mutation, keys: keys_of(&mut std::iter::once(*seq)) })
            }
        }
    }
}

impl Script for SpecialCollections {
    fn plan(&mut self, w: &mut World, client: u32) -> Vec<Step> {
        // Once the faults stop, turns check until one has committed: no seed ends unchecked.
        // Once the faults stop, turns check both collections until each check has committed.
        let series_unchecked = self.healed && self.series_checked == 0;
        let capped_unchecked = self.healed && self.checked == 0;
        if (series_unchecked && !capped_unchecked) || (!capped_unchecked && w.rng.chance(TS_SHARE))
        {
            let turn = if series_unchecked || w.rng.chance(CHECK_SHARE) {
                Turn::TsCheck
            } else {
                let n = self.sent_by.entry(client).or_insert(0);
                *n += 1;
                let id = format!("m{client}-{n}");
                self.sent_ts.insert(id.clone());
                let meta = w.rng.range(0, METAS);
                let t = T0 + w.rng.range(0, (WINDOWS as i64 * HOUR) as u64) as i64;
                Turn::TsInsert { id, meta, t }
            };
            self.turns.insert(client, turn);
            let mut steps: Vec<Step> = buckets().into_iter().map(|(.., k)| Step::Read(k)).collect();
            steps.extend([Step::Expand, Step::Commit]);
            return steps;
        }
        let n = w.rng.range(0, self.sent.len().max(1) as u64) as usize;
        let victim = self.sent.iter().nth(n).cloned();
        let turn = if (self.healed && self.checked == 0) || w.rng.chance(CHECK_SHARE) {
            Turn::Check
        } else if let Some(id) = victim.filter(|_| w.rng.chance(DELETE_SHARE)) {
            Turn::Delete { id }
        } else {
            let n = self.sent_by.entry(client).or_insert(0);
            *n += 1;
            let id = format!("c{client}-{n}");
            self.sent.insert(id.clone());
            Turn::Insert { id, pad: w.rng.range(0, MAX_PAD + 1) as usize }
        };
        self.turns.insert(client, turn);
        vec![Step::Read(state_key(0, COLLECTION)), Step::Expand, Step::Commit]
    }

    fn expand(&mut self, client: u32, read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<Step> {
        let Some(turn) = self.turns.get(&client) else { return Vec::new() };
        if matches!(turn, Turn::TsCheck | Turn::TsInsert { .. }) {
            return self
                .write(turn, read)
                .and_then(|b| b.encode().ok())
                .map(|bytes| vec![Step::Mutate(bytes)])
                .unwrap_or_default();
        }
        let Ok(s) = state_of(read) else { return Vec::new() };
        let unread: Vec<Vec<u8>> = window(s)
            .map(|seq| doc_key(0, COLLECTION, seq))
            .filter(|k| !read.contains_key(k))
            .collect();
        if !unread.is_empty() {
            let mut steps: Vec<Step> = unread.into_iter().map(Step::Read).collect();
            steps.push(Step::Expand);
            return steps;
        }
        // A write that cannot be encoded writes nothing: the turn commits its reads alone.
        self.write(turn, read)
            .and_then(|b| b.encode().ok())
            .map(|bytes| vec![Step::Mutate(bytes)])
            .unwrap_or_default()
    }

    fn value(
        &mut self,
        _client: u32,
        _key: &[u8],
        _read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>> {
        None
    }

    fn mutated(&mut self, client: u32, r: Result<Vec<u8>, RangeError>) {
        let Ok(result) = r.map_err(|_| ()).and_then(|b| MutationResult::decode(&b).map_err(|_| ()))
        else {
            return;
        };
        match self.turns.get(&client) {
            Some(Turn::Insert { .. }) if result.n == 1 => self.inserted += 1,
            Some(Turn::TsInsert { .. }) if result.n == 1 => self.measured += 1,
            _ => {}
        }
    }

    fn healed(&mut self) {
        self.healed = true;
    }

    fn ended(&mut self, client: u32, done: Finished) {
        let turn = self.turns.remove(&client);
        if !matches!(done.outcome, Outcome::Committed(_)) || self.violation.is_some() {
            return;
        }
        let checked = match turn {
            Some(Turn::TsCheck | Turn::TsInsert { .. }) => self.check_series(&done),
            _ => self.check(&done),
        };
        if let Err(v) = checked {
            self.violation = Some(v);
        }
        match turn {
            Some(Turn::Check) => self.checked += 1,
            Some(Turn::TsCheck) => self.series_checked += 1,
            _ => {}
        }
    }

    fn finish(&mut self) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        if self.inserted == 0 || self.checked == 0 {
            let what = format!("{} inserts and {} checks committed", self.inserted, self.checked);
            return Err(Violation::liveness(CHECKER, what));
        }
        if self.measured == 0 || self.series_checked == 0 {
            let what = format!(
                "{} measurements and {} checks committed",
                self.measured, self.series_checked
            );
            return Err(Violation::liveness(TS_CHECKER, what));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_hlc::Hlc;
    use teseladb_types::Nanos;

    use super::*;

    fn script() -> SpecialCollections {
        SpecialCollections {
            turns: BTreeMap::new(),
            sent_by: BTreeMap::new(),
            sent: ["a", "b"].into_iter().map(String::from).collect(),
            sent_ts: ["m1", "m2"].into_iter().map(String::from).collect(),
            healed: false,
            checked: 0,
            inserted: 0,
            measured: 0,
            series_checked: 0,
            violation: None,
        }
    }

    fn finished(ops: Vec<Op>, outcome: Outcome) -> Finished {
        Finished {
            ops,
            read_ts: Hlc { physical: 1, logical: 0 },
            outcome,
            began: Nanos::ZERO,
            acked: None,
            in_window: true,
        }
    }

    fn committed(ops: Vec<Op>) -> Finished {
        finished(ops, Outcome::Committed(Hlc { physical: 1, logical: 0 }))
    }

    fn doc(id: &str, pad: usize) -> Vec<u8> {
        Document::new().with("_id", id).with("p", "x".repeat(pad)).to_bytes().expect("small")
    }

    /// The reads of a capped turn: the state, then the documents `(seq, bytes)`.
    fn capped(s: CappedState, docs: &[(u64, Vec<u8>)]) -> Vec<Op> {
        let mut ops = vec![Op::Read(state_key(0, COLLECTION), Some(s.encode()))];
        ops.extend(
            docs.iter().map(|(seq, d)| Op::Read(doc_key(0, COLLECTION, *seq), Some(d.clone()))),
        );
        ops
    }

    fn capped_fails(docs: &[(u64, Vec<u8>)], next: u64, why: &str) {
        let bytes = docs.iter().map(|(_, d)| d.len() as u64).sum();
        let s = CappedState { bytes, docs: docs.len() as u64, next };
        let v = script().check(&committed(capped(s, docs))).expect_err(why);
        assert_eq!(v.checker, CHECKER);
        assert!(v.detail.contains(why), "{why}: {}", v.detail);
    }

    #[test]
    fn the_capped_checks_catch_each_broken_collection() {
        // A good one passes, also a single document over the size.
        let ok = [(0, doc("a", 10)), (1, doc("b", 10))];
        let s =
            CappedState { bytes: ok.iter().map(|(_, d)| d.len() as u64).sum(), docs: 2, next: 2 };
        script().check(&committed(capped(s, &ok))).expect("a good collection");
        let big = [(0, doc("a", 300))];
        let s = CappedState { bytes: big[0].1.len() as u64, docs: 1, next: 1 };
        script().check(&committed(capped(s, &big))).expect("the newest alone may pass the size");
        // A state that does not match its documents.
        let s = CappedState { bytes: 0, docs: 1, next: 1 };
        let v = script().check(&committed(capped(s, &[]))).expect_err("no documents");
        assert!(v.detail.contains("for 0 documents"), "{}", v.detail);
        // An unreadable state.
        let ops = vec![Op::Read(state_key(0, COLLECTION), Some(vec![1, 2, 3]))];
        let v = script().check(&committed(ops)).expect_err("unreadable");
        assert!(v.detail.contains("an unreadable state"), "{}", v.detail);
        let five: Vec<(u64, Vec<u8>)> = (0..5).map(|i| (i, doc("a", 0))).collect();
        capped_fails(&five, 5, "documents with max");
        capped_fails(&[(0, doc("a", 120)), (1, doc("b", 120))], 2, "bytes in 2 documents");
        capped_fails(&[(3, doc("a", 1))], 2, "at or past the next one");
        capped_fails(&[(0, doc("z", 1))], 1, "is no insert a client sent");
        capped_fails(&[(0, doc("a", 1)), (1, doc("a", 1))], 2, "_id a twice");
        let numeric = Document::new().with("_id", 7).to_bytes().expect("small");
        capped_fails(&[(0, numeric)], 1, "is no insert a client sent");
    }

    /// The bucket of meta 0 and window 0 holding `rows`, with `edit` applied, as the read of a
    /// time-series turn.
    fn series_read(rows: &[Document], edit: impl FnOnce(&mut Document)) -> Vec<Op> {
        let (id, key) = bucket(0, 0);
        let mut b = series().build(id, Some(&Value::Int64(0)), rows).expect("a bucket");
        edit(&mut b);
        vec![Op::Read(key, Some(b.to_bytes().expect("small")))]
    }

    fn row(id: &str, t: i64) -> Document {
        Document::new().with("_id", id).with("t", Value::DateTime(t))
    }

    fn series_fails(ops: Vec<Op>, why: &str) {
        let v = script().check_series(&committed(ops)).expect_err(why);
        assert_eq!(v.checker, TS_CHECKER);
        assert!(v.detail.contains(why), "{why}: {}", v.detail);
    }

    fn control_of(b: &Document) -> Document {
        match b.get("control") {
            Some(Value::Document(c)) => c.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_series_checks_catch_each_broken_bucket() {
        let good = [row("m1", T0 + 1000), row("m2", T0 + 2000)];
        script().check_series(&committed(series_read(&good, |_| {}))).expect("a good bucket");
        // An absent bucket is skipped.
        let (_, key) = bucket(0, 1);
        script().check_series(&committed(vec![Op::Read(key, None)])).expect("absent");
        let (_, key) = bucket(0, 0);
        series_fails(vec![Op::Read(key, Some(vec![9]))], "an unreadable bucket");
        series_fails(
            series_read(&good, |b| {
                b.insert("data", Value::Int32(1));
            }),
            "does not unpack",
        );
        series_fails(
            series_read(&good, |b| {
                b.insert("meta", Value::Int64(1));
            }),
            "bucket of meta 0 holds meta",
        );
        series_fails(
            series_read(&good, |b| {
                let mut c = control_of(b);
                c.remove("min");
                b.insert("control", c);
            }),
            "without its times",
        );
        // A count above the measurements' columns leaves rows without their fields.
        series_fails(
            series_read(&good, |b| {
                let mut c = control_of(b);
                c.insert("count", Value::Int32(3));
                b.insert("control", c);
            }),
            "a measurement without _id or time",
        );
        series_fails(series_read(&[row("m9", T0)], |_| {}), "measurement m9 not sent");
        series_fails(
            series_read(&[row("m1", T0), row("m1", T0 + 1)], |_| {}),
            "measurement m1 not sent, or twice",
        );
        series_fails(series_read(&[row("m1", T0 + HOUR + 5)], |_| {}), "in the bucket of");
        series_fails(series_read(&[row("m1", T0 - 5)], |_| {}), "in the bucket of");
        for (lo, hi) in [(T0 + 500, T0 + 900), (T0, T0 + 50)] {
            let ops = series_read(&[row("m1", T0 + 100)], |b| {
                let mut c = control_of(b);
                c.insert("min", Document::new().with("t", Value::DateTime(lo)));
                c.insert("max", Document::new().with("t", Value::DateTime(hi)));
                b.insert("control", c);
            });
            series_fails(ops, "in the bucket of");
        }
    }

    #[test]
    fn turns_record_what_committed_and_finish_names_what_is_missing() {
        let mut s = script();
        // No answer, an unreadable one and one for a client without a turn count nothing.
        s.mutated(0, Err(RangeError::Timeout));
        s.mutated(0, Ok(vec![1]));
        let one = MutationResult { n: 1, ..MutationResult::default() }.encode().expect("small");
        s.mutated(0, Ok(one.clone()));
        s.turns.insert(0, Turn::Insert { id: "a".into(), pad: 0 });
        s.turns.insert(1, Turn::TsInsert { id: "m1".into(), meta: 0, t: T0 });
        s.turns.insert(2, Turn::Delete { id: "a".into() });
        let none = MutationResult::default().encode().expect("small");
        s.mutated(0, Ok(none.clone()));
        s.mutated(1, Ok(none));
        s.mutated(2, Ok(one.clone()));
        assert_eq!((s.inserted, s.measured), (0, 0));
        let v = s.finish().expect_err("nothing yet");
        assert_eq!((v.checker, v.liveness), (CHECKER, true));
        s.mutated(0, Ok(one.clone()));
        s.mutated(1, Ok(one));
        assert_eq!((s.inserted, s.measured), (1, 1));
        // A turn that did not commit is not checked; checks that commit are counted.
        s.ended(2, finished(vec![], Outcome::Aborted));
        assert!(!s.turns.contains_key(&2));
        let v = s.finish().expect_err("no check yet");
        assert!(v.detail.contains("1 inserts and 0 checks"), "{}", v.detail);
        s.turns.insert(3, Turn::Check);
        s.ended(3, committed(vec![]));
        let v = s.finish().expect_err("no series check yet");
        assert_eq!(v.checker, TS_CHECKER);
        s.turns.insert(3, Turn::TsCheck);
        s.ended(3, committed(vec![]));
        s.finish().expect("each kind committed");
        // A violation is kept and reported once; later turns are not checked.
        s.turns.insert(3, Turn::Check);
        s.ended(3, committed(vec![Op::Read(state_key(0, COLLECTION), Some(vec![1]))]));
        s.ended(3, committed(vec![]));
        assert_eq!(s.finish().expect_err("kept").checker, CHECKER);
        s.finish().expect("reported once");
    }

    #[test]
    fn healed_turns_check_each_collection_until_one_check_committed() {
        let mut w = World::new(1, 3, 1, false);
        let mut s = script();
        s.healed();
        // The capped check first, then the series one.
        assert_eq!(s.plan(&mut w, 0).len(), 3);
        assert!(matches!(s.turns.get(&0), Some(Turn::Check)));
        s.checked = 1;
        assert!(s.plan(&mut w, 0).len() > 3);
        assert!(matches!(s.turns.get(&0), Some(Turn::TsCheck)));
        // Without a turn or with an unreadable state a client reads nothing more.
        assert!(s.expand(9, &BTreeMap::new()).is_empty());
        s.turns.insert(1, Turn::Check);
        let bad: BTreeMap<_, _> = [(state_key(0, COLLECTION), Some(vec![1]))].into();
        assert!(s.expand(1, &bad).is_empty());
    }

    #[test]
    fn a_run_without_measurements_or_their_checks_fails_liveness() {
        let mut s = script();
        s.inserted = 1;
        s.checked = 1;
        let v = s.finish().expect_err("no measurement");
        assert!(v.liveness && v.detail.contains("0 measurements"), "{}", v.detail);
        s.measured = 1;
        s.series_checked = 1;
        s.finish().expect("both kinds");
    }
}
