//! `$text` with `find` and `aggregate` over a text index (011 FR-001, T029), against a RefDb
//! 9.0.4 replica set (measured 02-11-2026 with `scratchpad/lane3m/m37.js`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "t";

fn parse(json: &str) -> Document {
    extjson::parse(json).expect("json")
}

fn setup() -> Mem {
    let mut m = Mem::new();
    let docs = r#"{"insert": "c", "documents": [
        {"_id": 1, "t": "The Quick brown fox", "l": "english"},
        {"_id": 2, "t": "quick-brown dog"},
        {"_id": 3, "t": "Café au lait, CAFÉ noir"},
        {"_id": 4, "t": "el perro corre rápido", "lang": "spanish"},
        {"_id": 5, "t": "Fox fox FOX"},
        {"_id": 6, "a": {"b": "nested fox"}},
        {"_id": 7, "t": "brown", "u": "quick fox"},
        {"_id": 8, "t": "don't stop"},
        {"_id": 9, "t": "İstanbul ışık"},
        {"_id": 10, "t": "12 345 v2 x1y"}]}"#;
    m.run(DB, parse(docs));
    let index = r#"{"createIndexes": "c", "indexes": [{"key": {"t": "text", "a.b": "text", "u": "text"},
        "name": "t_text", "weights": {"u": 2, "a.b": 5}, "language_override": "lang"}]}"#;
    let r = m.run(DB, parse(index));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m
}

fn batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.firstBatch") {
        Value::Array(b) => b
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{r:?} {other:?}"),
    }
}

/// `_id` and `s` of each document.
fn scored(r: &Document) -> Vec<(i32, f64)> {
    batch(r)
        .iter()
        .map(|d| match (d.get("_id"), d.get("s")) {
            (Some(Value::Int32(id)), Some(Value::Double(s))) => (*id, *s),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn find(m: &mut Mem, filter: &str, sort: &str) -> Document {
    let cmd = format!(
        r#"{{"find": "c", "filter": {filter}, "projection": {{"s": {{"$meta": "textScore"}}}}, "sort": {sort}}}"#
    );
    m.run(DB, parse(&cmd))
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(e))) => (*c, e.clone()),
        _ => panic!("no error: {r:?}"),
    }
}

#[test]
fn find_scores_and_sorts_as_measured() {
    let mut m = setup();
    let by_id = r#"{"_id": 1}"#;
    let fox = vec![(1, 0.6666666666666666), (5, 1.75), (6, 3.75), (7, 1.5)];
    assert_eq!(scored(&find(&mut m, r#"{"$text": {"$search": "fox"}}"#, by_id)), fox);
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "fox"}, "_id": {"$gt": 2}}"#, by_id)),
        vec![(5, 1.75), (6, 3.75), (7, 1.5)]
    );
    assert_eq!(
        scored(&find(&mut m, r#"{"$or": [{"$text": {"$search": "fox"}}, {"_id": 1}]}"#, by_id)),
        fox
    );
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "corre", "$language": "spanish"}}"#, by_id)),
        vec![(4, 0.6666666666666666)]
    );
    let by_score = r#"{"s": {"$meta": "textScore"}}"#;
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "fox quick"}}"#, by_score)),
        vec![(6, 3.75), (7, 3.0), (5, 1.75), (1, 1.3333333333333333), (2, 0.6666666666666666)]
    );
}

#[test]
fn aggregate_gives_the_score_to_the_first_match() {
    let mut m = setup();
    let cmd = r#"{"aggregate": "c", "cursor": {}, "pipeline": [
        {"$match": {"$text": {"$search": "fox"}}},
        {"$project": {"s": {"$meta": "textScore"}}}, {"$sort": {"_id": 1}}]}"#;
    assert_eq!(
        scored(&m.run(DB, parse(cmd))),
        vec![(1, 0.6666666666666666), (5, 1.75), (6, 3.75), (7, 1.5)]
    );
    let later = r#"{"aggregate": "c", "cursor": {}, "pipeline": [
        {"$match": {"_id": {"$gt": 0}}}, {"$match": {"$text": {"$search": "fox"}}}]}"#;
    assert_eq!(
        error(&m.run(DB, parse(later))),
        (17313, "$match with $text is only allowed as the first pipeline stage".to_owned())
    );
}

#[test]
fn text_errors_are_refds() {
    let mut m = setup();
    let cases: Vec<(&str, i32, &str)> = vec![
        (
            r#"{"$text": {"$search": "fox", "$language": "xx"}}"#,
            2,
            "unsupported language: \"xx\" for text index version 3",
        ),
        (
            r#"{"$text": {"$search": 5}}"#,
            14,
            "\"$search\" had the wrong type. Expected string, found int",
        ),
        (r#"{"$text": {"$search": "fox", "$foo": 1}}"#, 2, "extra fields in $text"),
        (r#"{"$text": {}}"#, 4, "Missing expected field \"$search\""),
        (r#"{"$text": "fox"}"#, 2, "$text expects an object"),
        (
            r#"{"$and": [{"$text": {"$search": "fox"}}, {"$text": {"$search": "quick"}}]}"#,
            2,
            "Too many text expressions",
        ),
        (r#"{"$nor": [{"$text": {"$search": "fox"}}]}"#, 2, "text expression not allowed in nor"),
    ];
    for (filter, code, msg) in cases {
        let (c, e) = error(&find(&mut m, filter, r#"{"_id": 1}"#));
        assert_eq!(c, code, "{filter}: {e}");
        assert!(e.ends_with(msg), "{filter}: {e}");
    }
    let text = doc(&[("$search", "x".into())]);
    let missing =
        m.run(DB, doc(&[("find", "d".into()), ("filter", doc(&[("$text", text.into())]).into())]));
    assert_eq!(error(&missing).0, 27);
    let no_meta = m.run(DB, parse(r#"{"find": "c", "projection": {"s": {"$meta": "textScore"}}}"#));
    assert_eq!(error(&no_meta).0, 40218);
}
