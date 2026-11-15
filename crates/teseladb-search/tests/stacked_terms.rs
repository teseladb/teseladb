//! Queries over analyzers that stack several terms at one position (011 FR-002, FR-007):
//! `asciiFolding` with its original tokens gives `cafe` and `café` at one position, and
//! `wordDelimiterGraph` gives `wi`, `fi` and `wifi` for `wi-fi`. A query matches the documents
//! that have any of the stacked terms, whichever form they were written in.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

const DEFINITION: &str = r#"{
  "analyzers": [
    {"name": "folded", "tokenizer": {"type": "whitespace"},
     "tokenFilters": [{"type": "asciiFolding", "originalTokens": "include"}]},
    {"name": "split", "tokenizer": {"type": "whitespace"},
     "tokenFilters": [{"type": "wordDelimiterGraph", "delimiterOptions": {"concatenateWords": true}}]}
  ],
  "mappings": {"dynamic": false, "fields": {
    "f": {"type": "string", "analyzer": "folded"},
    "w": {"type": "string", "analyzer": "split"}
  }}
}"#;

const DOCS: &[(&str, &str)] = &[
    ("café noir", "wi-fi here"),
    ("cafe au lait", "wifi there"),
    ("thé vert", "fi alone"),
    ("café café", "no network"),
];

fn index() -> Indexer {
    let def = Definition::parse(&extjson::parse(DEFINITION).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let docs = DOCS.iter().enumerate().map(|(i, (f, w))| {
        let id = i32::try_from(i).expect("small");
        let key = u8::try_from(i).expect("small");
        (vec![key], Some(Document::new().with("_id", id).with("f", *f).with("w", *w)))
    });
    ix.apply(docs).expect("indexed");
    ix
}

fn hits(ix: &Indexer, spec: &str) -> Vec<u8> {
    let spec = extjson::parse(spec).expect("JSON");
    let mut keys: Vec<u8> = ix.search(&spec).expect("search").iter().map(|f| f.key[0]).collect();
    keys.sort_unstable();
    keys
}

#[test]
fn folded_and_original_forms_match_each_other() {
    let ix = index();
    for q in ["café", "cafe"] {
        let spec = format!(r#"{{"text": {{"query": "{q}", "path": "f"}}}}"#);
        assert_eq!(hits(&ix, &spec), vec![0, 1, 3], "{q}");
        let details =
            format!(r#"{{"text": {{"query": "{q}", "path": "f"}}, "scoreDetails": true}}"#);
        assert_eq!(hits(&ix, &details), vec![0, 1, 3], "{q} with details");
    }
    let phrase = r#"{"phrase": {"query": "cafe noir", "path": "f"}}"#;
    assert_eq!(hits(&ix, phrase), vec![0]);
}

#[test]
fn delimited_words_match_their_parts_and_their_whole() {
    let ix = index();
    // `wi-fi` is two paths, `wi fi` or `wifi`; a path needs every term, so `fi alone` is out.
    assert_eq!(hits(&ix, r#"{"text": {"query": "wi-fi", "path": "w"}}"#), vec![0, 1]);
    assert_eq!(hits(&ix, r#"{"text": {"query": "wifi", "path": "w"}}"#), vec![0, 1]);
    assert_eq!(hits(&ix, r#"{"phrase": {"query": "wi-fi here", "path": "w"}}"#), vec![0]);
}
