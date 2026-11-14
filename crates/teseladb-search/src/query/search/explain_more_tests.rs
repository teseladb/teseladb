//! `scoreDetails` of the forms the fixtures of `tests/score_details_refsearch.rs` leave out: score
//! options on fuzzy, phrase and synonym queries, constant scores of whole `compound`s, values of
//! other types and paths, and `near` on several `geo` paths. The descriptions follow the rules
//! measured there (DV-083).

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;
use crate::index::EQUIVALENT;

fn index() -> MemIndex {
    let mapping = Document::new()
        .with("name", "sy")
        .with("analyzer", teseladb_names::ANALYZER_STANDARD)
        .with("source", Document::new().with("collection", "syn"));
    let fields = Document::new()
        .with("k", Document::new().with("type", "token").with("normalizer", "lowercase"))
        .with("g1", Document::new().with("type", "geo"))
        .with("g2", Document::new().with("type", "geo"));
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", true).with("fields", fields))
        .with("synonyms", Value::Array(vec![mapping.into()]));
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    let point = |x: f64, y: f64| {
        Document::new()
            .with("type", "Point")
            .with("coordinates", Value::Array(vec![x.into(), y.into()]))
    };
    let docs = [
        Document::new()
            .with("t", "gato negro gata")
            .with("k", "Abc")
            .with("o", Value::ObjectId([0xff; 12]))
            .with("b", false)
            .with("d", Value::DateTime(1_000))
            .with("g1", point(1.0, 1.0))
            .with("g2", point(2.0, 2.0)),
        Document::new().with("t", "perro gato").with("k", "x").with("d", Value::DateTime(5_000)),
    ];
    for (i, d) in docs.iter().enumerate() {
        idx.put(&[i as u8], d).expect("indexed");
    }
    let syn = Document::new()
        .with("mappingType", EQUIVALENT)
        .with("synonyms", Value::Array(vec!["gato".into(), "minino".into()]));
    idx.set_synonyms("sy", &[syn]).expect("loaded");
    idx
}

/// The description of the explanation of document 0 for `spec`.
fn top(idx: &MemIndex, spec: Document) -> (f32, String) {
    let got = explain(idx, &spec, &[vec![0]]).expect("explained");
    let Some((_, e)) = got.into_iter().next() else { panic!("nothing for {spec:?}") };
    (e.value, e.description)
}

fn op(name: &str, d: Document) -> Document {
    Document::new().with(name, d)
}

fn text(q: &str) -> Document {
    Document::new().with("path", "t").with("query", q)
}

fn constant(v: f64) -> Document {
    Document::new().with("constant", Document::new().with("value", v))
}

#[test]
fn score_options_name_the_whole_query() {
    let idx = index();
    let fuzzy = text("gato")
        .with("fuzzy", Document::new().with("maxEdits", 1))
        .with("score", constant(2.0));
    assert_eq!(
        top(&idx, op("text", fuzzy)).1,
        "ConstantScore(($type:string/t:gata)^0.75 $type:string/t:gato)^2.0"
    );
    let phrase = text("gato negro").with("slop", 2).with("score", constant(3.0));
    assert_eq!(
        top(&idx, op("phrase", phrase)).1,
        "ConstantScore($type:string/t:\"gato negro\"~2)^3.0"
    );
    let syn = text("gato").with("synonyms", "sy").with("score", constant(2.0));
    assert_eq!(
        top(&idx, op("text", syn)).1,
        "ConstantScore($type:string/t:gato $type:string/t:minino)^2.0"
    );
    let function = Document::new().with("function", Document::new().with("constant", 4.0));
    let syn = text("gato negro").with("synonyms", "sy").with("score", function);
    assert_eq!(
        top(&idx, op("text", syn)).1,
        "weight(FunctionScoreQuery($type:string/t:\"(gato minino) negro\", scored by constant(4.0))), result of:"
    );
    let boost_path = Document::new().with("boost", Document::new().with("path", "b"));
    assert_eq!(
        top(&idx, op("phrase", text("gato negro").with("score", boost_path))).1,
        "weight(FunctionScoreQuery($type:string/t:\"gato negro\", scored by boost(b))), result of:"
    );
}

#[test]
fn whole_compounds_and_single_filters_describe_their_clauses() {
    let idx = index();
    let c = Document::new()
        .with(
            "should",
            Value::Array(vec![op("text", text("gato")).into(), op("text", text("perro")).into()]),
        )
        .with("score", constant(5.0));
    assert_eq!(
        top(&idx, op("compound", c)).1,
        "ConstantScore($type:string/t:gato $type:string/t:perro)^5.0"
    );
    let inner = Document::new().with("must", Value::Array(vec![op("text", text("gato")).into()]));
    let c = Document::new().with("filter", Value::Array(vec![op("compound", inner).into()]));
    assert_eq!(
        top(&idx, op("compound", c)),
        (
            0.0,
            "ConstantScore(ScoreDetailsWrapped (+ScoreDetailsWrapped ($type:string/t:gato)))^0.0"
                .to_owned()
        )
    );
    let range = Document::new().with("path", "d").with("gte", Value::DateTime(0));
    let c = Document::new().with("filter", Value::Array(vec![op("range", range).into()]));
    assert!(
        top(&idx, op("compound", c))
            .1
            .starts_with("ConstantScore(ScoreDetailsWrapped (IndexOrDocValuesQuery")
    );
}

#[test]
fn values_of_every_type_are_described() {
    let idx = index();
    let equals = |v: Value| op("equals", Document::new().with("path", "k").with("value", v));
    assert_eq!(top(&idx, equals("ABC".into())).1, "$type:token/k:[[61 62 63] TO [61 62 63]]");
    let o =
        op("equals", Document::new().with("path", "o").with("value", Value::ObjectId([0xff; 12])));
    assert_eq!(
        top(&idx, o).1,
        "ConstantScore($type:objectId/o:[ff ff ff ff ff ff ff ff ff ff ff ff]) [BM25Similarity], result of:"
    );
    let b = op("equals", Document::new().with("path", "b").with("value", false));
    assert_eq!(top(&idx, b).1, "ConstantScore($type:boolean/b:F) [BM25Similarity], result of:");
    let bools = Value::Array(vec![false.into(), false.into(), true.into()]);
    let b = op("in", Document::new().with("path", "b").with("value", bools));
    assert_eq!(top(&idx, b).1, "ConstantScore($type:boolean/b:F $type:boolean/b:T)");
    let d = op(
        "range",
        Document::new()
            .with("path", "d")
            .with("gt", Value::DateTime(0))
            .with("lt", Value::DateTime(2_000)),
    );
    assert!(top(&idx, d).1.contains("$type:date/d:[1 TO 1999]"));
    let near = Document::new()
        .with("path", "d")
        .with("origin", Value::DateTime(0))
        .with("pivot", 1000)
        .with("score", constant(2.0));
    assert_eq!(top(&idx, op("near", near)).1, "ConstantScore(query)^2.0");
}

#[test]
fn near_on_several_geo_paths_adds_them() {
    let idx = index();
    let origin = Document::new()
        .with("type", "Point")
        .with("coordinates", Value::Array(vec![0.0.into(), 0.0.into()]));
    let near = Document::new()
        .with("path", Value::Array(vec!["g1".into(), "g2".into(), "t".into()]))
        .with("origin", origin)
        .with("pivot", 1000);
    let got = explain(&idx, &op("near", near), &[vec![0]]).expect("explained");
    let e = &got[0].1;
    assert_eq!(e.description, "sum of:");
    assert_eq!(e.details.len(), 2);
    assert!(e.details.iter().all(|d| d.description.starts_with("Distance score")));
}
