//! Custom analyzers with the `nGram` tokenizer and filter, `daitchMokotoffSoundex` with its
//! default, `length` and `limitTokenCount` (011 FR-007): the definition is accepted, an integer
//! option may be a whole double, and each analyzer cuts the field and the query alike.

use teseladb_bson::{Document, extjson};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

const DEFINITION: &str = r#"{
  "analyzers": [
    {"name": "ng", "tokenizer": {"type": "nGram", "minGram": 2.0, "maxGram": 3}},
    {"name": "ngf", "tokenizer": {"type": "whitespace"}, "tokenFilters": [{"type": "nGram", "minGram": 2, "maxGram": 2}]},
    {"name": "dm", "tokenizer": {"type": "whitespace"}, "tokenFilters": [{"type": "daitchMokotoffSoundex"}]},
    {"name": "len", "tokenizer": {"type": "whitespace"}, "tokenFilters": [{"type": "length", "min": 2, "max": 4}]},
    {"name": "lim", "tokenizer": {"type": "whitespace"}, "tokenFilters": [{"type": "limitTokenCount", "maxTokenCount": 2}]}
  ],
  "mappings": {"dynamic": false, "fields": {
    "ng": {"type": "string", "analyzer": "ng"},
    "ngf": {"type": "string", "analyzer": "ngf"},
    "dm": {"type": "string", "analyzer": "dm"},
    "len": {"type": "string", "analyzer": "len"},
    "lim": {"type": "string", "analyzer": "lim"}
  }}
}"#;

fn index() -> Indexer {
    let def = Definition::parse(&extjson::parse(DEFINITION).expect("JSON")).expect("definition");
    let mut ix = Indexer::new(def);
    let doc = Document::new()
        .with("_id", 0)
        .with("ng", "abcd")
        .with("ngf", "xyz")
        .with("dm", "Schwarz")
        .with("len", "a hello abc")
        .with("lim", "one two three");
    ix.apply(vec![(vec![0], Some(doc))]).expect("indexed");
    ix
}

fn finds(ix: &Indexer, path: &str, query: &str) -> bool {
    let spec = format!(r#"{{"text": {{"query": "{query}", "path": "{path}"}}}}"#);
    !ix.search(&extjson::parse(&spec).expect("JSON")).expect("search").is_empty()
}

#[test]
fn ngram_tokenizer_and_filter_cut_grams() {
    let ix = index();
    for gram in ["ab", "bcd", "cd"] {
        assert!(finds(&ix, "ng", gram), "{gram}");
    }
    assert!(!finds(&ix, "ng", "ad"));
    assert!(finds(&ix, "ngf", "yz"));
    // The filter's grams are two letters in a row: `xz` is not one of `xyz`.
    assert!(!finds(&ix, "ngf", "xz"));
}

#[test]
fn soundex_keeps_the_original_token_by_default() {
    let ix = index();
    assert!(finds(&ix, "dm", "Schwarz"));
    // A word that sounds alike finds it through its code.
    assert!(finds(&ix, "dm", "Shvarts"));
}

#[test]
fn length_and_token_count_drop_tokens() {
    let ix = index();
    assert!(finds(&ix, "len", "abc"));
    assert!(!finds(&ix, "len", "hello"));
    assert!(!finds(&ix, "len", "a"));
    assert!(finds(&ix, "lim", "two"));
    assert!(!finds(&ix, "lim", "three"));
}
