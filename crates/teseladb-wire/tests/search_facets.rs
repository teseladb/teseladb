//! Facets through the commands (011 T034, FR-004): `$searchMeta` with the `facet` collector and
//! `$$SEARCH_META` after `$search`, as a 9.0.4 replica set with `refsearch` 1.70.6 answers them on
//! the corpus of `scratchpad/lane3l/m23.js` (measured 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::extjson;
use teseladb_bson::{Document, RawDoc, Value};

const DB: &str = "t";

fn parse(json: &str) -> Document {
    extjson::parse(json).unwrap()
}

fn canonical(v: &Value) -> String {
    let d = Document::new().with("v", v.clone());
    extjson::to_relaxed(RawDoc::from_bytes(&d.to_bytes().unwrap()).unwrap())
}

fn aggregate(m: &mut Mem, pipeline: &str) -> Result<String, String> {
    let wrapped = parse(&format!(r#"{{"p": {pipeline}}}"#));
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", wrapped.get("p").unwrap().clone())
        .with("cursor", Document::new());
    let r = m.run(DB, cmd);
    if let Some(Value::String(e)) = r.get("errmsg") {
        return Err(e.clone());
    }
    Ok(canonical(field(&r, "cursor.firstBatch")))
}

fn corpus() -> Mem {
    let mut m = Mem::new();
    m.now = 1_793_559_095_123;
    let docs = parse(
        r#"{"d": [
         {"_id": 1, "t": "gato negro", "g": "Ficción"},
         {"_id": 2, "t": "gato blanco", "g": "ficción"},
         {"_id": 3, "t": "perro", "g": "Ensayo"},
         {"_id": 4, "t": "gato", "g": "Ensayo"},
         {"_id": 5, "t": "gato gato", "g": 5},
         {"_id": 6, "t": "otro", "g": ["Poesía", "Ensayo"]}]}"#,
    );
    m.run(
        DB,
        Document::new().with("insert", "c").with("documents", docs.get("d").unwrap().clone()),
    );
    let definition = parse(
        r#"{"mappings": {"dynamic": true, "fields": {"g": [{"type": "token"}, {"type": "string"}]}}}"#,
    );
    let index = doc(&[("name", "default".into()), ("definition", definition.into())]);
    let indexes = Value::Array(vec![index.into()]);
    m.run(DB, Document::new().with("createSearchIndexes", "c").with("indexes", indexes));
    m
}

const FACET: &str = r#"{"facet": {"operator": {"text": {"path": "t", "query": "gato"}},
    "facets": {"g": {"type": "string", "path": "g"}}}}"#;

const BUCKETS: &str = r#"{"g": {"buckets": [{"_id": "Ensayo", "count": 1}, {"_id": "Ficción", "count": 1}, {"_id": "ficción", "count": 1}]}}"#;

#[test]
fn search_meta_gives_the_facets() {
    let mut m = corpus();
    let got = aggregate(&mut m, &format!(r#"[{{"$searchMeta": {FACET}}}]"#));
    let want = format!(r#"[{{"count": {{"lowerBound": 4}}, "facet": {BUCKETS}}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
}

#[test]
fn search_meta_variable_follows_a_search() {
    let mut m = corpus();
    let with_count = format!(r#"{}, "count": {{"type": "total"}}}}"#, &FACET[..FACET.len() - 1]);
    let search = format!(r#"{{"$search": {with_count}}}"#);
    let got = aggregate(
        &mut m,
        &format!(
            r#"[{search}, {{"$limit": 1}}, {{"$project": {{"_id": 0, "m": "$$SEARCH_META"}}}}]"#
        ),
    );
    let want = format!(r#"[{{"m": {{"count": {{"total": 4}}, "facet": {BUCKETS}}}}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
    // Inside a `$facet` pipeline too.
    let got = aggregate(
        &mut m,
        &format!(
            r#"[{{"$search": {FACET}}}, {{"$limit": 1}},
               {{"$facet": {{"meta": [{{"$replaceWith": "$$SEARCH_META"}}]}}}}]"#
        ),
    );
    let want = format!(r#"[{{"meta": [{{"count": {{"lowerBound": 4}}, "facet": {BUCKETS}}}]}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
    // Without a `$search` before it (measured: code 6347902).
    let got = aggregate(&mut m, r#"[{"$project": {"m": "$$SEARCH_META"}}]"#);
    assert_eq!(
        got,
        Err("Can't access $$SEARCH_META without a $search stage earlier in the pipeline".to_owned())
    );
}
