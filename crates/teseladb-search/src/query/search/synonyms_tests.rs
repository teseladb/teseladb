//! The checks and states of `synonyms` (011 T036), with the messages the reference search server 1.70.6 gives
//! (measured 02-11-2026, `scratchpad/lane3l/m21.js`). The scores are compared with the reference search server in
//! the recorded `scoreDetails` comparison.

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;
use crate::index::{EQUIVALENT, EXPLICIT};

fn index() -> MemIndex {
    let mapping = Document::new()
        .with("name", "sy")
        .with("analyzer", teseladb_names::ANALYZER_STANDARD)
        .with("source", Document::new().with("collection", "syn"));
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", true))
        .with("synonyms", Value::Array(vec![mapping.into()]));
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, t) in ["gato negro", "minino blanco", "perro"].iter().enumerate() {
        idx.put(&[i as u8], &Document::new().with("t", *t)).expect("indexed");
    }
    idx
}

fn words(all: &[&str]) -> Value {
    Value::Array(all.iter().map(|w| Value::from(*w)).collect())
}

fn text(extra: Document) -> Document {
    let mut t = Document::new().with("path", "t").with("query", "gato");
    for (k, v) in extra.iter() {
        t.insert(k, v.clone());
    }
    Document::new().with("text", t)
}

fn keys(idx: &MemIndex, spec: &Document) -> Result<Vec<u8>, SearchError> {
    Ok(run(idx, spec)?.into_iter().map(|h| h.key[0]).collect())
}

#[test]
fn synonyms_option_is_checked() {
    let idx = index();
    assert_eq!(
        keys(&idx, &text(Document::new().with("synonyms", "nope"))),
        Err(SearchError("unknown synonym mapping name \"nope\"".to_owned()))
    );
    assert_eq!(
        keys(&idx, &text(Document::new().with("synonyms", 5))),
        Err(SearchError("\"text.synonyms\" must be a string".to_owned()))
    );
    assert_eq!(
        keys(&idx, &text(Document::new().with("synonyms", "sy").with("fuzzy", Document::new()))),
        Err(SearchError("\"text\" at most one of [fuzzy, synonyms] may be present".to_owned()))
    );
}

#[test]
fn synonyms_expand_once_loaded_and_fail_while_invalid() {
    let mut idx = index();
    let spec = text(Document::new().with("synonyms", "sy"));
    // Before its collection is read the mapping has no synonyms.
    assert_eq!(keys(&idx, &spec), Ok(vec![0]));
    let docs = vec![
        Document::new()
            .with("mappingType", EQUIVALENT)
            .with("synonyms", words(&["gato", "minino"])),
        Document::new()
            .with("mappingType", EXPLICIT)
            .with("input", words(&["can"]))
            .with("synonyms", words(&["perro"])),
    ];
    idx.set_synonyms("sy", &docs).expect("loaded");
    let found = keys(&idx, &spec).expect("runs");
    assert_eq!(found.len(), 2);
    assert!(found.contains(&0) && found.contains(&1));
    let can = text(Document::new().with("synonyms", "sy").with("query", "can"));
    assert_eq!(keys(&idx, &can), Ok(vec![2]));
    // A mapping the definition does not name is ignored.
    idx.set_synonyms("other", &docs).expect("ignored");
    let mut bad = docs.clone();
    bad.push(Document::new().with("mappingType", "bogus").with("synonyms", words(&["x"])));
    idx.set_synonyms("sy", &bad).expect("loaded");
    assert_eq!(
        keys(&idx, &spec),
        Err(SearchError(
            "synonym mapping \"sy\" is in state INVALID and is not ready to service queries"
                .to_owned()
        ))
    );
    idx.set_synonyms("sy", &docs).expect("reloaded");
    assert_eq!(keys(&idx, &spec).map(|k| k.len()), Ok(2));
}

#[test]
fn phrase_with_synonyms_runs_on_the_expansions() {
    let mut idx = index();
    let docs = vec![
        Document::new()
            .with("mappingType", EQUIVALENT)
            .with("synonyms", words(&["gato", "minino"])),
    ];
    idx.set_synonyms("sy", &docs).expect("loaded");
    let phrase = |q: &str| {
        Document::new().with(
            "phrase",
            Document::new().with("path", "t").with("query", q).with("synonyms", "sy"),
        )
    };
    assert_eq!(keys(&idx, &phrase("minino negro")), Ok(vec![0]));
    assert_eq!(keys(&idx, &phrase("perro")), Ok(vec![2]));
    assert_eq!(keys(&idx, &phrase("blanco perro")), Ok(vec![]));
}
