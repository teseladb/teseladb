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
    assert_eq!(ids(&meets), [1, 2], "a line along part of a side does not meet it either");
    let in_box = find(&mut m, r#"{"p": {"$geoWithin": {"$box": [[0, 0], [5, 5]]}}}"#);
    assert_eq!(ids(&in_box), [1, 2]);
    let bad = find(&mut m, r#"{"p": {"$geoWithin": {"$center": [[0, 0], -1]}}}"#);
    assert_eq!(field(&bad, "code"), &Value::Int32(2));
    assert_eq!(field(&bad, "errmsg"), &Value::from("Radius must be a non-negative number: -1"));
    let near = find(&mut m, r#"{"p": {"$near": [1, 1]}}"#);
    assert_eq!(field(&near, "code"), &Value::Int32(291));
}

/// `$near` and `$nearSphere` in a `find` read nearest first through a `2d` or `2dsphere`
/// index (009 T035; measured on a `refrouter` 9.0.4, `lc12`).
#[test]
fn near_reads_nearest_first_through_a_geo_index() {
    let mut m = Mem::new();
    let docs = json(
        r#"{"d": [
            {"_id": 1, "p": {"type": "Point", "coordinates": [1, 1]}, "q": [1, 1]},
            {"_id": 2, "p": {"type": "Point", "coordinates": [3, 3]}, "q": [3, 4]},
            {"_id": 3, "q": [-2, 0]}
        ]}"#,
    );
    let r = m.run(
        DB,
        Document::new().with("insert", "c").with("documents", docs.get("d").cloned().unwrap()),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    let index = |m: &mut Mem, key: &str, kind: &str| {
        let spec = Document::new().with("key", doc(&[(key, kind.into())])).with("name", key);
        let r = m.run(
            DB,
            Document::new()
                .with("createIndexes", "c")
                .with("indexes", Value::Array(vec![Value::Document(spec)])),
        );
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    };
    let find = |m: &mut Mem, cmd: &str| m.run(DB, json(cmd));
    index(&mut m, "p", "2dsphere");
    let r = find(
        &mut m,
        r#"{"find": "c", "filter": {"p": {"$near": {"$geometry": {"type": "Point",
            "coordinates": [1, 1]}, "$maxDistance": 2000000}}}}"#,
    );
    assert_eq!(ids(&r), [1, 2]);
    let r = find(
        &mut m,
        r#"{"find": "c", "filter": {"p": {"$nearSphere": {"$geometry": {"type": "Point",
            "coordinates": [1, 1]}, "$minDistance": 1000}}}}"#,
    );
    assert_eq!(ids(&r), [2]);
    let legacy = find(&mut m, r#"{"find": "c", "filter": {"p": {"$near": [1, 1]}}}"#);
    assert_eq!(field(&legacy, "code"), &Value::Int32(291));
    assert_eq!(
        field(&legacy, "errmsg"),
        &Value::from(
            "error enumerating plans for query: ns=t.cTree: GEONEAR  field=p \
             maxdist=1.79769e+308 isNearSphere=0\nSort: {}\nProj: {}\n planner returned error \
             :: caused by :: unable to find index for $geoNear query"
        )
    );
    index(&mut m, "q", "2d");
    let r = find(&mut m, r#"{"find": "c", "filter": {"q": {"$near": [0, 0]}}}"#);
    assert_eq!(ids(&r), [1, 3, 2]);
    let r = find(&mut m, r#"{"find": "c", "filter": {"q": {"$near": [0, 0], "$maxDistance": 3}}}"#);
    assert_eq!(ids(&r), [1, 3]);
    let r = find(
        &mut m,
        r#"{"find": "c", "filter": {"q": {"$nearSphere": [0, 0], "$maxDistance": 0.1}}}"#,
    );
    assert_eq!(ids(&r), [1, 3, 2]);
    let r = find(
        &mut m,
        r#"{"find": "c", "filter": {"q": {"$near": [0, 0]}, "_id": {"$gt": 1}},
            "projection": {"q": 0}, "limit": 1}"#,
    );
    assert_eq!(
        field(&r, "cursor.firstBatch"),
        &Value::Array(vec![Value::Document(doc(&[("_id", 3.into())]))])
    );
    let other = find(&mut m, r#"{"find": "c", "filter": {"x": 1, "r": {"$near": [1, 1]}}}"#);
    assert_eq!(field(&other, "code"), &Value::Int32(291));
    let Value::String(msg) = field(&other, "errmsg") else { panic!("{other:?}") };
    assert!(msg.contains("Tree: $and\n    x $eq 1\n    GEONEAR  field=r "), "{msg}");
}
