//! Filters on several indexes resolved with sets of local identifiers (009 FR-023): an
//! intersection of two indexes, a `$or` over indexes and an `$in` of many values give the
//! documents a collection scan gives, on documents numbered by their range, without numbers, and
//! with both, after inserts, updates and deletes chosen by a seed; and the planner takes the
//! intersection by its estimate when the statistics find it far cheaper.

mod common;

use std::collections::BTreeSet;

use common::{Mem, doc, field};
use teseladb_plan::stats::{FieldStats, RangeStats};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    let r = m.run("p", json(cmd));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
    r
}

fn ids(r: &Document) -> BTreeSet<i32> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "_id") {
                Value::Int32(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

const DOCS: u64 = 400;

fn document(rng: &mut Rng, id: u64) -> String {
    format!(
        r#"{{"_id":{id},"a":{},"b":{},"c":[{},{}]}}"#,
        rng.below(10),
        rng.below(7),
        rng.below(12),
        rng.below(12)
    )
}

/// A collection of `DOCS` documents with indexes on `a`, `b` and the array `c`, then writes chosen
/// by `rng`: updates of indexed and other fields, deletes and inserts back. The writes go with
/// the numbering on or off at random when `mixed`.
fn collection(rng: &mut Rng, mixed: bool) -> Mem {
    let mut m = Mem::new();
    m.unnumbered = mixed && rng.below(2) == 0;
    for spec in [r#"{"key":{"a":1},"name":"a_1"}"#, r#"{"key":{"b":1},"name":"b_1"}"#] {
        run(&mut m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    }
    run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"c":1},"name":"c_1"}]}"#);
    let docs: Vec<String> = (0..DOCS).map(|i| document(rng, i)).collect();
    run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(",")));
    for _ in 0..rng.below(300) {
        if mixed {
            m.unnumbered = rng.below(3) == 0;
        }
        let id = rng.below(DOCS);
        let cmd = match rng.below(5) {
            0 => format!(
                r#"{{"update":"c","updates":[{{"q":{{"_id":{id}}},"u":{{"$set":{{"a":{}}}}}}}]}}"#,
                rng.below(10)
            ),
            1 => format!(
                r#"{{"update":"c","updates":[{{"q":{{"_id":{id}}},"u":{{"$set":{{"x":{}}}}}}}]}}"#,
                rng.below(10)
            ),
            2 => format!(
                r#"{{"update":"c","updates":[{{"q":{{"_id":{id}}},"u":{{"$set":{{"b":{},"c":[{}]}}}}}}]}}"#,
                rng.below(7),
                rng.below(12)
            ),
            3 => format!(r#"{{"delete":"c","deletes":[{{"q":{{"_id":{id}}},"limit":1}}]}}"#),
            _ => format!(
                r#"{{"update":"c","updates":[{{"q":{{"_id":{id}}},"u":{},"upsert":true}}]}}"#,
                document(rng, id)
            ),
        };
        run(&mut m, &cmd);
    }
    m
}

/// The filters the test compares, chosen by `rng`.
fn filters(rng: &mut Rng) -> Vec<String> {
    let (a, b, c) = (rng.below(10), rng.below(7), rng.below(12));
    let many: Vec<String> = (0..rng.below(8) + 2).map(|_| rng.below(12).to_string()).collect();
    vec![
        format!(r#"{{"a":{a},"b":{b}}}"#),
        format!(r#"{{"a":{{"$gte":{a}}},"b":{{"$lte":{b}}}}}"#),
        format!(r#"{{"a":{a},"c":{c}}}"#),
        format!(r#"{{"$or":[{{"a":{a}}},{{"b":{b}}},{{"c":{c}}}]}}"#),
        format!(r#"{{"c":{{"$in":[{}]}}}}"#, many.join(",")),
        format!(r#"{{"c":{{"$in":[{}]}},"b":{b}}}"#, many.join(",")),
        format!(r#"{{"a":{{"$in":[{a},{}]}},"$or":[{{"b":{b}}},{{"c":{c}}}]}}"#, (a + 3) % 10),
    ]
}

fn compare(m: &mut Mem, filter: &str) {
    let planned = run(m, &format!(r#"{{"find":"c","filter":{filter},"batchSize":100000}}"#));
    let scanned = run(
        m,
        &format!(r#"{{"find":"c","filter":{filter},"hint":{{"$natural":1}},"batchSize":100000}}"#),
    );
    assert_eq!(ids(&planned), ids(&scanned), "{filter}");
}

fn hash_intersection(m: &mut Mem, on: bool) {
    let set = doc(&[
        ("setParameter", Value::Int32(1)),
        ("internalQueryPlannerEnableHashIntersection", Value::Boolean(on)),
    ]);
    let r = m.run("admin", set);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn sets_of_numbered_documents_give_what_a_scan_gives() {
    let mut rng = Rng(0x0009_F023_5EED_0001);
    for _ in 0..12 {
        let mut m = collection(&mut rng, false);
        hash_intersection(&mut m, true);
        for f in filters(&mut rng) {
            compare(&mut m, &f);
        }
    }
}

#[test]
fn documents_with_and_without_numbers_give_what_a_scan_gives() {
    let mut rng = Rng(0x0009_F023_5EED_0002);
    for _ in 0..12 {
        let mut m = collection(&mut rng, true);
        hash_intersection(&mut m, true);
        for f in filters(&mut rng) {
            compare(&mut m, &f);
        }
    }
}

/// The statistics of the collection's documents, as a compaction of its range would publish them.
fn publish_stats(m: &Mem) {
    let key =
        m.data.keys().find(|k| teseladb_keys::space::in_documents(k)).cloned().expect("a document");
    let coll = teseladb_keys::space::collection_of(&key).expect("a collection").to_vec();
    let mut values: [Vec<Value>; 3] = Default::default();
    let mut documents = 0;
    for (k, v) in &m.data {
        if !teseladb_keys::space::in_documents(k) {
            continue;
        }
        // The change metas of 014 live beside the documents.
        let Ok(raw) = teseladb_keys::stored::raw(v) else { continue };
        documents += 1;
        let d = Document::from(raw);
        for (i, name) in ["a", "b"].iter().enumerate() {
            values[i].push(d.get(name).cloned().unwrap_or(Value::Null));
        }
        if let Some(Value::Array(c)) = d.get("c") {
            values[2].extend(c.iter().cloned());
        }
    }
    let mut fields = std::collections::BTreeMap::new();
    for (name, v) in ["a", "b", "c"].iter().zip(values) {
        fields.insert((*name).to_owned(), FieldStats::build(v));
    }
    teseladb_plan::stats::publish_range(&coll, 1, RangeStats { documents, fields });
}

#[test]
fn the_planner_intersects_by_its_estimate_and_gives_what_a_scan_gives() {
    // Two conditions that keep a twentieth each: together one document in four hundred.
    let mut m = Mem::new();
    for spec in [r#"{"key":{"a":1},"name":"a_1"}"#, r#"{"key":{"b":1},"name":"b_1"}"#] {
        run(&mut m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    }
    let docs: Vec<String> =
        (0..7000).map(|i| format!(r#"{{"_id":{i},"a":{},"b":{}}}"#, i % 20, i / 20 % 20)).collect();
    for chunk in docs.chunks(1000) {
        run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
    }
    publish_stats(&m);
    let explained = run(
        &mut m,
        r#"{"explain":{"find":"c","filter":{"a":3,"b":4}},"verbosity":"queryPlanner"}"#,
    );
    let plan = format!("{:?}", field(&explained, "queryPlanner.winningPlan"));
    assert!(plan.contains("AND_HASH"), "{plan}");
    compare(&mut m, r#"{"a":3,"b":4}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3,"b":4},"batchSize":100000}"#);
    let want = (0..7000).filter(|i| i % 20 == 3 && i / 20 % 20 == 4).count();
    assert_eq!(ids(&r).len(), want);
    // A condition that keeps almost nothing reads its own index.
    let explained = run(
        &mut m,
        r#"{"explain":{"find":"c","filter":{"a":30,"b":4}},"verbosity":"queryPlanner"}"#,
    );
    let plan = format!("{:?}", field(&explained, "queryPlanner.winningPlan"));
    assert!(!plan.contains("AND_HASH"), "{plan}");
}
