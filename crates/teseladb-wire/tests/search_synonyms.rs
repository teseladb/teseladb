//! Synonyms read from their collection (011 T036, FR-006): the scores the reference search server
//! 1.70.6 gives on the corpus of `scratchpad/lane3l/m21.js` (measured 02-11-2026), a change of the
//! synonym collection seen by the next query, and the error of a mapping with a bad document.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_search::index::{EQUIVALENT, EXPLICIT};

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn words(all: &[&str]) -> Value {
    Value::Array(all.iter().map(|w| Value::from(*w)).collect())
}

fn insert(m: &mut Mem, coll: &str, docs: Vec<Document>) {
    m.run(DB, Document::new().with("insert", coll).with("documents", arr(docs)));
}

fn scores(m: &mut Mem, q: &str) -> Result<Vec<(i32, f64)>, String> {
    let text = Document::new().with("path", "t").with("query", q).with("synonyms", "sy");
    let project = Document::new()
        .with("$project", Document::new().with("s", Document::new().with("$meta", "searchScore")));
    let pipeline =
        arr(vec![Document::new().with("$search", Document::new().with("text", text)), project]);
    let r = m.run(
        DB,
        Document::new()
            .with("aggregate", "c")
            .with("pipeline", pipeline)
            .with("cursor", Document::new()),
    );
    if let Some(Value::String(e)) = r.get("errmsg") {
        return Err(e.clone());
    }
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    Ok(b.iter()
        .map(|v| {
            let Value::Document(d) = v else { panic!("{v:?}") };
            match (d.get("_id"), d.get("s")) {
                (Some(Value::Int32(id)), Some(Value::Double(s))) => (*id, *s),
                other => panic!("{other:?}"),
            }
        })
        .collect())
}

fn corpus() -> Mem {
    let mut m = Mem::new();
    m.now = 1_793_559_095_123;
    let texts = [
        "gato negro",
        "minino blanco minino",
        "perro can",
        "chucho viejo",
        "gato montés salvaje",
        "felino montés",
        "el can del gato",
    ];
    let docs = texts
        .iter()
        .enumerate()
        .map(|(i, t)| doc(&[("_id", (i as i32 + 1).into()), ("t", (*t).into())]))
        .collect();
    insert(&mut m, "c", docs);
    let synonyms = vec![
        Document::new()
            .with("mappingType", EQUIVALENT)
            .with("synonyms", words(&["gato", "minino"])),
        Document::new()
            .with("mappingType", EXPLICIT)
            .with("input", words(&["can"]))
            .with("synonyms", words(&["perro", "chucho"])),
        Document::new()
            .with("mappingType", EQUIVALENT)
            .with("synonyms", words(&["gato montés", "felino"])),
    ];
    insert(&mut m, "syn", synonyms);
    let mapping = Document::new()
        .with("name", "sy")
        .with("analyzer", teseladb_names::ANALYZER_STANDARD)
        .with("source", Document::new().with("collection", "syn"));
    let definition = Document::new()
        .with("mappings", Document::new().with("dynamic", true))
        .with("synonyms", arr(vec![mapping]));
    let index = Document::new().with("name", "default").with("definition", definition);
    m.run(DB, Document::new().with("createSearchIndexes", "c").with("indexes", arr(vec![index])));
    m
}

#[test]
fn synonyms_come_from_their_collection_with_the_reference_search_scores() {
    let mut m = corpus();
    assert_eq!(
        scores(&mut m, "minino"),
        Ok(vec![
            (2, 0.9993889331817627),
            (1, 0.4133392870426178),
            (5, 0.351778119802475),
            (7, 0.3061772584915161)
        ])
    );
    assert_eq!(
        scores(&mut m, "can"),
        Ok(vec![(3, 1.4185636043548584), (4, 0.8369882106781006), (7, 0.4307965636253357)])
    );
    assert_eq!(
        scores(&mut m, "felino"),
        Ok(vec![(6, 1.8319028615951538), (5, 1.5590662956237793)])
    );
    assert_eq!(scores(&mut m, "minino negro"), Ok(vec![(1, 2.087315797805786)]));
    assert_eq!(scores(&mut m, "can perro"), Ok(vec![]));
}

#[test]
fn a_change_of_the_synonym_collection_is_seen_and_a_bad_document_fails_the_mapping() {
    let mut m = corpus();
    assert_eq!(scores(&mut m, "anciano"), Ok(vec![]));
    let added = Document::new()
        .with("mappingType", EQUIVALENT)
        .with("synonyms", words(&["viejo", "anciano"]));
    insert(&mut m, "syn", vec![added]);
    assert_eq!(scores(&mut m, "anciano").map(|s| s.len()), Ok(1));
    let bad = Document::new().with("_id", 99).with("mappingType", "bogus");
    insert(&mut m, "syn", vec![bad]);
    let invalid = "synonym mapping \"sy\" is in state INVALID and is not ready to service queries";
    assert_eq!(scores(&mut m, "anciano"), Err(invalid.to_owned()));
    let del = Document::new().with("q", Document::new().with("_id", 99)).with("limit", 1);
    m.run(DB, Document::new().with("delete", "syn").with("deletes", arr(vec![del])));
    assert_eq!(scores(&mut m, "anciano").map(|s| s.len()), Ok(1));
}
