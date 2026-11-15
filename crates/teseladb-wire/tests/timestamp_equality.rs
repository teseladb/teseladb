//! A `Timestamp` in a filter matches the document that holds it (found by the differential suite
//! of 002 on 15-11-2026: `{v: Timestamp(1700000000, 5)}` found nothing while `$gt` found it).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "ts";

fn ts(time: u32, increment: u32) -> Value {
    Value::Timestamp { time, increment }
}

fn count(m: &mut Mem, filter: Document) -> i32 {
    let r = m.run(DB, doc(&[("count", Value::from("c")), ("query", Value::Document(filter))]));
    match field(&r, "n") {
        Value::Int32(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

#[test]
fn a_timestamp_in_a_filter_matches_its_document() {
    let mut m = Mem::new();
    let values = [(0, 1), (1, 0), (1_700_000_000, 5), (1_600_000_000, 5), (u32::MAX, u32::MAX)];
    let docs: Vec<Value> = values
        .iter()
        .enumerate()
        .map(|(i, (t, n))| {
            Value::Document(doc(&[("_id", Value::Int32(i as i32)), ("v", ts(*t, *n))]))
        })
        .collect();
    let r = m.run(DB, doc(&[("insert", Value::from("c")), ("documents", Value::Array(docs))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    for (t, n) in values {
        let eq = doc(&[("v", ts(t, n))]);
        assert_eq!(count(&mut m, eq), 1, "{{v: Timestamp({t}, {n})}}");
        let explicit = doc(&[("v", Value::Document(doc(&[("$eq", ts(t, n))])))]);
        assert_eq!(count(&mut m, explicit), 1, "{{v: {{$eq: Timestamp({t}, {n})}}}}");
    }
    // The seconds order first, then the increment.
    let after = doc(&[("v", Value::Document(doc(&[("$gt", ts(1_600_000_000, 9))])))]);
    assert_eq!(count(&mut m, after), 2, "Timestamp(1700000000, 5) and the largest");
    let before = doc(&[("v", Value::Document(doc(&[("$lt", ts(1, 1))])))]);
    assert_eq!(count(&mut m, before), 2, "Timestamp(0, 1) and Timestamp(1, 0)");
}
