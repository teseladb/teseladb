//! The `facet` collector of `$searchMeta` (011 FR-004, T034, T047): each check of its fields
//! with the message `query/search/facet.rs` gives, and the counts of string, number and date
//! facets over documents with arrays, missing values and values outside every range.

use teseladb_bson::extjson;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

fn parse(json: &str) -> Document {
    extjson::parse(json).unwrap()
}

fn index() -> Indexer {
    let def = parse(
        r#"{"mappings": {"dynamic": true, "fields": {
            "g": [{"type": "token"}, {"type": "string"}],
            "n": [{"type": "number"}],
            "d": [{"type": "date"}]}}}"#,
    );
    let mut ix = Indexer::new(Definition::parse(&def).expect("valid"));
    let docs = parse(
        r#"{"d": [
          {"t": "gato", "g": "uno", "n": 1, "d": {"$date": "2026-01-01T00:00:00Z"}},
          {"t": "gato", "g": ["uno", "dos", "uno"], "n": [5, 25]},
          {"t": "gato", "g": "dos", "n": 99, "d": {"$date": "2027-06-01T00:00:00Z"}},
          {"t": "gato", "n": 12.5},
          {"t": "perro", "g": "tres", "n": 3}]}"#,
    );
    let Some(Value::Array(docs)) = docs.get("d") else { panic!("docs") };
    let changes = docs.iter().enumerate().map(|(i, d)| {
        let Value::Document(d) = d else { panic!("a document") };
        (vec![i as u8], Some(d.clone()))
    });
    ix.apply(changes.collect::<Vec<_>>()).expect("indexed");
    ix
}

fn meta(ix: &Indexer, facet: &str) -> Result<String, String> {
    let spec = parse(&format!(r#"{{"facet": {facet}}}"#));
    let d = ix.meta(&spec).map_err(|e| e.0)?;
    Ok(extjson::to_relaxed(RawDoc::from_bytes(&d.to_bytes().unwrap()).unwrap()))
}

const GATO: &str = r#""operator": {"text": {"path": "t", "query": "gato"}}"#;

#[test]
fn the_fields_of_the_collector_are_checked() {
    let ix = index();
    let err = |f: &str| meta(&ix, f).err().unwrap_or_else(|| panic!("{f} is accepted"));
    assert_eq!(err("3"), r#""facet" must be a document"#);
    assert_eq!(err(r#"{"x": 1, "facets": {}}"#), r#""facet" unrecognized field "x""#);
    assert_eq!(err(r#"{"operator": 1, "facets": {}}"#), r#""facet.operator" must be a document"#);
    assert_eq!(err(r#"{}"#), r#""facet.facets" is required"#);
    assert_eq!(err(r#"{"facets": {}}"#), r#""facet.facets" cannot be empty"#);
    assert_eq!(err(r#"{"facets": 2}"#), r#""facet.facets" must be a document"#);
    let one = |f: &str| err(&format!(r#"{{"facets": {{"f": {f}}}}}"#));
    assert_eq!(one("1"), r#""facet.facets.f" must be a document"#);
    assert_eq!(one("{}"), r#""facet.facets.f.type" is required"#);
    assert_eq!(
        one(r#"{"type": "geo"}"#),
        r#""facet.facets.f.type" must be one of [date, number, string]"#
    );
    assert_eq!(one(r#"{"type": "string"}"#), r#""facet.facets.f.path" is required"#);
    assert_eq!(
        one(r#"{"type": "string", "path": 1}"#),
        r#""facet.facets.f.path" must be a string"#
    );
    assert_eq!(
        one(r#"{"type": "string", "path": "g", "numBuckets": "x"}"#),
        r#""facet.facets.f.numBuckets" must be a integer"#
    );
    assert_eq!(
        one(r#"{"type": "string", "path": "g", "numBuckets": {"$numberLong": "1001"}}"#),
        r#""facet.facets.f.numBuckets" must be within bounds [1..1000]"#
    );
    assert_eq!(
        one(r#"{"type": "string", "path": "g", "numBuckets": 0}"#),
        r#""facet.facets.f.numBuckets" must be within bounds [1..1000]"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n"}"#),
        r#""facet.facets.f.boundaries" is required"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n", "boundaries": 1}"#),
        r#""facet.facets.f.boundaries" must be an array"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n", "boundaries": [1, "a"]}"#),
        r#""facet.facets.f.boundaries[1]" must be a int32, int64, or double"#
    );
    assert_eq!(
        one(r#"{"type": "date", "path": "d", "boundaries": [1, 2]}"#),
        r#""facet.facets.f.boundaries[0]" must be a date time"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n", "boundaries": [1]}"#),
        r#""facet.facets.f.boundaries" size must be within bounds: [2..1000]"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n", "boundaries": [5, 5]}"#),
        r#""facet.facets.f.boundaries" must be distinct numbers in ascending order"#
    );
    assert_eq!(
        one(r#"{"type": "number", "path": "n", "boundaries": [1, 5], "default": 3}"#),
        r#""facet.facets.f.default" must be a string"#
    );
    // Dynamic mappings index numbers and dates on every path, but strings only as text.
    assert_eq!(
        one(r#"{"type": "string", "path": "t"}"#),
        r#"Cannot facet on field "t" "" because it was not indexed as a "token" field."#
    );
}

#[test]
fn facets_count_each_document_once_per_bucket() {
    let ix = index();
    // `uno` counts the second document once though it holds it twice; buckets go by count,
    // then by value in ascending order.
    let got = meta(
        &ix,
        &format!(
            r#"{{{GATO}, "facets": {{"g": {{"type": "string", "path": "g", "numBuckets": 2}}}}}}"#
        ),
    );
    assert_eq!(
        got.unwrap(),
        r#"{"count":{"lowerBound":4},"facet":{"g":{"buckets":[{"_id":"dos","count":2},{"_id":"uno","count":2}]}}}"#
    );
    // A document with a value in a range and one in none counts in the range only; `default`
    // counts the documents in none.
    let ranges =
        r#"{"type": "number", "path": "n", "boundaries": [0, 10, 20], "default": "otros"}"#;
    let got = meta(&ix, &format!(r#"{{{GATO}, "facets": {{"n": {ranges}}}}}"#)).unwrap();
    assert_eq!(
        got,
        r#"{"count":{"lowerBound":4},"facet":{"n":{"buckets":[{"_id":0,"count":2},{"_id":10,"count":1},{"_id":"otros","count":1}]}}}"#
    );
    let dates = r#"{"type": "date", "path": "d", "boundaries": [{"$date": "2026-01-01T00:00:00Z"}, {"$date": "2027-01-01T00:00:00Z"}]}"#;
    let got = meta(&ix, &format!(r#"{{"facets": {{"d": {dates}}}}}"#)).unwrap();
    assert_eq!(
        got,
        r#"{"count":{"lowerBound":5},"facet":{"d":{"buckets":[{"_id":{"$date":"2026-01-01T00:00:00Z"},"count":1}]}}}"#
    );
}
