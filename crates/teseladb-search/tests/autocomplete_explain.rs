//! `scoreDetails` of `autocomplete` (011 FR-003) on a field indexed both for `autocomplete` and as
//! text: the grams and the `text` query add up, `fuzzy` gives one automaton clause per query, and
//! `tokenOrder: "sequential"` keeps the whole query alone. Every `sum of:` node is the sum of its
//! parts and every hit's explanation has the hit's score.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::index::Explanation;
use teseladb_search::indexer::{Found, Indexer};

const DEFINITION: &str = r#"{
  "mappings": {"dynamic": false, "fields": {
    "a": [{"type": "autocomplete", "minGrams": 2, "maxGrams": 7}, {"type": "string"}]
  }}
}"#;

const TEXTS: &[&str] = &["quick brown fox", "quiet night", "brown bear"];

const SUM: &str = "sum of:";

fn index() -> Indexer {
    let def = Definition::parse(&extjson::parse(DEFINITION).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let docs = TEXTS.iter().enumerate().map(|(i, t)| {
        let id = i32::try_from(i).expect("small");
        (vec![u8::try_from(i).expect("small")], Some(Document::new().with("_id", id).with("a", *t)))
    });
    ix.apply(docs).expect("indexed");
    ix
}

fn search(ix: &Indexer, operator: &str) -> Vec<Found> {
    let spec = format!(r#"{{"autocomplete": {operator}, "scoreDetails": true}}"#);
    let mut found = ix.search(&extjson::parse(&spec).expect("JSON")).expect("search");
    found.sort_by_key(|f| f.key[0]);
    found
}

fn keys(found: &[Found]) -> Vec<u8> {
    found.iter().map(|f| f.key[0]).collect()
}

fn sums_add_up(e: &Explanation) {
    if e.description == SUM {
        let total: f32 = e.details.iter().map(|d| d.value).sum();
        assert!((total - e.value).abs() <= 1e-4 * e.value.abs().max(1.0), "{e:?}");
    }
    e.details.iter().for_each(sums_add_up);
}

fn details(f: &Found) -> &Explanation {
    let d = f.details.as_ref().expect("details");
    assert!((d.value - f.score).abs() <= 1e-4 * f.score.abs().max(1.0), "{d:?} {}", f.score);
    sums_add_up(d);
    d
}

fn texts(e: &Explanation, out: &mut Vec<String>) {
    out.push(e.description.clone());
    e.details.iter().for_each(|d| texts(d, out));
}

fn all_texts(e: &Explanation) -> String {
    let mut out = Vec::new();
    texts(e, &mut out);
    out.join("\n")
}

#[test]
fn grams_and_the_text_query_add_up() {
    let ix = index();
    let found = search(&ix, r#"{"query": "quick", "path": "a"}"#);
    assert_eq!(keys(&found), vec![0]);
    let d = details(&found[0]);
    // The grams' term and the `text` query's term, summed.
    assert_eq!(d.description, SUM);
    assert_eq!(d.details.len(), 2, "{d:?}");
    // A prefix that is no word: the grams alone.
    let found = search(&ix, r#"{"query": "qui", "path": "a"}"#);
    assert_eq!(keys(&found), vec![0, 1]);
    found.iter().for_each(|f| {
        details(f);
    });
}

#[test]
fn a_boost_weighs_grams_and_text_alike() {
    let ix = index();
    let plain = search(&ix, r#"{"query": "quick", "path": "a"}"#);
    let boosted =
        search(&ix, r#"{"query": "quick", "path": "a", "score": {"boost": {"value": 2}}}"#);
    assert_eq!(keys(&boosted), vec![0]);
    let (p, b) = (details(&plain[0]), details(&boosted[0]));
    assert!((b.value - 2.0 * p.value).abs() <= 1e-4 * b.value, "{p:?} {b:?}");
}

#[test]
fn fuzzy_gives_one_automaton_clause() {
    let ix = index();
    for order in ["any", "sequential"] {
        let operator = format!(
            r#"{{"query": "quack", "path": "a", "tokenOrder": "{order}", "fuzzy": {{"maxEdits": 1}}}}"#
        );
        let found = search(&ix, &operator);
        assert_eq!(keys(&found), vec![0], "{order}");
        let d = details(&found[0]);
        assert!(all_texts(d).contains("AutomatonQuery"), "{order}: {d:?}");
    }
    // A query shorter than the smallest gram names no term.
    assert!(search(&ix, r#"{"query": "q", "path": "a", "fuzzy": {"maxEdits": 1}}"#).is_empty());
}

#[test]
fn a_sequential_query_is_its_whole_text_alone() {
    let ix = index();
    let found = search(&ix, r#"{"query": "quick bro", "path": "a", "tokenOrder": "sequential"}"#);
    assert_eq!(keys(&found), vec![0]);
    details(&found[0]);
    // Several queries: one part each.
    let found = search(&ix, r#"{"query": ["qui", "bro"], "path": "a"}"#);
    assert_eq!(keys(&found), vec![0, 1, 2]);
    let both = details(&found[0]);
    assert_eq!(both.description, SUM);
    assert_eq!(both.details.len(), 2, "{both:?}");
}
