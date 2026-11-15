//! `scoreDetails` of `equals`, `in`, `range` and `near` (011 FR-003) on dates, numbers and a
//! lowercased token: each hit's explanation has the hit's score, its `sum of:` nodes add up, and
//! the query it describes names the values as the field stores them.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::index::Explanation;
use teseladb_search::indexer::{Found, Indexer};

const DEFINITION: &str = r#"{
  "mappings": {"dynamic": false, "fields": {
    "d": {"type": "date"},
    "n": {"type": "number"},
    "k": {"type": "token", "normalizer": "lowercase"}
  }}
}"#;

const DOCS: &[&str] = &[
    r#"{"_id": 0, "d": {"$date": "2026-01-01T00:00:00Z"}, "n": 7, "k": "Red"}"#,
    r#"{"_id": 1, "d": {"$date": "2026-01-02T00:00:00Z"}, "n": 7.5, "k": "blue"}"#,
    r#"{"_id": 2, "d": {"$date": "2026-03-01T00:00:00Z"}, "n": 20, "k": "RED"}"#,
];

const SUM: &str = "sum of:";

fn index() -> Indexer {
    let def = Definition::parse(&extjson::parse(DEFINITION).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let docs = DOCS.iter().enumerate().map(|(i, d)| {
        let doc: Document = extjson::parse(d).expect("JSON");
        (vec![u8::try_from(i).expect("small")], Some(doc))
    });
    ix.apply(docs).expect("indexed");
    ix
}

fn search(ix: &Indexer, operator: &str) -> Vec<Found> {
    let spec = format!(r#"{{{operator}, "scoreDetails": true}}"#);
    let mut found = ix.search(&extjson::parse(&spec).expect("JSON")).expect("search");
    found.sort_by_key(|f| f.key[0]);
    found
}

fn sums_add_up(e: &Explanation) {
    if e.description == SUM {
        let total: f32 = e.details.iter().map(|d| d.value).sum();
        assert!((total - e.value).abs() <= 1e-4 * e.value.abs().max(1.0), "{e:?}");
    }
    e.details.iter().for_each(sums_add_up);
}

fn all_texts(e: &Explanation) -> String {
    let mut out = vec![e.description.clone()];
    out.extend(e.details.iter().map(all_texts));
    out.join("\n")
}

/// The keys of the hits and the text of their explanations, checked for their scores and sums.
fn explained(ix: &Indexer, operator: &str) -> (Vec<u8>, String) {
    let found = search(ix, operator);
    let mut texts = Vec::new();
    for f in &found {
        let d = f.details.as_ref().expect("details");
        assert!((d.value - f.score).abs() <= 1e-4 * f.score.abs().max(1.0), "{operator}: {d:?}");
        sums_add_up(d);
        texts.push(all_texts(d));
    }
    (found.iter().map(|f| f.key[0]).collect(), texts.join("\n"))
}

#[test]
fn equals_on_whole_and_fractional_numbers() {
    let ix = index();
    let (keys, text) = explained(&ix, r#""equals": {"path": "n", "value": 7}"#);
    assert_eq!(keys, vec![0]);
    assert!(text.contains("int64"), "{text}");
    let (keys, text) = explained(&ix, r#""equals": {"path": "n", "value": 7.5}"#);
    assert_eq!(keys, vec![1]);
    // A fraction names no 64-bit integer.
    assert!(!text.contains("int64"), "{text}");
}

#[test]
fn in_names_dates_and_lowercased_tokens() {
    let ix = index();
    let dates = r#""in": {"path": "d", "value": [{"$date": "2026-01-01T00:00:00Z"}, {"$date": "2026-03-01T00:00:00Z"}]}"#;
    let (keys, text) = explained(&ix, dates);
    assert_eq!(keys, vec![0, 2]);
    assert!(text.contains("$type:date/d") && text.contains("$type:dateMultiple/d"), "{text}");
    // The normalizer lowers the query's token as it lowered the stored ones.
    let (keys, text) = explained(&ix, r#""in": {"path": "k", "value": ["RED"]}"#);
    assert_eq!(keys, vec![0, 2]);
    assert!(text.contains("$type:token/k:(red)"), "{text}");
    let (keys, _) = explained(&ix, r#""equals": {"path": "k", "value": "Blue"}"#);
    assert_eq!(keys, vec![1]);
}

#[test]
fn range_between_two_whole_numbers_holds_no_integer() {
    let ix = index();
    let (keys, text) = explained(&ix, r#""range": {"path": "n", "gt": 7.2, "lt": 7.8}"#);
    assert_eq!(keys, vec![1]);
    assert!(!text.contains("int64"), "{text}");
    let (keys, text) = explained(&ix, r#""range": {"path": "n", "gte": 7, "lte": 20}"#);
    assert_eq!(keys, vec![0, 1, 2]);
    assert!(text.contains("int64"), "{text}");
}

#[test]
fn near_on_dates_and_on_boosted_numbers() {
    let ix = index();
    let near_date =
        r#""near": {"path": "d", "origin": {"$date": "2026-01-01T00:00:00Z"}, "pivot": 86400000}"#;
    let (keys, _) = explained(&ix, near_date);
    assert_eq!(keys, vec![0, 1, 2]);
    let found = search(&ix, near_date);
    // The origin itself scores 1, a pivot away a half.
    assert!((found[0].score - 1.0).abs() < 1e-6, "{}", found[0].score);
    assert!((found[1].score - 0.5).abs() < 1e-6, "{}", found[1].score);
    let boosted = r#""near": {"path": "d", "origin": {"$date": "2026-01-01T00:00:00Z"}, "pivot": 86400000, "score": {"boost": {"value": 3}}}"#;
    let found = search(&ix, boosted);
    assert!((found[1].score - 1.5).abs() < 1e-5, "{}", found[1].score);
    explained(&ix, boosted);
    let near_number =
        r#""near": {"path": "n", "origin": 7, "pivot": 1, "score": {"boost": {"value": 2}}}"#;
    let (keys, text) = explained(&ix, near_number);
    assert_eq!(keys, vec![0, 1, 2]);
    assert!(text.contains("product of:") && text.contains("boost"), "{text}");
}
