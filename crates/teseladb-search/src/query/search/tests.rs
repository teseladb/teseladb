//! `$search` operators as `refsearch` 1.70.6 runs them on a 9.0.4 replica set (measured 01-11-2026
//! on the corpus of `index::tests`, index `{mappings: {dynamic: true}}`).

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;

const TEXTS: [&str; 8] = [
    "gato",
    "gato perro",
    "gato gato perro raton",
    "perro perro perro",
    "raton queso queso queso queso queso queso",
    "el gato y el perro y el raton y el queso y la casa y el jardin y la calle",
    "gato gato gato gato gato gato gato gato gato gato",
    "casa",
];

fn index() -> MemIndex {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, t) in TEXTS.iter().enumerate() {
        let u = if i % 2 == 1 { "gato" } else { "perro casa" };
        idx.put(&[i as u8], &Document::new().with("_id", i as i32).with("t", *t).with("u", u))
            .expect("indexed");
    }
    idx
}

fn text(q: &str) -> Value {
    Document::new().with("text", Document::new().with("query", q).with("path", "t")).into()
}

fn search(spec: Document) -> Result<Vec<(u8, f64)>, SearchError> {
    Ok(run(&index(), &spec)?.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect())
}

fn compound(c: Document) -> Document {
    Document::new().with("compound", c)
}

fn list(v: Vec<Value>) -> Value {
    Value::Array(v)
}

fn sorted(mut v: Vec<(u8, f64)>) -> Vec<(u8, f64)> {
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

#[test]
fn compound_adds_and_filters_as_refsearch() {
    let must2 = compound(Document::new().with("must", list(vec![text("gato"), text("perro")])));
    assert_eq!(
        search(must2).unwrap(),
        [(1, 0.7410147190093994), (2, 0.7044532299041748), (5, 0.27572640776634216)]
    );
    let should2 = compound(Document::new().with("should", list(vec![text("gato"), text("perro")])));
    assert_eq!(search(should2).unwrap().len(), 6);
    let must_should = compound(
        Document::new()
            .with("must", list(vec![text("gato")]))
            .with("should", list(vec![text("perro")])),
    );
    assert_eq!(
        search(must_should).unwrap(),
        sorted(vec![
            (1, 0.7410147190093994),
            (2, 0.7044532299041748),
            (6, 0.41735297441482544),
            (0, 0.33963894844055176),
            (5, 0.27572640776634216)
        ])
    );
    let filter = compound(
        Document::new()
            .with("filter", list(vec![text("gato")]))
            .with("should", list(vec![text("perro")])),
    );
    assert_eq!(
        search(filter).unwrap(),
        [
            (1, 0.43321695923805237),
            (2, 0.36481431126594543),
            (5, 0.1611970067024231),
            (0, 0.0),
            (6, 0.0)
        ]
    );
    let must_not = compound(
        Document::new()
            .with("should", list(vec![text("gato")]))
            .with("mustNot", list(vec![text("perro")])),
    );
    assert_eq!(search(must_not).unwrap(), [(6, 0.41735297441482544), (0, 0.33963894844055176)]);
    let only_not = compound(Document::new().with("mustNot", list(vec![text("gato")])));
    assert_eq!(search(only_not).unwrap(), [(3, 0.0), (4, 0.0), (7, 0.0)]);
    let msm = compound(
        Document::new()
            .with("should", list(vec![text("gato"), text("perro"), text("raton")]))
            .with("minimumShouldMatch", 2),
    );
    assert_eq!(
        search(msm).unwrap(),
        [(2, 1.201538324356079), (1, 0.7410147190093994), (5, 0.495368629693985)]
    );
    let inner = compound(Document::new().with("should", list(vec![text("gato"), text("queso")])));
    let nested = compound(Document::new().with("must", list(vec![inner.into()])));
    let array_query = Document::new().with(
        "text",
        Document::new().with("query", list(vec!["gato".into(), "queso".into()])).with("path", "t"),
    );
    assert_eq!(search(nested).unwrap(), search(array_query).unwrap());
    let one = compound(Document::new().with("must", text("gato")));
    assert_eq!(search(one).unwrap().len(), 5);
}

#[test]
fn scores_can_be_constant_or_boosted_by_a_field() {
    let t = |score: Document| {
        Document::new().with(
            "text",
            Document::new().with("query", "gato").with("path", "t").with("score", score),
        )
    };
    let constant =
        search(t(Document::new().with("constant", Document::new().with("value", 2)))).unwrap();
    assert!(constant.iter().all(|(_, s)| *s == 2.0) && constant.len() == 5, "{constant:?}");
    let by_path =
        search(t(Document::new().with("boost", Document::new().with("path", "n")))).unwrap();
    assert!(by_path.iter().all(|(_, s)| *s == 0.0) && by_path.len() == 5, "{by_path:?}");
    let missing = Document::new()
        .with("index", "default")
        .with("text", Document::new().with("query", "gato").with("path", "zz"));
    assert!(search(missing).unwrap().is_empty());
}

#[test]
fn errors_are_refsearch_s() {
    let ops = OPERATORS.join(", ");
    let cases: Vec<(Document, String)> = vec![
        (
            Document::new().with("index", "default"),
            format!("Query should contain either an operator [{ops}] or a collector [facet]"),
        ),
        (
            Document::new().with("foo", Document::new().with("query", "gato")),
            format!("Query should contain either an operator [{ops}] or a collector [facet]"),
        ),
        (
            Document::new()
                .with("text", Document::new().with("query", "g").with("path", "t"))
                .with("phrase", Document::new().with("query", "g").with("path", "t")),
            format!("at most one of [{ops}] may be present"),
        ),
        (
            Document::new().with("text", Document::new().with("path", "t")),
            "\"text.query\" is required".to_owned(),
        ),
        (
            Document::new().with("text", Document::new().with("query", "g")),
            "\"text.path\" is required".to_owned(),
        ),
        (
            Document::new().with("text", Document::new().with("query", 5).with("path", "t")),
            "\"text.query\" must be a string".to_owned(),
        ),
        (
            Document::new()
                .with("text", Document::new().with("query", "g").with("path", "t").with("foo", 1)),
            "\"text\" unrecognized field \"foo\"".to_owned(),
        ),
        (
            compound(Document::new()),
            "\"compound\" at least one of [filter, must, mustNot, should] must be present"
                .to_owned(),
        ),
        (
            compound(Document::new().with("must", 5)),
            "\"compound.must\" must be a document".to_owned(),
        ),
        (
            compound(
                Document::new()
                    .with("should", list(vec![text("gato")]))
                    .with("minimumShouldMatch", 2),
            ),
            "\"compound\" minimumShouldMatch cannot be greater than number of should clauses"
                .to_owned(),
        ),
        (
            Document::new().with(
                "text",
                Document::new().with("query", "g").with("path", "t").with(
                    "score",
                    Document::new()
                        .with("boost", Document::new().with("value", 2))
                        .with("constant", Document::new().with("value", 1)),
                ),
            ),
            "\"text.score\" only one of [boost, constant, function] may be present".to_owned(),
        ),
    ];
    for (spec, want) in cases {
        assert_eq!(search(spec.clone()), Err(SearchError(want)), "{spec:?}");
    }
}

fn phrase(q: Value, slop: Option<i32>) -> Document {
    let mut p = Document::new().with("query", q).with("path", "t");
    if let Some(s) = slop {
        p.push("slop", s);
    }
    Document::new().with("phrase", p)
}

#[test]
fn phrases_score_as_refsearch() {
    assert_eq!(
        search(phrase("gato perro".into(), None)).unwrap(),
        [(1, 0.7410147190093994), (2, 0.6240124702453613)]
    );
    assert_eq!(
        search(phrase("gato gato".into(), None)).unwrap(),
        [(6, 0.8207941055297852), (2, 0.5183963179588318)]
    );
    assert!(search(phrase("gato raton".into(), None)).unwrap().is_empty());
    assert_eq!(search(phrase("gato raton".into(), Some(1))).unwrap(), [(2, 0.5131921768188477)]);
    assert_eq!(
        search(phrase("gato raton".into(), Some(5))).unwrap(),
        [(2, 0.5131921768188477), (5, 0.069083571434021)]
    );
    assert_eq!(
        search(phrase("queso".into(), None)).unwrap(),
        [(4, 1.0456602573394775), (5, 0.2978915572166443)]
    );
    assert_eq!(
        search(phrase(list(vec!["gato perro".into(), "queso queso".into()]), None)).unwrap(),
        [(4, 2.017218589782715), (1, 0.7410147190093994), (2, 0.6240124702453613)]
    );
    assert_eq!(
        search(Document::new().with("phrase", Document::new().with("path", "t"))),
        Err(SearchError("\"phrase.query\" is required".to_owned()))
    );
    assert_eq!(
        search(phrase("a b".into(), Some(-1))),
        Err(SearchError("\"phrase.slop\" cannot be negative".to_owned()))
    );
}
