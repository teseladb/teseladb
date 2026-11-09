//! A `find` of one `_id` by equality goes without the planner (H-013 of the native tests, as
//! RefDb's express path): it answers what the planned read answers for every kind of `_id`,
//! with a sort or without one, and the commands it does not take (a projection, a collation, a
//! `skip`, a `batchSize`) still answer as before.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("x", json(cmd))
}

fn batch(r: &Document) -> Vec<Document> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ids(r: &Document) -> Vec<String> {
    batch(r).iter().map(|d| format!("{:?}", d.get("_id"))).collect()
}

#[test]
fn a_find_by_id_answers_what_the_planned_read_answers() {
    let mut m = Mem::new();
    let docs = r#"[{"_id":1,"v":"int"},{"_id":{"$numberLong":"2"},"v":"long"},{"_id":3.5,"v":"double"},
        {"_id":"s","v":"string"},{"_id":{"$oid":"650000000000000000000001"},"v":"oid"},
        {"_id":true,"v":"bool"},{"_id":{"a":1,"b":2},"v":"doc"},{"_id":null,"v":"null"}]"#;
    let r = run(&mut m, &format!(r#"{{"insert":"c","documents":{docs}}}"#));
    assert_eq!(field(&r, "n"), &Value::Int32(8), "{r:?}");
    let filters = [
        r#"{"_id":1}"#,
        r#"{"_id":1.0}"#,
        r#"{"_id":{"$numberLong":"1"}}"#,
        r#"{"_id":2}"#,
        r#"{"_id":3.5}"#,
        r#"{"_id":"s"}"#,
        r#"{"_id":{"$oid":"650000000000000000000001"}}"#,
        r#"{"_id":true}"#,
        r#"{"_id":{"a":1,"b":2}}"#,
        r#"{"_id":{"b":2,"a":1}}"#,
        r#"{"_id":null}"#,
        r#"{"_id":{"$eq":"s"}}"#,
        r#"{"_id":"missing"}"#,
    ];
    for f in filters {
        // The express path (sort by `_id` and a limit, as YCSB reads) against the planned one (a
        // filter with a second condition that every document meets).
        let express =
            run(&mut m, &format!(r#"{{"find":"c","filter":{f},"sort":{{"_id":1}},"limit":1}}"#));
        let planned_filter = format!(r#"{{"$and":[{f},{{"v":{{"$exists":true}}}}]}}"#);
        let planned = run(&mut m, &format!(r#"{{"find":"c","filter":{planned_filter}}}"#));
        assert_eq!(batch(&express), batch(&planned), "{f}");
        assert_eq!(field(&express, "cursor.id"), &Value::Int64(0), "{f}");
        let plain = run(&mut m, &format!(r#"{{"find":"c","filter":{f}}}"#));
        assert_eq!(batch(&plain), batch(&planned), "{f}");
    }
}

#[test]
fn a_find_by_id_the_express_path_does_not_take_answers_as_before() {
    let mut m = Mem::new();
    run(&mut m, r#"{"insert":"c","documents":[{"_id":"a","v":1},{"_id":"B","v":2}]}"#);
    // A projection.
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":"a"},"projection":{"v":0}}"#);
    assert_eq!(batch(&r), vec![json(r#"{"_id":"a"}"#)]);
    // A collation that makes "b" equal to "B".
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"_id":"b"},"collation":{"locale":"en","strength":2}}"#,
    );
    assert_eq!(ids(&r), vec![format!("{:?}", Some(&Value::String("B".into())))]);
    // A `skip` past the document, and a first batch of none.
    let filter = Value::Document(doc(&[("_id", Value::String("a".into()))]));
    let skip =
        doc(&[("find", Value::String("c".into())), ("filter", filter), ("skip", Value::Int32(1))]);
    assert!(batch(&m.run("x", skip)).is_empty());
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":"a"},"batchSize":0}"#);
    assert!(batch(&r).is_empty());
    assert_ne!(field(&r, "cursor.id"), &Value::Int64(0), "the cursor stays open");
    // A collection with a default collation.
    run(&mut m, r#"{"create":"ci","collation":{"locale":"en","strength":2}}"#);
    run(&mut m, r#"{"insert":"ci","documents":[{"_id":"A","v":1}]}"#);
    assert_eq!(batch(&run(&mut m, r#"{"find":"ci","filter":{"_id":"a"}}"#)).len(), 1);
}
