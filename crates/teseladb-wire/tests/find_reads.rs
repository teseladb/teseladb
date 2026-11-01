//! Paths of `find` and `getMore` the other tests do not take: time series read backward, skipped
//! and limited, buckets the time bounds rule out, more buckets than one scan reads, and batches
//! cut at 16 MiB.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("q", json(cmd))
}

fn date(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

fn docs_at(r: &Document, path: &str) -> Vec<Document> {
    let Value::Array(b) = field(r, path) else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn values(docs: &[Document]) -> Vec<Value> {
    docs.iter().map(|d| field(d, "v").clone()).collect()
}

fn ints(v: &[i32]) -> Vec<Value> {
    v.iter().map(|n| Value::Int32(*n)).collect()
}

/// Two meta values in the first hour (times 0 to 2 s and 6 s), one more two hours later.
fn series(m: &mut Mem) {
    run(m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let docs = [
        format!(r#"{{"t":{},"m":1,"v":1}}"#, date(T0)),
        format!(r#"{{"t":{},"m":1,"v":2}}"#, date(T0 + 2000)),
        format!(r#"{{"t":{},"m":2,"v":3}}"#, date(T0 + 6000)),
        format!(r#"{{"t":{},"m":1,"v":4}}"#, date(T0 + 7_200_000)),
    ];
    let r = run(m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(4), "{r:?}");
}

#[test]
fn a_time_series_reads_backward_skipped_and_limited() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(&mut m, r#"{"find":"ts","projection":{"_id":0,"v":1}}"#);
    let forward = values(&docs_at(&r, "cursor.firstBatch"));
    assert_eq!(forward.len(), 4);
    let r = run(&mut m, r#"{"find":"ts","hint":{"$natural":-1},"projection":{"_id":0,"v":1}}"#);
    let backward = values(&docs_at(&r, "cursor.firstBatch"));
    assert_eq!(backward, forward.iter().rev().cloned().collect::<Vec<_>>());
    let r = run(&mut m, r#"{"find":"ts","sort":{"v":-1},"skip":1,"limit":2,"projection":{"v":1}}"#);
    assert_eq!(values(&docs_at(&r, "cursor.firstBatch")), ints(&[3, 2]));
}

/// A bucket of the window the scan starts at whose times end before the bound is ruled out
/// before it is unpacked; a limited read stops inside a bucket.
#[test]
fn time_bounds_rule_out_buckets_and_a_limit_stops_inside_one() {
    let mut m = Mem::new();
    series(&mut m);
    let r = run(
        &mut m,
        &format!(
            r#"{{"find":"ts","filter":{{"t":{{"$gte":{}}}}},"sort":{{"v":1}}}}"#,
            date(T0 + 5000)
        ),
    );
    assert_eq!(values(&docs_at(&r, "cursor.firstBatch")), ints(&[3, 4]));
    let r = run(&mut m, r#"{"count":"ts","query":{"m":1},"limit":1}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{"m":1},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
}

/// More buckets than one scan reads: every measurement of each is counted and deleted.
#[test]
fn reads_and_deletes_go_past_one_scan_of_buckets() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let docs: Vec<String> =
        (0..1100).map(|i| format!(r#"{{"t":{},"m":{i},"v":{i}}}"#, date(T0))).collect();
    let r = run(&mut m, &format!(r#"{{"insert":"ts","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(1100), "{r:?}");
    let r = run(&mut m, r#"{"count":"ts","query":{"v":{"$gte":0}}}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1100));
    let r = run(&mut m, r#"{"delete":"ts","deletes":[{"q":{"m":{"$gte":0}},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1100), "{r:?}");
    let r = run(&mut m, r#"{"count":"ts"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(0));
}

/// A batch stops before the document that would take it past 16 MiB, and `getMore` gives the
/// rest.
#[test]
fn batches_stop_at_sixteen_mebibytes() {
    let mut m = Mem::new();
    let big = "x".repeat(3 << 20);
    for i in 0..7 {
        let d = Document::new().with("_id", i).with("s", big.as_str());
        let r = m.run(
            "q",
            Document::new().with("insert", "c").with("documents", vec![Value::Document(d)]),
        );
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    }
    let r = run(&mut m, r#"{"find":"c","projection":{"_id":1,"s":1}}"#);
    let first = docs_at(&r, "cursor.firstBatch");
    assert_eq!(first.len(), 5);
    let Value::Int64(id) = field(&r, "cursor.id") else { panic!("{r:?}") };
    let r = m.run("q", doc(&[("getMore", Value::Int64(*id)), ("collection", Value::from("c"))]));
    let rest = docs_at(&r, "cursor.nextBatch");
    // Documents without a shard key go in the order of their `_id` hash (D-008).
    let mut ids: Vec<i32> = first
        .iter()
        .chain(&rest)
        .map(|d| match field(d, "_id") {
            Value::Int32(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6]);
}
