//! `path: {"wildcard": ...}` (011 FR-002): `*` stands for any run of characters, dots included,
//! anywhere in the path, so `t*`, `*.t` and `items.*` name every indexed string field they fit.
//! The grams of `autocomplete` fields are never a string path.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

const DEFINITION: &str = r#"{
  "mappings": {"dynamic": false, "fields": {
    "title": {"type": "string"},
    "tag": {"type": "string"},
    "body": {"type": "document", "fields": {"title": {"type": "string"}}},
    "a": {"type": "autocomplete"},
    "items": {"type": "embeddedDocuments", "fields": {"title": {"type": "string"}, "note": {"type": "string"}}}
  }}
}"#;

fn index() -> Indexer {
    let def = Definition::parse(&extjson::parse(DEFINITION).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let item = |t: &str, n: &str| Document::new().with("title", t).with("note", n).into();
    let docs = vec![
        Document::new().with("_id", 0).with("title", "red fox").with("tag", "zoo").with("a", "red"),
        Document::new()
            .with("_id", 1)
            .with("title", "blue car")
            .with("body", Document::new().with("title", "red sky"))
            .with("items", vec![item("green", "red note")]),
        Document::new()
            .with("_id", 2)
            .with("title", "grey dog")
            .with("items", vec![item("red hat", "plain")]),
    ];
    let keyed =
        docs.into_iter().enumerate().map(|(i, d)| (vec![u8::try_from(i).expect("small")], Some(d)));
    ix.apply(keyed).expect("indexed");
    ix
}

fn hits(ix: &Indexer, spec: &str) -> Vec<u8> {
    let spec = extjson::parse(spec).expect("JSON");
    let mut keys: Vec<u8> = ix.search(&spec).expect("search").iter().map(|f| f.key[0]).collect();
    keys.sort_unstable();
    keys
}

fn text(path: &str) -> String {
    format!(r#"{{"text": {{"query": "red", "path": {{"wildcard": "{path}"}}}}}}"#)
}

#[test]
fn a_star_matches_any_run_of_characters() {
    let ix = index();
    // `title` and nothing else: `body.title` does not start with `t`.
    assert_eq!(hits(&ix, &text("t*")), vec![0]);
    // `title` and `body.title`.
    assert_eq!(hits(&ix, &text("*title")), vec![0, 1]);
    assert_eq!(hits(&ix, &text("body.*")), vec![1]);
    assert_eq!(hits(&ix, &text("*")), vec![0, 1]);
    // No field fits: nothing found, no error.
    assert_eq!(hits(&ix, &text("nope*")), Vec::<u8>::new());
    // Without a star the path is the field itself.
    assert_eq!(hits(&ix, &text("title")), vec![0]);
}

#[test]
fn autocomplete_grams_are_not_string_paths() {
    let ix = index();
    // `a` holds `red` only as `autocomplete` grams.
    assert_eq!(hits(&ix, &text("a*")), Vec::<u8>::new());
}

#[test]
fn a_star_inside_an_embedded_document_names_its_fields() {
    let ix = index();
    let spec = |path: &str| {
        format!(
            r#"{{"embeddedDocument": {{"path": "items", "operator": {{"text": {{"query": "red", "path": {{"wildcard": "{path}"}}}}}}}}}}"#
        )
    };
    assert_eq!(hits(&ix, &spec("items.*")), vec![1, 2]);
    assert_eq!(hits(&ix, &spec("items.t*")), vec![2]);
    assert_eq!(hits(&ix, &spec("items.n*")), vec![1]);
}
