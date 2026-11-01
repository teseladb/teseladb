//! Reads the planner answers with the right documents whatever plan wins: indexes it cannot plan
//! with (hashed, partial, sparse, with a collation, hidden), `$or` with a clause no index serves,
//! hash intersections stopped by a limit or a batch, and plans tried against more documents than
//! the trial reads.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::indexes::{IndexEntry, IndexState};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("p", json(cmd))
}

fn ids(r: &Document) -> Vec<i32> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    let mut out: Vec<i32> = b
        .iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "_id") {
                Value::Int32(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// `count` documents: `_id` i, `a` = i mod 10, `b` = i mod 7, `s` a letter.
fn loaded(count: i32) -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> = (0..count)
        .map(|i| {
            let s = ["x", "Y", "z"][(i % 3) as usize];
            format!(r#"{{"_id":{i},"a":{},"b":{},"s":"{s}"}}"#, i % 10, i % 7)
        })
        .collect();
    for chunk in docs.chunks(1000) {
        let r = run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    m
}

fn index(m: &mut Mem, spec: &str) {
    let r = run(m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{spec}: {r:?}");
}

fn expected(count: i32, keep: impl Fn(i32) -> bool) -> Vec<i32> {
    (0..count).filter(|i| keep(*i)).collect()
}

#[test]
fn indexes_the_planner_cannot_use_still_give_the_matches() {
    let mut m = loaded(70);
    index(&mut m, r#"{"key":{"a":"hashed"},"name":"ah"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"bp","partialFilterExpression":{"b":{"$gt":3}}}"#);
    index(&mut m, r#"{"key":{"s":1},"name":"ss","sparse":true}"#);
    index(&mut m, r#"{"key":{"s":1,"a":1},"name":"sc","collation":{"locale":"en","strength":2}}"#);
    index(&mut m, r#"{"key":{"a":1,"b":1},"name":"ab","hidden":true}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 3));
    let r = run(&mut m, r#"{"find":"c","filter":{"b":{"$gt":5}}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 7 > 5));
    let r = run(&mut m, r#"{"find":"c","filter":{"s":"z","a":{"$lt":2}}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 3 == 2 && i % 10 < 2));
    let r = run(&mut m, r#"{"find":"c","filter":{"a":1,"b":1}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 1 && i % 7 == 1));
}

#[test]
fn or_with_a_clause_no_index_serves_scans_the_collection() {
    let mut m = loaded(70);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"$or":[{"a":2},{"s":"Y"}]}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 2 || i % 3 == 1));
    let r =
        run(&mut m, r#"{"explain":{"find":"c","filter":{"a":2}},"verbosity":"allPlansExecution"}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn hash_intersections_stop_at_a_limit_or_a_batch() {
    let mut m = loaded(700);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let set = doc(&[
        ("setParameter", Value::Int32(1)),
        ("internalQueryPlannerEnableHashIntersection", Value::Boolean(true)),
    ]);
    let r = m.run("admin", set);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let all = expected(700, |i| i % 10 == 4 && i % 7 == 2);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":4,"b":2}}"#);
    assert_eq!(ids(&r), all);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":4,"b":2},"limit":3}"#);
    let got = ids(&r);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|i| all.contains(i)), "{got:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":4},"b":{"$lte":2}},"batchSize":2}"#);
    assert_eq!(ids(&r).len(), 2);
    let r = run(&mut m, r#"{"count":"c","query":{"a":{"$gte":4},"b":{"$lte":2}}}"#);
    let want = expected(700, |i| i % 10 >= 4 && i % 7 <= 2).len();
    assert_eq!(field(&r, "n"), &Value::Int32(i32::try_from(want).unwrap_or(i32::MAX)));
}

/// With more documents than a trial reads, the plans stop at the trial's budget and the winner
/// goes on to the end.
#[test]
fn trials_stop_at_their_budget_on_a_large_collection() {
    let mut m = loaded(12_000);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(&mut m, r#"{"count":"c","query":{"a":{"$gte":0},"b":{"$gte":0}}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(12_000), "{r:?}");
    // Both scans read every key and nothing matches: the trial ends at its budget.
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":0},"b":{"$gte":0},"s":"w"}}"#);
    assert_eq!(ids(&r), Vec::<i32>::new());
    let r = run(&mut m, r#"{"find":"c","filter":{"a":9,"b":6},"projection":{"_id":1}}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b.len(), 101);
}

/// Seeds index `id` named `name` on `key` of collection `c` straight into the node's catalog,
/// with `buckets` and `state` and no entries.
fn seeded(m: &mut Mem, id: u32, key: Document, buckets: u8, state: IndexState) {
    let collection = m.catalog.collections.values().next().expect("c").id;
    let spec =
        Document::new().with("key", key).with("name", format!("i{id}")).with("v", 2).to_bytes();
    let entry = IndexEntry {
        id,
        collection,
        name: format!("i{id}"),
        spec: spec.expect("spec"),
        buckets,
        state,
    };
    m.indexes.last_index = m.indexes.last_index.max(id);
    m.indexes.indexes.insert(id, entry);
}

/// An index still being built, one with hash buckets and a `text` index are left out of the plans:
/// the matches come from the collection, not from an index without entries.
#[test]
fn indexes_being_built_bucketed_or_text_are_not_planned() {
    let mut m = loaded(70);
    seeded(&mut m, 100, Document::new().with("a", 1), 0, IndexState::Building);
    seeded(&mut m, 101, Document::new().with("b", 1), 4, IndexState::Ready);
    index(&mut m, r#"{"key":{"s":"text"},"name":"st"}"#);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 10 == 3));
    let r = run(&mut m, r#"{"find":"c","filter":{"b":2}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 7 == 2));
    let r = run(&mut m, r#"{"find":"c","filter":{"s":"x"}}"#);
    assert_eq!(ids(&r), expected(70, |i| i % 3 == 0));
}

/// An `$or` whose clauses both keep a filter on the fetched document gives once a document both
/// clauses find.
#[test]
fn or_clauses_with_filters_give_a_shared_document_once() {
    let mut m = loaded(300);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"$or":[{"a":1,"s":"x"},{"b":1,"s":"x"}]},"batchSize":300}"#,
    );
    let want = expected(300, |i| (i % 10 == 1 || i % 7 == 1) && i % 3 == 0);
    assert!(want.contains(&141), "a document both clauses find");
    assert_eq!(ids(&r), want);
}

/// Plans that sort in memory stop at the trial's budget too; the winner then sorts everything.
#[test]
fn sorting_plans_stop_at_the_trial_budget() {
    let mut m = loaded(3000);
    index(&mut m, r#"{"key":{"a":1},"name":"a_1"}"#);
    index(&mut m, r#"{"key":{"b":1},"name":"b_1"}"#);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"a":{"$gte":0},"b":{"$gte":0}},"sort":{"s":1,"_id":1},
            "batchSize":3000,"projection":{"_id":1}}"#,
    );
    let got = ids(&r);
    assert_eq!(got.len(), 3000);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    // "Y" sorts before "x" and "z": the first document is the first with `i % 3 == 1`.
    assert_eq!(b.first(), Some(&Value::Document(doc(&[("_id", Value::Int32(1))]))));
}

/// A range of `_id` over more documents than a read of the index of `_id` takes at once in a
/// bucket reads every bucket to its end.
#[test]
fn id_ranges_read_every_chunk_of_every_bucket() {
    let mut m = loaded(6000);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"_id":{"$gte":10}},"batchSize":6000,"projection":{"_id":1}}"#,
    );
    assert_eq!(ids(&r), expected(6000, |i| i >= 10));
}
