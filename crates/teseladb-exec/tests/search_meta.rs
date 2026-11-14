//! The metadata of the search stages through a pipeline (011 FR-022, T026): what `$meta` reads
//! after `$search`, `$vectorSearch` and `$searchMeta`, and the errors of a 9.0.4 deployment with
//! the reference search server 1.70.6 (measured 01-11-2026; the errors without search on a reference router 9.0.4 too).

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::search::Scored;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// A node whose search stages give two documents with their scores.
struct SearchEnv;

fn scored(id: i32, name: &str, score: f64) -> Scored {
    let mut meta = BTreeMap::new();
    meta.insert(name.to_owned(), Value::Double(score));
    Scored { doc: Document::new().with("_id", id), meta }
}

impl Env for SearchEnv {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(vec![Document::new().with("_id", 9)])
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn search_source(&self, stage: &str, _spec: &Document) -> Result<Vec<Scored>, MqlError> {
        Ok(match stage {
            "$search" => vec![scored(2, "searchScore", 0.5), scored(1, "searchScore", 0.25)],
            "$vectorSearch" => {
                vec![scored(1, "vectorSearchScore", 1.0), scored(2, "vectorSearchScore", 0.5)]
            }
            _ => vec![Scored {
                doc: Document::new().with("count", Document::new().with("lowerBound", 2i64)),
                meta: BTreeMap::new(),
            }],
        })
    }
}

fn run(stages: Vec<Document>) -> Result<Vec<Document>, MqlError> {
    let bytes: Vec<Vec<u8>> = stages.iter().map(|s| s.to_bytes().expect("small")).collect();
    let raw: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document")))
        .collect();
    let p = Pipeline::parse_command(&raw, &Ctx::default(), true)?;
    p.run(Vec::new(), &SearchEnv)
}

fn search() -> Document {
    Document::new().with("$search", Document::new().with("text", Document::new()))
}

fn vector() -> Document {
    Document::new().with("$vectorSearch", Document::new().with("path", "v"))
}

fn meta(field: &str, name: &str) -> Document {
    Document::new()
        .with("$project", Document::new().with(field, Document::new().with("$meta", name)))
}

fn group_by_id() -> Document {
    Document::new().with("$group", Document::new().with("_id", "$_id"))
}

fn x(id: i32, v: f64) -> Document {
    Document::new().with("_id", id).with("x", v)
}

fn id(id: i32) -> Document {
    Document::new().with("_id", id)
}

fn message(r: Result<Vec<Document>, MqlError>) -> (i32, String) {
    let e = r.expect_err("an error");
    (e.code.code, e.message)
}

const SEARCH_MISSING: &str = "query requires $search score metadata, but it is not available";
const VECTOR_MISSING: &str = "query requires $vectorSearch score metadata, but it is not available";

#[test]
fn the_scores_of_the_search_stages_reach_meta() {
    assert_eq!(run(vec![search(), meta("x", "searchScore")]).unwrap(), [x(2, 0.5), x(1, 0.25)]);
    assert_eq!(run(vec![search(), meta("x", "score")]).unwrap(), [x(2, 0.5), x(1, 0.25)]);
    assert_eq!(
        run(vec![vector(), meta("x", "vectorSearchScore")]).unwrap(),
        [x(1, 1.0), x(2, 0.5)]
    );
    assert_eq!(run(vec![vector(), meta("x", "score")]).unwrap(), [x(1, 1.0), x(2, 0.5)]);
    // The optional names give nothing.
    assert_eq!(run(vec![search(), meta("x", "searchHighlights")]).unwrap(), [id(2), id(1)]);
    let sort = Document::new()
        .with("$sort", Document::new().with("s", Document::new().with("$meta", "searchScore")));
    let by_id = Document::new().with("$sort", Document::new().with("_id", 1));
    assert_eq!(
        run(vec![search(), by_id, sort, meta("x", "searchScore")]).unwrap(),
        [x(2, 0.5), x(1, 0.25)]
    );
}

#[test]
fn a_stage_that_makes_new_documents_leaves_the_search_scores_out() {
    // `$group` still reads them; after it they read as nothing, but `score` is an error.
    let max = Document::new().with(
        "$group",
        Document::new()
            .with("_id", Value::Null)
            .with("s", Document::new().with("$max", Document::new().with("$meta", "searchScore"))),
    );
    assert_eq!(
        run(vec![search(), max]).unwrap(),
        [Document::new().with("_id", Value::Null).with("s", 0.5)]
    );
    let after = run(vec![search(), group_by_id(), meta("x", "searchScore")]).unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|d| d.get("x").is_none()), "{after:?}");
    let after = run(vec![vector(), group_by_id(), meta("x", "vectorSearchScore")]).unwrap();
    assert!(after.iter().all(|d| d.get("x").is_none()), "{after:?}");
    assert_eq!(
        message(run(vec![search(), group_by_id(), meta("x", "score")])),
        (40218, "query requires score metadata, but it is not available".to_owned())
    );
}

#[test]
fn metadata_no_search_stage_gave_is_refused_before_running() {
    let m = Document::new().with("$match", Document::new());
    assert_eq!(
        message(run(vec![m.clone(), meta("x", "searchScore")])),
        (40218, SEARCH_MISSING.to_owned())
    );
    assert_eq!(
        message(run(vec![m, meta("x", "vectorSearchScore")])),
        (40218, VECTOR_MISSING.to_owned())
    );
    assert_eq!(
        message(run(vec![search(), meta("x", "vectorSearchScore")])),
        (40218, VECTOR_MISSING.to_owned())
    );
    assert_eq!(
        message(run(vec![vector(), meta("x", "searchScore")])),
        (40218, SEARCH_MISSING.to_owned())
    );
    let search_meta = Document::new().with("$searchMeta", Document::new());
    assert_eq!(
        message(run(vec![search_meta, meta("x", "searchScore")])),
        (40218, SEARCH_MISSING.to_owned())
    );
    assert_eq!(
        message(run(vec![search(), meta("x", "searchRootDocumentId")])),
        (
            40218,
            "query requires $search root document id metadata, but it is not available".to_owned()
        )
    );
    let add = Document::new().with(
        "$addFields",
        Document::new().with("x", Document::new().with("$meta", "searchScore")),
    );
    assert_eq!(message(run(vec![add])), (40218, SEARCH_MISSING.to_owned()));
}

#[test]
fn search_meta_gives_its_document() {
    let search_meta = Document::new().with("$searchMeta", Document::new());
    assert_eq!(
        run(vec![search_meta]).unwrap(),
        [Document::new().with("count", Document::new().with("lowerBound", 2i64))]
    );
}

#[test]
fn documents_that_did_not_come_from_the_search_stage_refuse_its_score() {
    // Measured on 9.0.4: a document `$unionWith` or `$lookup` brings in has no `searchScore`,
    // and reading it is the 40218 while the pipeline runs; `$facet` sees the search's documents.
    let union = Document::new().with("$unionWith", Document::new().with("coll", "c"));
    assert_eq!(
        message(run(vec![search(), union, meta("x", "searchScore")])),
        (40218, SEARCH_MISSING.to_owned())
    );
    let inner = Value::Array(vec![meta("x", "searchScore").into()]);
    let lookup = Document::new().with(
        "$lookup",
        Document::new().with("from", "c").with("pipeline", inner.clone()).with("as", "j"),
    );
    assert_eq!(message(run(vec![search(), lookup])), (40218, SEARCH_MISSING.to_owned()));
    let facet = Document::new().with("$facet", Document::new().with("a", inner));
    let want = Document::new().with("a", Value::Array(vec![x(2, 0.5).into(), x(1, 0.25).into()]));
    assert_eq!(run(vec![search(), facet]).unwrap(), [want]);
}

#[test]
fn the_reference_checks_the_limit_and_filter_of_vector_search_first() {
    let vs = |spec: Document| run(vec![Document::new().with("$vectorSearch", spec)]);
    let positive = (7_912_700, "Expected limit to be positive".to_owned());
    for limit in [Value::Int32(0), Value::Int64(-1), Value::Double(-0.5), Value::Double(f64::NAN)] {
        assert_eq!(message(vs(Document::new().with("limit", limit))), positive);
    }
    assert_eq!(
        message(vs(Document::new().with("limit", "5"))),
        (8_575_100, "Expected limit field to be a number in $vectorSearch".to_owned())
    );
    assert_eq!(
        message(vs(Document::new().with("limit", 2).with("filter", 5))),
        (10_065, "invalid parameter: expected an object (filter)".to_owned())
    );
    let empty_and = Document::new().with("$and", Value::Array(Vec::new()));
    assert_eq!(
        message(vs(Document::new().with("filter", empty_and))),
        (2, "$and argument must be a non-empty array".to_owned())
    );
    let unknown = Document::new().with("n", Document::new().with("$foo", 1));
    assert_eq!(
        message(vs(Document::new().with("filter", unknown))),
        (2, "unknown operator: $foo".to_owned())
    );
    // What the reference search server checks reaches the node: a fractional limit, a missing one.
    let fine = Document::new().with("limit", 2.5).with("filter", Document::new().with("n", 1));
    assert_eq!(vs(fine).expect("runs").len(), 2);
    assert_eq!(vs(Document::new()).expect("runs").len(), 2);
    let decimal = teseladb_bson::Decimal128::from_parts(false, 3, 0).expect("a decimal");
    assert_eq!(vs(Document::new().with("limit", decimal)).expect("runs").len(), 2);
    assert_eq!(
        message(run(vec![Document::new().with("$vectorSearch", 5)])),
        (9, "$vectorSearch value must be an object. Found: int".to_owned())
    );
}
