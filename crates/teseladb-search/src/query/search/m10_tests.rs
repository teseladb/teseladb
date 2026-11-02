//! `autocomplete` with `nGram` and `rightEdgeGram`, points on the boundary of a shape, and
//! `hasAncestor` and `hasRoot` in nested `embeddedDocuments`, as `refsearch` 1.70.6 runs them
//! (measured 02-11-2026 on a 9.0.4 replica set, `scratchpad/lane3k/m10.js` and `m11.js`, whose
//! documents and index are these).

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn point(x: f64, y: f64) -> Value {
    let c = arr(vec![x.into(), y.into()]);
    Document::new().with("type", "Point").with("coordinates", c).into()
}

fn line(points: &[(f64, f64)]) -> Value {
    let c = arr(points.iter().map(|&(x, y)| arr(vec![x.into(), y.into()])).collect());
    Document::new().with("type", "LineString").with("coordinates", c).into()
}

fn polygon(points: &[(f64, f64)]) -> Value {
    let ring = arr(points.iter().map(|&(x, y)| arr(vec![x.into(), y.into()])).collect());
    Document::new().with("type", "Polygon").with("coordinates", arr(vec![ring])).into()
}

fn op(name: &str, args: Document) -> Document {
    Document::new().with(name, args)
}

fn text(path: &str, query: &str) -> Document {
    op("text", Document::new().with("path", path).with("query", query))
}

/// The index of `m10.js`.
fn m10() -> MemIndex {
    let ac = |t: &str| -> Value {
        Document::new()
            .with("type", "autocomplete")
            .with("tokenization", t)
            .with("minGrams", 2)
            .with("maxGrams", 4)
            .into()
    };
    let b = Document::new()
        .with("type", "embeddedDocuments")
        .with("dynamic", false)
        .with("fields", Document::new().with("x", Document::new().with("type", "string")));
    let a = Document::new().with("type", "embeddedDocuments").with("dynamic", false).with(
        "fields",
        Document::new().with("y", Document::new().with("type", "string")).with("b", b),
    );
    let fields = Document::new()
        .with("n", ac("nGram"))
        .with("r", ac("rightEdgeGram"))
        .with("e", ac("edgeGram"))
        .with("g", Document::new().with("type", "geo").with("indexShapes", true))
        .with("a", a);
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", false).with("fields", fields));
    let x = |s: &str| Value::from(Document::new().with("x", s));
    let el = |y: &str, b: Vec<Value>| Value::from(Document::new().with("y", y).with("b", arr(b)));
    let words = |d: Document, s: &str| d.with("n", s).with("r", s).with("e", s);
    let docs = [
        words(Document::new(), "gato montes")
            .with("a", arr(vec![el("rojo", vec![x("uno"), x("dos")]), el("azul", vec![x("uno")])]))
            .with("g", point(45.0, 45.0)),
        words(Document::new(), "perro gatuno")
            .with("a", arr(vec![el("rojo", vec![x("tres")])]))
            .with("g", point(-3.5, 40.5)),
        words(Document::new(), "abcdefgh")
            .with("a", arr(vec![el("verde rojo", vec![x("uno uno")])]))
            .with("g", line(&[(45.0, 0.0), (45.0, 60.0)])),
        words(Document::new(), "el gatopardo salvaje")
            .with("g", line(&[(-3.5, 39.0), (-3.5, 42.0)])),
        Document::new()
            .with("g", polygon(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)])),
        Document::new().with("g", point(0.0, 5.0)),
    ];
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, d) in docs.iter().enumerate() {
        idx.put(&[i as u8 + 1], d).expect("indexed");
    }
    idx
}

/// The documents `spec` finds, by key, with their scores.
fn found(idx: &MemIndex, spec: &Document) -> Result<Vec<(u8, f64)>, String> {
    run(idx, spec)
        .map(|hits| {
            let mut v: Vec<(u8, f64)> =
                hits.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect();
            v.sort_by_key(|h| h.0);
            v
        })
        .map_err(|e| e.0)
}

fn sorted(v: &[(u8, f64)]) -> Vec<(u8, f64)> {
    let mut v = v.to_vec();
    v.sort_by_key(|h| h.0);
    v
}

/// `m10.out`: field, query, option (`seq` for `tokenOrder: sequential`, `fuzzy` for one edit)
/// and the scores `refsearch` gives.
type Case = (&'static str, &'static str, &'static str, &'static [(u8, f64)]);
const AUTOCOMPLETE: &[Case] = &[
    (
        "n",
        "gat",
        "",
        &[(4, 0.3289514482021332), (1, 0.30685117840766907), (2, 0.30685117840766907)],
    ),
    ("n", "ato", "", &[(4, 0.639270544052124), (1, 0.5963217616081238)]),
    ("n", "tes", "", &[(1, 1.0357903242111206)]),
    ("n", "ntes", "", &[(1, 1.0357903242111206)]),
    ("n", "es", "", &[(1, 1.0357903242111206)]),
    ("n", "montes", "", &[(1, 1.0863754749298096)]),
    ("n", "gato montes", "", &[(1, 1.7118198871612549), (4, 0.6562740802764893)]),
    ("n", "efgh", "", &[(3, 0.9174026846885681)]),
    ("n", "cdef", "", &[(3, 0.9174026846885681)]),
    ("n", "abcdefgh", "", &[(3, 1.0413317680358887)]),
    ("n", "to mo", "", &[(1, 2.540947675704956), (4, 0.639270544052124)]),
    ("n", "o m", "", &[(1, 0.9088355302810669)]),
    ("n", "aje", "", &[(4, 1.082348108291626)]),
    ("n", "s", "", &[]),
    ("r", "gat", "", &[]),
    ("r", "ato", "", &[(1, 0.8303260803222656)]),
    ("r", "tes", "", &[(1, 0.9828349351882935)]),
    ("r", "ntes", "", &[(1, 1.0822227001190186)]),
    ("r", "es", "", &[(1, 0.9828349351882935)]),
    ("r", "montes", "", &[(1, 1.0822227001190186)]),
    ("r", "gato montes", "", &[(1, 1.9125487804412842)]),
    ("r", "efgh", "", &[(3, 1.0138719081878662)]),
    ("r", "cdef", "", &[]),
    ("r", "abcdefgh", "", &[(3, 1.0138719081878662)]),
    ("r", "to mo", "", &[(1, 0.8303260803222656)]),
    ("r", "o m", "", &[]),
    ("r", "aje", "", &[(4, 1.0246577262878418)]),
    ("r", "s", "", &[]),
    (
        "e",
        "gat",
        "",
        &[(1, 0.29116320610046387), (4, 0.2825148105621338), (2, 0.24598270654678345)],
    ),
    ("e", "ato", "", &[]),
    ("e", "montes", "", &[(1, 0.9828349351882935)]),
    ("e", "gato montes", "", &[(1, 1.585571527481079), (4, 0.6127268075942993)]),
    ("e", "efgh", "", &[]),
    ("e", "abcdefgh", "", &[(3, 1.0138719081878662)]),
    ("e", "to mo", "", &[(1, 0.8303260803222656)]),
    ("e", "aje", "", &[]),
    ("n", "ato mon", "seq", &[(1, 0.9088355302810669)]),
    ("n", "atp", "fuzzy", &[(4, 1.0), (1, 1.0), (2, 1.0)]),
    ("n", "nte", "fuzzy", &[(1, 1.0)]),
    ("r", "ato mon", "seq", &[]),
    ("r", "atp", "fuzzy", &[(1, 1.0)]),
    ("r", "nte", "fuzzy", &[(1, 1.0)]),
];

#[test]
fn autocomplete_tokenizations_match_refsearch() {
    let idx = m10();
    for &(path, query, option, want) in AUTOCOMPLETE {
        let mut d = Document::new().with("path", path).with("query", query);
        match option {
            "seq" => d.push("tokenOrder", "sequential"),
            "fuzzy" => d.push("fuzzy", Document::new().with("maxEdits", 1)),
            _ => {}
        }
        let got = found(&idx, &op("autocomplete", d));
        assert_eq!(got, Ok(sorted(want)), "{path} {query:?} {option}");
    }
}

/// `m13.js`: runs of up to three words, punctuation dropped from the indexed strings and kept
/// in the query's whole text.
#[test]
fn autocomplete_indexes_runs_of_up_to_three_words() {
    let field = Document::new()
        .with("type", "autocomplete")
        .with("tokenization", "edgeGram")
        .with("minGrams", 2)
        .with("maxGrams", 15);
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", false).with("fields", Document::new().with("e", field)),
    );
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, t) in ["a1 b2 c3 d4 e5 f6 g7 h8", "Hola, Mundo! bien", "xx yy zz"].iter().enumerate() {
        idx.put(&[i as u8 + 1], &Document::new().with("e", *t)).expect("indexed");
    }
    let seq = |q: &str| {
        let d = Document::new().with("path", "e").with("query", q).with("tokenOrder", "sequential");
        found(&idx, &op("autocomplete", d))
    };
    let cases: [(&str, &[(u8, f64)]); 15] = [
        ("a1", &[(1, 0.8512857556343079)]),
        ("a1 b2", &[(1, 0.7985512018203735)]),
        ("c3 d4", &[(1, 0.7985512018203735)]),
        ("g7 h8", &[(1, 0.6734051704406738)]),
        ("a1 b2 c3 d4 e5", &[]),
        ("hola", &[(2, 0.8760805130004883)]),
        ("hola,", &[]),
        ("hola m", &[(2, 0.831670880317688)]),
        ("hola, m", &[]),
        ("mundo b", &[(2, 0.7218903303146362)]),
        ("mundo!", &[]),
        ("xx yy", &[(3, 0.831670880317688)]),
        ("yy zz", &[(3, 0.7218903303146362)]),
        ("xx yy zz", &[(3, 0.7218903303146362)]),
        ("bien", &[(2, 0.7218903303146362)]),
    ];
    for (q, want) in cases {
        assert_eq!(seq(q), Ok(want.to_vec()), "{q:?}");
    }
    let any = |q: &str| {
        found(&idx, &op("autocomplete", Document::new().with("path", "e").with("query", q)))
    };
    assert_eq!(any("xx yy zz"), Ok(vec![(3, 3.1515321731567383)]));
    assert_eq!(any("hola, mundo"), Ok(vec![(2, 1.7077513933181763)]));
    assert_eq!(any("b2 c3 d4"), Ok(vec![(1, 3.227262496948242)]));
}

fn shape(relation: &str, geometry: Value) -> Document {
    op(
        "geoShape",
        Document::new().with("path", "g").with("relation", relation).with("geometry", geometry),
    )
}

/// What a search finds, or its error.
type Found = Result<Vec<(u8, f64)>, String>;

fn ones(ids: &[u8]) -> Found {
    Ok(ids.iter().map(|&i| (i, 1.0)).collect())
}

/// A stored point sits on 32-bit coordinates and the query's geometry does not: a point is on a
/// line only when both land on the same value (45° does, -3.5° and 5° do not).
#[test]
fn points_on_a_boundary_count_after_quantization() {
    let idx = m10();
    let f = |s: &Document| found(&idx, s);
    let west = polygon(&[(0.0, 0.0), (0.0, 10.0), (-10.0, 10.0), (-10.0, 0.0), (0.0, 0.0)]);
    let cases: Vec<(Document, Found)> = vec![
        (shape("intersects", line(&[(45.0, 0.0), (45.0, 60.0)])), ones(&[1, 3])),
        (shape("intersects", line(&[(-3.5, 39.0), (-3.5, 42.0)])), ones(&[])),
        (shape("intersects", point(45.0, 45.0)), ones(&[1, 3])),
        (shape("intersects", point(-3.5, 40.5)), ones(&[])),
        (shape("intersects", point(0.0, 5.0)), ones(&[5])),
        (shape("intersects", point(10.0, 5.0)), ones(&[])),
        (shape("intersects", west.clone()), ones(&[5, 6])),
        (shape("within", west.clone()), ones(&[6])),
        (shape("disjoint", west.clone()), ones(&[1, 2, 3, 4])),
        (shape("contains", point(0.0, 5.0)), ones(&[5])),
        (shape("contains", point(45.0, 30.0)), ones(&[3])),
        (op("geoWithin", Document::new().with("path", "g").with("geometry", west)), ones(&[6])),
    ];
    for (spec, want) in cases {
        assert_eq!(f(&spec), want, "{spec:?}");
    }
    let boxed = |low: (f64, f64), high: (f64, f64)| {
        let b = Document::new()
            .with("bottomLeft", point(low.0, low.1))
            .with("topRight", point(high.0, high.1));
        op("geoWithin", Document::new().with("path", "g").with("box", b))
    };
    assert_eq!(f(&boxed((0.0, 0.0), (45.0, 45.0))), ones(&[1, 6]));
    assert_eq!(f(&boxed((-3.5, 40.5), (0.0, 45.0))), ones(&[]));
}

fn embedded(path: &str, operator: Document) -> Document {
    op("embeddedDocument", Document::new().with("path", path).with("operator", operator))
}

fn ancestor(path: &str, operator: Document) -> Document {
    op("hasAncestor", Document::new().with("ancestorPath", path).with("operator", operator))
}

fn root(operator: Document) -> Document {
    op("hasRoot", Document::new().with("operator", operator))
}

fn exists(path: &str) -> Document {
    op("exists", Document::new().with("path", path))
}

/// `a.b` inside `a`, or alone: its elements' statistics are those of every `a.b` element, and
/// `hasAncestor` scores the ancestor element's score plus 1 for each element.
#[test]
fn has_ancestor_scores_nested_elements() {
    let idx = m10();
    let f = |s: &Document| found(&idx, s);
    let a = |o: Document| embedded("a", o);
    let ab = |o: Document| embedded("a.b", o);
    let rojo = || ancestor("a", text("a.y", "rojo"));
    let uno = || text("a.b.x", "uno");
    let both = vec![(1, 0.525850236415863), (3, 0.28368237614631653)];
    assert_eq!(f(&a(ab(uno()))), Ok(both.clone()));
    assert_eq!(f(&ab(uno())), Ok(both));
    let by_rojo = vec![(1, 2.3531434535980225), (2, 1.1765717267990112), (3, 1.1301733255386353)];
    assert_eq!(f(&a(ab(rojo()))), Ok(by_rojo.clone()));
    assert_eq!(f(&ab(rojo())), Ok(by_rojo));
    let must = op("compound", Document::new().with("must", arr(vec![uno().into(), rojo().into()])));
    assert_eq!(f(&a(ab(must))), Ok(vec![(1, 1.4394968748092651), (3, 1.4138556718826294)]));
    let boosted = op(
        "hasAncestor",
        Document::new()
            .with("ancestorPath", "a")
            .with("operator", text("a.y", "rojo"))
            .with("score", Document::new().with("boost", Document::new().with("value", 3))),
    );
    assert_eq!(
        f(&a(ab(boosted))),
        Ok(vec![(1, 7.0594305992126465), (2, 3.5297152996063232), (3, 3.3905200958251953)])
    );
    // `hasRoot` reaches the document, through `hasAncestor` too: 1 + 1 per level.
    assert_eq!(f(&a(ab(root(exists("n"))))), Ok(vec![(1, 6.0), (2, 2.0), (3, 2.0)]));
    assert_eq!(f(&ab(root(exists("n")))), Ok(vec![(1, 6.0), (2, 2.0), (3, 2.0)]));
    assert_eq!(f(&a(ab(ancestor("a", root(exists("n")))))), Ok(vec![(1, 9.0), (2, 3.0), (3, 3.0)]));
    assert_eq!(f(&a(ab(ancestor("a", exists("a.y"))))), Ok(vec![(1, 6.0), (2, 2.0), (3, 2.0)]));
    // A field of a deeper level is not in the ancestor's documents.
    assert_eq!(f(&a(ab(ancestor("a", uno())))), Ok(vec![]));
    // An `embeddedDocument` inside another scores the outer element it belongs to.
    let should =
        arr(vec![text("a.y", "rojo").into(), ab(ancestor("a", text("a.y", "verde"))).into()]);
    assert_eq!(
        f(&a(op("compound", Document::new().with("should", should)))),
        Ok(vec![(1, 0.17657174170017242), (2, 0.17657174170017242), (3, 1.5695794820785522)])
    );
}

#[test]
fn has_ancestor_errors_match_refsearch() {
    let idx = m10();
    let err = |s: Document| found(&idx, &s).unwrap_err();
    let a = |o: Document| embedded("a", o);
    let ab = |o: Document| embedded("a.b", o);
    assert_eq!(
        err(a(ab(ancestor("x", text("a.y", "rojo"))))),
        "hasAncestor.ancestorPath 'x' must be a parent of embeddedDocument.path 'a.b'"
    );
    assert_eq!(
        err(a(ab(ancestor("a.b", text("a.y", "rojo"))))),
        "hasAncestor.ancestorPath 'a.b' must be a parent of embeddedDocument.path 'a.b'"
    );
    assert_eq!(
        err(a(ancestor("a", text("a.y", "rojo")))),
        "hasAncestor.ancestorPath 'a' must be a parent of embeddedDocument.path 'a'"
    );
    assert_eq!(
        err(a(ab(ancestor("a", ancestor("a", exists("a.y")))))),
        "hasAncestor.ancestorPath 'a' must be a parent of hasAncestor.ancestorPath 'a'"
    );
    assert_eq!(
        err(a(ab(ancestor("a", text("n", "rojo"))))),
        "text operator path 'n' must be a descendant of hasAncestor.ancestorPath 'a'"
    );
    assert_eq!(
        err(a(ab(text("a.y", "rojo")))),
        "\"embeddedDocument.operator.embeddedDocument\" a.y is not a subfield of a.b"
    );
    let at = "embeddedDocument.operator.embeddedDocument.operator.hasAncestor";
    assert_eq!(
        err(a(ab(op("hasAncestor", Document::new().with("ancestorPath", "a"))))),
        format!("\"{at}.operator\" is required")
    );
    assert_eq!(
        err(a(ab(op("hasAncestor", Document::new().with("operator", exists("a.y")))))),
        format!("\"{at}.ancestorPath\" is required")
    );
    let extra =
        Document::new().with("ancestorPath", "a").with("operator", exists("a.y")).with("x", 1);
    assert_eq!(err(a(ab(op("hasAncestor", extra)))), format!("\"{at}\" unrecognized field \"x\""));
}
