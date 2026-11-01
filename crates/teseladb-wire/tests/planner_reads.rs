//! Reads the planner answers with the right documents whatever plan wins: indexes it cannot plan
//! with (hashed, partial, sparse, with a collation, hidden), `$or` with a clause no index serves,
//! hash intersections stopped by a limit or a batch, and plans tried against more documents than
//! the trial reads.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

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
