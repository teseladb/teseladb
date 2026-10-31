//! `$geoWithin` and `$geoIntersects` in the commands without an index (009 T022, FR-012): the
//! matcher asks 009's geometry through the node's extensions (measured on a `refnode` 9.0.4).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "t";

fn ids(r: &Document) -> Vec<i32> {
    let Value::Array(batch) = field(r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    batch
        .iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(n)) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

fn json(s: &str) -> Document {
    extjson::parse(s).unwrap()
}

#[test]
fn geographic_filters_select_without_an_index() {
    let mut m = Mem::new();
    let docs = json(
        r#"{"d": [
            {"_id": 1, "p": [1, 1]},
            {"_id": 2, "p": {"type": "Point", "coordinates": [1, 1]}},
            {"_id": 3, "p": {"type": "Point", "coordinates": [50, 50]}},
            {"_id": 4, "p": "x"},
            {"_id": 5, "p": {"type": "LineString", "coordinates": [[1, 0], [3, 0]]}}
        ]}"#,
    );
    let r = m.run(
        DB,
        Document::new().with("insert", "c").with("documents", docs.get("d").cloned().unwrap()),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(5));
    let find = |m: &mut Mem, filter: &str| {
        m.run(
            DB,
            Document::new()
                .with("find", "c")
                .with("filter", json(filter))
                .with("sort", doc(&[("_id", 1.into())])),
        )
    };
    let square =
        r#"{"type": "Polygon", "coordinates": [[[0, 0], [5, 0], [5, 5], [0, 5], [0, 0]]]}"#;
    let within = find(&mut m, &format!(r#"{{"p": {{"$geoWithin": {{"$geometry": {square}}}}}}}"#));
    assert_eq!(ids(&within), [1, 2], "a line along part of a side is not within");
    let meets =
        find(&mut m, &format!(r#"{{"p": {{"$geoIntersects": {{"$geometry": {square}}}}}}}"#));
    assert_eq!(ids(&meets), [1, 2, 5]);
    let in_box = find(&mut m, r#"{"p": {"$geoWithin": {"$box": [[0, 0], [5, 5]]}}}"#);
    assert_eq!(ids(&in_box), [1, 2]);
    let bad = find(&mut m, r#"{"p": {"$geoWithin": {"$center": [[0, 0], -1]}}}"#);
    assert_eq!(field(&bad, "code"), &Value::Int32(2));
    assert_eq!(field(&bad, "errmsg"), &Value::from("Radius must be a non-negative number: -1"));
    let near = find(&mut m, r#"{"p": {"$near": [1, 1]}}"#);
    assert_eq!(field(&near, "code"), &Value::Int32(291));
}
