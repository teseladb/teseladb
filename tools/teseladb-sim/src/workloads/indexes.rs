//! `indexes` workload (009 T003, FR-003, FR-010, FR-017, SC-001): four documents of one collection
//! with six secondary indexes (simple and multikey, compound with a descending field, sparse,
//! hashed, unique and TTL) over two ranges, the documents in the first and the index entries in the second, so
//! that every write is a transaction across ranges. Each write is evaluated by the code a node runs
//! for a write that spans ranges (`teseladb-wire`'s `Bound` and `cross_range::intent_writes`): the
//! transaction reads the document, and the unique entries the evaluation asks for, at its mark,
//! and writes what comes out. Now and then a client reads, at one mark, every document and every
//! entry its documents can have, and the index-consistency checker compares them. Other turns
//! delete a document as the TTL monitor does, with its filter: the document it deleted must have
//! expired at the turn's time.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_hlc::Hlc;
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::spec::index_def;
use teseladb_index::ttl::Ttl;
use teseladb_keys::{Key, KeyBuilder, KeyParts, PrimaryKey};
use teseladb_range::host::Mutation;
use teseladb_range::intents::IntentOp;
use teseladb_txn::coordinator::Outcome;
use teseladb_wire::commands::cross_range::intent_writes;
use teseladb_wire::commands::index_maintenance::{INDEX_ENTRY, IndexRule};
use teseladb_wire::commands::mutation::{Bound, DOCUMENT, DocMutation, Image, Rules};
use teseladb_wire::commands::ttl::expired_filter;

use crate::checkers::Violation;
use crate::checkers::index_consistency::{self, CHECKER, RangeState};
use crate::workloads::Workload;
use crate::workloads::txn::{Finished, Op, Script, Step, TxnWorkload};
use crate::world::World;

/// The collection.
const COLLECTION: u32 = 7;
/// Where the second range starts: the entries of the collection's first index
/// (`01 · tenant 0 · collection 7 · index 1`).
const CUT: &[u8] = &[0x01, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1];
/// Documents, by `_id`.
const DOCS: i32 = 4;
/// Clients.
const CLIENTS: u32 = 4;
/// Share of the turns that check the indexes instead of writing.
const CHECK_SHARE: f64 = 0.15;
/// Values of `a`: single values and arrays (multikey).
const A_SINGLE: [i32; 3] = [0, 1, 2];
/// Values of `b` (or no `b`: the sparse index leaves the document out).
const B_VALUES: [i32; 2] = [0, 1];
/// Share of the documents with `b`.
const B_SHARE: f64 = 0.7;
/// Values of `c`, which a unique index keeps.
const C_VALUES: [i32; 6] = [0, 1, 2, 3, 4, 5];
/// The mark a write is evaluated at: nothing the workload writes reads it.
const EVAL_MARK: Hlc = Hlc { physical: 1, logical: 0 };
/// Share of the turns that delete as the TTL monitor does.
const TTL_SHARE: f64 = 0.1;
/// The simulated clocks' epoch (`World`), in ms.
const EPOCH_MS: i64 = 1_800_000_000_000;
/// Values of `t`: seconds after the epoch, which expire one second later while the run goes on.
const T_SECONDS: [i64; 6] = [0, 2, 4, 6, 8, 10];
/// `expireAfterSeconds` of the TTL index.
const EXPIRE_SECONDS: i32 = 1;
/// The TTL index.
const TTL_INDEX: u32 = 6;
/// Name of the TTL check.
pub const TTL_CHECKER: &str = "ttl";

/// The indexes: number and specification.
fn specs() -> Vec<(u32, Document)> {
    let key = |k: Document| Document::new().with("key", k);
    vec![
        (1, key(Document::new().with("a", 1)).with("name", "a_1")),
        (2, key(Document::new().with("a", 1).with("b", -1)).with("name", "a_1_b_-1")),
        (3, key(Document::new().with("b", 1)).with("name", "b_1").with("sparse", true)),
        (4, key(Document::new().with("b", "hashed")).with("name", "b_hashed")),
        (5, key(Document::new().with("c", 1)).with("name", "c_1").with("unique", true)),
        (
            TTL_INDEX,
            key(Document::new().with("t", 1))
                .with("name", "t_1")
                .with("expireAfterSeconds", EXPIRE_SECONDS),
        ),
    ]
}

/// Every value `a` takes: each single value and each pair.
fn a_values() -> Vec<Value> {
    let mut out: Vec<Value> = A_SINGLE.iter().map(|v| Value::Int32(*v)).collect();
    for (i, x) in A_SINGLE.iter().enumerate() {
        for y in &A_SINGLE[i + 1..] {
            out.push(Value::Array(vec![Value::Int32(*x), Value::Int32(*y)]));
        }
    }
    out
}

/// The fields of a document besides `_id`.
#[derive(Clone)]
struct Fields {
    a: Value,
    b: Option<i32>,
    c: i32,
    /// Seconds after the epoch of `t`.
    t: i64,
}

/// The date `t` holds.
fn t_value(seconds: i64) -> Value {
    Value::DateTime(EPOCH_MS + seconds * 1000)
}

/// A document with `_id` `id` and `fields`.
fn document(id: i32, f: Fields) -> Document {
    let mut d = Document::new().with("_id", id).with("a", f.a);
    if let Some(b) = f.b {
        d.push("b", b);
    }
    d.with("c", f.c).with("t", t_value(f.t))
}

/// The key of document `id`.
fn doc_key(id: i32) -> Vec<u8> {
    let holder = Document::new().with("_id", id).to_bytes().expect("a small document");
    let raw = RawDoc::from_bytes(&holder).expect("just written");
    KeyBuilder::document(0, COLLECTION, raw.get("_id").expect("just written")).as_bytes().to_vec()
}

/// The primary key of document `id`.
fn primary_key(id: i32) -> PrimaryKey {
    let holder = Document::new().with("_id", id).to_bytes().expect("a small document");
    let raw = RawDoc::from_bytes(&holder).expect("just written");
    PrimaryKey::id(raw.get("_id").expect("just written"))
}

/// What a client is doing.
enum Turn {
    /// A write: the mutation, the keys it read so far and what it writes.
    Write { bound: Box<Bound>, keys: Vec<Vec<u8>>, values: BTreeMap<Vec<u8>, Option<Vec<u8>>> },
    /// A read of everything, for the checker.
    Check,
    /// A delete as the TTL monitor's, at cluster time `now_ms`.
    Ttl { write: Box<Turn>, now_ms: i64 },
}

/// The script of `indexes`.
pub struct Indexes {
    rules: Rules,
    set: IndexSet,
    /// Every entry key the documents can have, with the key itself.
    entries: BTreeMap<Vec<u8>, Key>,
    turns: BTreeMap<u32, Turn>,
    /// Nodes of the world, whose clocks the TTL turns read.
    nodes: u32,
    /// The fault window ended.
    healed: bool,
    /// Checks and writes that committed.
    checked: u64,
    written: u64,
    violation: Option<Violation>,
}

impl Indexes {
    /// The script, with every entry key its documents can have.
    pub fn new() -> Indexes {
        let mut rules = Vec::new();
        let mut defs = Vec::new();
        for (id, spec) in specs() {
            let bytes = spec.to_bytes().expect("a small specification");
            let raw = RawDoc::from_bytes(&bytes).expect("just written");
            let def = index_def(raw, 0).expect("a valid specification").expect("not text");
            defs.push((id, def));
            rules.push(IndexRule { id, spec, buckets: 0, delete_only: false });
        }
        let set = IndexSet { tenant: 0, collection: COLLECTION, indexes: defs };
        let mut entries = BTreeMap::new();
        for id in 0..DOCS {
            let pk = primary_key(id);
            for a in a_values() {
                for b in B_VALUES.iter().map(|b| Some(*b)).chain([None]) {
                    for (c, t) in C_VALUES.iter().flat_map(|c| T_SECONDS.map(|t| (*c, t))) {
                        let f = Fields { a: a.clone(), b, c, t };
                        let bytes = document(id, f).to_bytes().expect("a small document");
                        let doc = RawDoc::from_bytes(&bytes).expect("just written");
                        let keys = set.keys(Version { doc, pk: &pk }, None).expect("indexable");
                        entries.extend(keys.into_keys().map(|k| (k.as_bytes().to_vec(), k)));
                    }
                }
            }
        }
        let rules = Rules {
            ns: "sim.indexes".into(),
            collection: COLLECTION,
            indexes: rules,
            ..Rules::default()
        };
        Indexes {
            rules,
            set,
            entries,
            turns: BTreeMap::new(),
            nodes: 1,
            healed: false,
            checked: 0,
            written: 0,
            violation: None,
        }
    }

    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        let script = Indexes { nodes: nodes.max(1), ..Indexes::new() };
        let w = TxnWorkload::new("indexes", (nodes, cores), CLIENTS, script);
        Box::new(w.with_cut(CUT))
    }

    /// The TTL monitor's delete of document `id` at cluster time `now_ms`, with its filter.
    fn expired(&self, id: i32, now_ms: i64) -> DocMutation {
        let (_, spec) = specs().into_iter().find(|(i, _)| *i == TTL_INDEX).expect("listed");
        let bytes = spec.to_bytes().expect("a small specification");
        let ttl = Ttl::of(RawDoc::from_bytes(&bytes).expect("just written")).expect("a TTL index");
        let filter = expired_filter(&ttl, now_ms).with("_id", id);
        DocMutation::Delete { filter, multi: false, image: Image::None, rules: self.rules.clone() }
    }

    /// A random write of document `id`: an insert, a replacement or a delete.
    fn mutation(&self, w: &mut World, id: i32) -> DocMutation {
        let values = a_values();
        let a = values[w.rng.range(0, values.len() as u64) as usize].clone();
        let b =
            w.rng.chance(B_SHARE).then(|| B_VALUES[w.rng.range(0, B_VALUES.len() as u64) as usize]);
        let c = C_VALUES[w.rng.range(0, C_VALUES.len() as u64) as usize];
        let t = T_SECONDS[w.rng.range(0, T_SECONDS.len() as u64) as usize];
        let doc = document(id, Fields { a, b, c, t });
        let filter = Document::new().with("_id", id);
        let rules = self.rules.clone();
        match w.rng.range(0, 3) {
            0 => DocMutation::Insert { docs: vec![doc], ordered: true, rules },
            1 => {
                let mut replacement = doc;
                replacement.remove("_id");
                DocMutation::Update {
                    filter,
                    update: Value::Document(replacement),
                    array_filters: None,
                    multi: false,
                    upsert: None,
                    image: Image::None,
                    rules,
                }
            }
            _ => DocMutation::Delete { filter, multi: false, image: Image::None, rules },
        }
    }

    /// The state a check read, for the checker.
    fn state(&self, done: &Finished) -> RangeState {
        let mut docs = Vec::new();
        let mut entries = Vec::new();
        let doc_keys: BTreeMap<Vec<u8>, i32> = (0..DOCS).map(|id| (doc_key(id), id)).collect();
        for op in &done.ops {
            let Op::Read(key, Some(value)) = op else { continue };
            if let Some(id) = doc_keys.get(key) {
                docs.push((value.clone(), primary_key(*id)));
            } else if let Some(k) = self.entries.get(key) {
                entries.push((k.clone(), value.clone()));
            }
        }
        let ready = self.set.indexes.iter().map(|(id, _)| (*id, Some(EVAL_MARK))).collect();
        RangeState {
            indexes: self.set.clone(),
            ready,
            at: done.read_ts.max(EVAL_MARK),
            docs,
            entries,
            multikey_paths: BTreeMap::new(),
        }
    }
}

impl Default for Indexes {
    fn default() -> Indexes {
        Indexes::new()
    }
}

impl Script for Indexes {
    fn plan(&mut self, w: &mut World, client: u32) -> Vec<Step> {
        // Once a write has committed, and again once the faults stop, every turn checks until one
        // check has committed: no seed ends unchecked, also one without faults, which never heals.
        let unchecked = (self.healed || self.written > 0) && self.checked == 0;
        if unchecked || w.rng.chance(CHECK_SHARE) {
            self.turns.insert(client, Turn::Check);
            let docs = (0..DOCS).map(doc_key);
            let mut steps: Vec<Step> =
                docs.chain(self.entries.keys().cloned()).map(Step::Read).collect();
            steps.push(Step::Commit);
            return steps;
        }
        let id = w.rng.range(0, DOCS as u64) as i32;
        let key = doc_key(id);
        let ttl = w.rng.chance(TTL_SHARE);
        // The cluster time of a pass: the clock of a node, as the monitor reads it.
        let now_ms = i64::try_from(w.wall_ms(client % self.nodes)).unwrap_or(i64::MAX);
        let mutation = if ttl { self.expired(id, now_ms) } else { self.mutation(w, id) };
        let bound = Box::new(Bound { mutation, keys: vec![key.clone()] });
        let keys = bound.latch_keys();
        let write = Turn::Write { bound, keys, values: BTreeMap::new() };
        let turn = if ttl { Turn::Ttl { write: Box::new(write), now_ms } } else { write };
        self.turns.insert(client, turn);
        vec![Step::Read(key), Step::Expand, Step::Commit]
    }

    fn expand(&mut self, client: u32, read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<Step> {
        let turn = match self.turns.get_mut(&client) {
            Some(Turn::Ttl { write, .. }) => write.as_mut(),
            Some(t) => t,
            None => return Vec::new(),
        };
        let Turn::Write { bound, keys, values } = turn else { return Vec::new() };
        let current: Vec<Option<(u8, &[u8])>> = keys
            .iter()
            .map(|k| read.get(k).and_then(|v| v.as_deref()).map(|v| (value_type_of(k), v)))
            .collect();
        // A write the evaluation refuses writes nothing; the transaction commits empty.
        let Ok(outcome) = bound.eval_with(keys, &current, EVAL_MARK) else { return Vec::new() };
        let unread: Vec<Vec<u8>> =
            bound.reads(&outcome).into_iter().filter(|k| !keys.contains(k)).collect();
        if !unread.is_empty() {
            keys.extend(unread.iter().cloned());
            let mut steps: Vec<Step> = unread.into_iter().map(Step::Read).collect();
            steps.push(Step::Expand);
            return steps;
        }
        let writes = intent_writes(outcome);
        *values = writes
            .iter()
            .map(|(k, op)| {
                let v = match op {
                    IntentOp::Put { value, .. } => Some(value.clone()),
                    IntentOp::Delete => None,
                };
                (k.clone(), v)
            })
            .collect();
        writes.into_iter().map(|(k, _)| Step::Write(k)).collect()
    }

    fn value(
        &mut self,
        client: u32,
        key: &[u8],
        _read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>> {
        let turn = match self.turns.get(&client) {
            Some(Turn::Ttl { write, .. }) => Some(write.as_ref()),
            t => t,
        };
        match turn {
            Some(Turn::Write { values, .. }) => values.get(key).cloned().flatten(),
            Some(Turn::Check | Turn::Ttl { .. }) | None => None,
        }
    }

    fn healed(&mut self) {
        self.healed = true;
    }

    fn value_type(&self, key: &[u8]) -> u8 {
        value_type_of(key)
    }

    fn ended(&mut self, client: u32, done: Finished) {
        let turn = self.turns.remove(&client);
        if !matches!(done.outcome, Outcome::Committed(_)) {
            return;
        }
        match turn {
            Some(Turn::Check) => {
                self.checked += 1;
                if self.violation.is_none()
                    && let Err(v) = index_consistency::check(&self.state(&done))
                {
                    self.violation = Some(v);
                }
            }
            Some(Turn::Write { values, .. }) if !values.is_empty() => self.written += 1,
            Some(Turn::Ttl { now_ms, .. }) => {
                if self.violation.is_none()
                    && let Err(v) = expired_when_deleted(&done, now_ms)
                {
                    self.violation = Some(v);
                }
            }
            Some(Turn::Write { .. }) | None => {}
        }
    }

    fn finish(&mut self) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        if self.written == 0 || self.checked == 0 {
            let what = format!("{} writes and {} checks committed", self.written, self.checked);
            return Err(Violation::liveness(CHECKER, what));
        }
        Ok(())
    }
}

/// A delete as the TTL monitor's took only a document whose `t` had expired at `now_ms` (FR-010):
/// `t` plus [`EXPIRE_SECONDS`] at or before it.
fn expired_when_deleted(done: &Finished, now_ms: i64) -> Result<(), Violation> {
    let deleted: BTreeSet<&Vec<u8>> = done
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Write(k, None) if value_type_of(k) == DOCUMENT => Some(k),
            _ => None,
        })
        .collect();
    for op in &done.ops {
        let Op::Read(key, Some(bytes)) = op else { continue };
        if !deleted.contains(key) {
            continue;
        }
        let fail = |what: String| Violation::safety(TTL_CHECKER, what);
        let doc = teseladb_keys::stored::bson(bytes)
            .and_then(RawDoc::from_bytes)
            .map_err(|e| fail(format!("a document: {e:?}")))?;
        let Some(teseladb_bson::RawValue::DateTime(t)) = doc.get("t") else {
            return Err(fail(format!("a TTL delete took a document without a date: {doc:?}")));
        };
        let expires = t.saturating_add(i64::from(EXPIRE_SECONDS) * 1000);
        if expires > now_ms {
            let what =
                format!("a TTL delete at {now_ms} took a document that expires at {expires}");
            return Err(fail(what));
        }
    }
    Ok(())
}

/// The value type stored at `key`: a document or an index entry.
fn value_type_of(key: &[u8]) -> u8 {
    match KeyParts::of(key) {
        Ok(KeyParts::Data { index: 0, .. }) => DOCUMENT,
        _ => INDEX_ENTRY,
    }
}

/// The entry keys of every document (for tests).
pub fn entry_keys() -> BTreeSet<Vec<u8>> {
    Indexes::new().entries.into_keys().collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_types::Nanos;

    use super::*;

    fn done(ops: Vec<Op>, outcome: Outcome) -> Finished {
        Finished {
            ops,
            read_ts: EVAL_MARK,
            outcome,
            began: Nanos::ZERO,
            acked: None,
            in_window: true,
        }
    }

    fn doc_at(t: Option<i64>) -> Vec<u8> {
        let d = Document::new().with("_id", 0);
        let d = match t {
            Some(t) => d.with("t", Value::DateTime(t)),
            None => d,
        };
        d.to_bytes().expect("small")
    }

    fn ttl_delete(t: Option<i64>) -> Vec<Op> {
        vec![Op::Read(doc_key(0), Some(doc_at(t))), Op::Write(doc_key(0), None)]
    }

    #[test]
    fn a_ttl_delete_of_a_document_not_expired_or_without_a_date_fails() {
        let now = EPOCH_MS + 10_000;
        let committed = Outcome::Committed(EVAL_MARK);
        expired_when_deleted(&done(ttl_delete(Some(now - 5_000)), committed.clone()), now)
            .expect("expired");
        let v = expired_when_deleted(&done(ttl_delete(None), committed.clone()), now)
            .expect_err("no date");
        assert!(v.detail.contains("without a date"), "{}", v.detail);
        let v = expired_when_deleted(&done(ttl_delete(Some(now)), committed.clone()), now)
            .expect_err("live");
        assert_eq!(v.checker, TTL_CHECKER);
        let bad = vec![Op::Read(doc_key(0), Some(vec![1])), Op::Write(doc_key(0), None)];
        expired_when_deleted(&done(bad, committed.clone()), now).expect_err("unreadable");
        // The script keeps the first violation, and finish reports it.
        let mut s = Indexes::new();
        let write = Turn::Write {
            bound: Box::new(Bound { mutation: s.expired(0, now), keys: vec![] }),
            keys: vec![],
            values: BTreeMap::new(),
        };
        s.turns.insert(0, Turn::Ttl { write: Box::new(write), now_ms: now });
        s.ended(0, done(ttl_delete(Some(now)), committed.clone()));
        s.turns.insert(0, Turn::Ttl { write: Box::new(Turn::Check), now_ms: now });
        s.ended(0, done(ttl_delete(None), committed));
        let v = s.finish().expect_err("kept");
        assert!(v.detail.contains("expires at"), "{}", v.detail);
    }

    #[test]
    fn checks_judge_what_they_read_and_finish_wants_both_kinds() {
        let mut s = Indexes::new();
        let committed = Outcome::Committed(EVAL_MARK);
        let v = s.finish().expect_err("nothing committed");
        assert!(v.liveness);
        // A check that read a key it does not know, and one whose document cannot be read.
        s.turns.insert(1, Turn::Check);
        s.ended(1, done(vec![Op::Read(vec![9, 9], Some(vec![1]))], committed.clone()));
        assert_eq!((s.checked, s.violation.is_none()), (1, true));
        s.turns.insert(1, Turn::Check);
        s.ended(1, done(vec![Op::Read(doc_key(1), Some(vec![1]))], committed.clone()));
        let first = s.violation.clone().expect("an unreadable document");
        s.turns.insert(1, Turn::Check);
        s.ended(1, done(vec![Op::Read(doc_key(1), Some(vec![2]))], committed));
        assert_eq!(s.violation, Some(first));
        assert!(s.finish().is_err());
        let v = s.finish().expect_err("no write committed");
        assert!(v.detail.contains("0 writes and 3 checks"), "{}", v.detail);
    }

    #[test]
    fn healed_turns_check_and_checks_expand_to_nothing() {
        let mut s = Indexes::new();
        let mut w = World::new(1, 3, 1, false);
        s.healed();
        let steps = s.plan(&mut w, 0);
        assert!(matches!(s.turns.get(&0), Some(Turn::Check)));
        assert!(steps.len() > DOCS as usize && matches!(steps.last(), Some(Step::Commit)));
        assert!(s.expand(0, &BTreeMap::new()).is_empty());
        assert!(s.expand(7, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_run_without_writes_or_checks_fails_liveness() {
        let v = Indexes::default().finish().expect_err("nothing committed");
        assert!(v.liveness, "{}", v.detail);
    }
}
