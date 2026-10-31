//! Tailable cursors and `$natural` order (015 T008, FR-004), against what a `refrouter` 9.0.4
//! answered on 31-10-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("m", json(cmd))
}

fn ids_of(batch: &Value) -> Vec<Value> {
    let Value::Array(b) = batch else { panic!("{batch:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "_id").clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ints(v: &[i32]) -> Vec<Value> {
    v.iter().map(|i| Value::Int32(*i)).collect()
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("not an error: {r:?}"),
    }
}

fn cursor_id(r: &Document) -> i64 {
    match field(r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?}"),
    }
}

fn more(m: &mut Mem, id: i64, extra: &str) -> Document {
    let mut cmd = doc(&[("getMore", Value::Int64(id)), ("collection", Value::from("cap"))]);
    for (k, v) in json(extra).iter() {
        cmd.push(k, v.clone());
    }
    m.run("m", cmd)
}

fn capped(m: &mut Mem, options: &str, docs: &str) {
    run(m, &format!(r#"{{"create":"cap","capped":true,{options}}}"#));
    if !docs.is_empty() {
        run(m, &format!(r#"{{"insert":"cap","documents":[{docs}]}}"#));
    }
}

#[test]
fn tailable_refuses_what_a_refrouter_refuses() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":10000"#, r#"{"_id":1}"#);
    run(&mut m, r#"{"create":"plain"}"#);
    run(&mut m, r#"{"create":"v","viewOn":"cap","pipeline":[]}"#);
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t"}}"#);
    let cases = [
        (
            r#"{"find":"plain","tailable":true}"#,
            2,
            "error processing query: ns=m.plainTree: $and\nSort: {}\nProj: {}\n tailable cursor requested on non capped collection",
        ),
        (
            r#"{"find":"plain","tailable":true,"filter":{"a":1}}"#,
            2,
            "error processing query: ns=m.plainTree: a $eq 1\nSort: {}\nProj: {}\n tailable cursor requested on non capped collection",
        ),
        (
            r#"{"find":"plain","tailable":true,"filter":{"a":1,"b":{"$gt":2}}}"#,
            2,
            "error processing query: ns=m.plainTree: $and\n    a $eq 1\n    b $gt 2\nSort: {}\nProj: {}\n tailable cursor requested on non capped collection",
        ),
        (
            r#"{"find":"plain","tailable":true,"projection":{"a":1}}"#,
            2,
            "error processing query: ns=m.plainTree: $and\nSort: {}\nProj: { a: 1 }\n tailable cursor requested on non capped collection",
        ),
        (
            r#"{"find":"cap","awaitData":true}"#,
            9,
            "Cannot set 'awaitData' without also setting 'tailable'",
        ),
        (
            r#"{"find":"cap","tailable":true,"sort":{"a":1}}"#,
            2,
            "cannot use tailable option with a sort other than {$natural: 1}",
        ),
        (
            r#"{"find":"cap","tailable":true,"sort":{"$natural":-1}}"#,
            2,
            "cannot use tailable option with a sort other than {$natural: 1}",
        ),
        (
            r#"{"find":"cap","tailable":true,"singleBatch":true}"#,
            2,
            "cannot use tailable option with the 'singleBatch' option",
        ),
        (
            r#"{"find":"v","tailable":true}"#,
            168,
            "Tailable cursors are not supported in aggregation.",
        ),
        (
            r#"{"find":"ts","tailable":true}"#,
            168,
            "Tailable cursors are not supported in aggregation.",
        ),
        (
            r#"{"find":"cap","sort":{"$natural":2}}"#,
            2,
            "$natural sort cannot be set to a value other than -1 or 1.",
        ),
        (
            r#"{"find":"cap","sort":{"$natural":1,"a":1}}"#,
            2,
            "$natural sort cannot be set to a value other than -1 or 1.",
        ),
    ];
    for (cmd, code, msg) in cases {
        assert_eq!(error(&run(&mut m, cmd)), (code, msg.to_owned()), "{cmd}");
    }
    let r = run(&mut m, r#"{"find":"nope","tailable":true}"#);
    assert_eq!(cursor_id(&r), 0);
}

#[test]
fn a_tailable_cursor_follows_the_inserts() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":10000"#, r#"{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}"#);
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"sort":{"$natural":1}}"#);
    let id = cursor_id(&r);
    assert_ne!(id, 0);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[1, 2, 3]));
    let r = more(&mut m, id, "{}");
    assert_eq!((cursor_id(&r), ids_of(field(&r, "cursor.nextBatch"))), (id, vec![]));
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":4}]}"#);
    let r = more(&mut m, id, "{}");
    assert_eq!(ids_of(field(&r, "cursor.nextBatch")), ints(&[4]));
    // `maxTimeMS` is only for `awaitData`.
    let (code, _) = error(&more(&mut m, id, r#"{"maxTimeMS":100}"#));
    assert_eq!(code, 2);
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"awaitData":true}"#);
    let id = cursor_id(&r);
    let r = more(&mut m, id, r#"{"maxTimeMS":100}"#);
    assert_eq!((cursor_id(&r), ids_of(field(&r, "cursor.nextBatch"))), (id, vec![]));
}

#[test]
fn a_filter_skip_and_limit_tail_as_measured() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":10000"#, r#"{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}"#);
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"skip":1}"#);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[2, 3]));
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"limit":1}"#);
    let limited = cursor_id(&r);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[1]));
    let r =
        run(&mut m, r#"{"find":"cap","tailable":true,"filter":{"a":{"$gte":2}},"batchSize":1}"#);
    let filtered = cursor_id(&r);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[2]));
    let r = more(&mut m, filtered, r#"{"batchSize":1}"#);
    assert_eq!(ids_of(field(&r, "cursor.nextBatch")), ints(&[3]));
    let r = more(&mut m, filtered, "{}");
    assert_eq!(ids_of(field(&r, "cursor.nextBatch")), vec![]);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":4},{"_id":5,"a":5},{"_id":6,"a":0}]}"#);
    let r = more(&mut m, filtered, "{}");
    assert_eq!(ids_of(field(&r, "cursor.nextBatch")), ints(&[5]));
    // The limit was reached: the cursor stays open and returns nothing.
    let r = more(&mut m, limited, "{}");
    assert_eq!((cursor_id(&r), ids_of(field(&r, "cursor.nextBatch"))), (limited, vec![]));
    // Nothing matches yet the collection has documents: the cursor stays open.
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"filter":{"zz":1}}"#);
    assert_ne!(cursor_id(&r), 0);
}

#[test]
fn an_empty_capped_collection_gives_no_cursor() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":1000"#, "");
    let r = run(&mut m, r#"{"find":"cap","tailable":true}"#);
    assert_eq!(cursor_id(&r), 0);
}

#[test]
fn a_lost_position_ends_the_cursor() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":4096,"max":2"#, r#"{"_id":1},{"_id":2}"#);
    let r = run(&mut m, r#"{"find":"cap","tailable":true,"batchSize":1}"#);
    let id = cursor_id(&r);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[1]));
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":3},{"_id":4},{"_id":5}]}"#);
    assert_eq!(
        error(&more(&mut m, id, "{}")),
        (136, "CollectionScan died due to position in capped collection being deleted. Last seen record id: RecordId(1)".to_owned())
    );
    assert_eq!(error(&more(&mut m, id, "{}")).0, 43, "killed");
    let r = run(&mut m, r#"{"find":"cap","tailable":true}"#);
    let id = cursor_id(&r);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[4, 5]));
    run(&mut m, r#"{"delete":"cap","deletes":[{"q":{"_id":5},"limit":1}]}"#);
    assert_eq!(
        error(&more(&mut m, id, "{}")),
        (136, "Executor error during getMore :: caused by :: CollectionScan died due to failure to restore tailable cursor position. Last seen record id: RecordId(5)".to_owned())
    );
}

#[test]
fn natural_order_goes_both_ways() {
    let mut m = Mem::new();
    capped(&mut m, r#""size":10000"#, r#"{"_id":3},{"_id":1},{"_id":2}"#);
    let r = run(&mut m, r#"{"find":"cap","sort":{"$natural":-1}}"#);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[2, 1, 3]));
    let r = run(&mut m, r#"{"find":"cap","sort":{"$natural":-1},"skip":1,"limit":1}"#);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[1]));
    let r = run(&mut m, r#"{"find":"cap","sort":{"$natural":1.0}}"#);
    assert_eq!(ids_of(field(&r, "cursor.firstBatch")), ints(&[3, 1, 2]));
}
