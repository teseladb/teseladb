//! `wildcard` and `regex` as `refsearch` 1.70.6 runs them (measured 02-11-2026 on a 9.0.4 replica
//! set, `scratchpad/lane3i/w1.js`..`w3.js`): documents `{_id: 1, t: "gato perro"}`,
//! `{_id: 2, t: "Gatos gatito"}`, `{_id: 3, t: "perro raton", k: "GaTo"}`,
//! `{_id: 4, t: ["gato", "casa gata"]}` and `{_id: 5, t: "g?to g*to"}`, index
//! `{mappings: {dynamic: true, fields: {k: {type: "string", analyzer: "reflib.keyword"}}}}`.
//! Equal scores come in key order here (`refsearch` gives them in its own document order).

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;

fn index() -> MemIndex {
    let k = Document::new().with("type", "string").with("analyzer", "reflib.keyword");
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", true).with("fields", Document::new().with("k", k)),
    );
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    let t4 = Value::Array(vec!["gato".into(), "casa gata".into()]);
    let docs = [
        Document::new().with("_id", 1).with("t", "gato perro"),
        Document::new().with("_id", 2).with("t", "Gatos gatito"),
        Document::new().with("_id", 3).with("t", "perro raton").with("k", "GaTo"),
        Document::new().with("_id", 4).with("t", t4),
        Document::new().with("_id", 5).with("t", "g?to g*to"),
    ];
    for (i, d) in docs.iter().enumerate() {
        idx.put(&[i as u8 + 1], d).expect("indexed");
    }
    idx
}

fn op(name: &str, args: Document) -> Document {
    Document::new().with(name, args)
}

fn q(query: impl Into<Value>, path: impl Into<Value>, allow: bool) -> Document {
    let d = Document::new().with("query", query).with("path", path);
    if allow { d.with("allowAnalyzedField", true) } else { d }
}

fn found(spec: &Document) -> Result<Vec<(u8, f64)>, String> {
    run(&index(), spec)
        .map(|hits| hits.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect())
        .map_err(|e| e.0)
}

fn ones(ids: &[u8]) -> Vec<(u8, f64)> {
    ids.iter().map(|&i| (i, 1.0)).collect()
}

const ANALYZED: &str =
    "Field t is analyzed. Use a keyword analyzed field or set allowAnalyzedField to true.";

#[test]
fn wildcard_matches_whole_terms_with_a_constant_score() {
    let w = |query: &str| found(&op("wildcard", q(query, "t", true)));
    assert_eq!(w("gat*"), Ok(ones(&[1, 2, 4])));
    assert_eq!(w("g?to"), Ok(ones(&[1, 4])));
    assert_eq!(w("*"), Ok(ones(&[1, 2, 3, 4, 5])));
    assert_eq!(w("gat\\*"), Ok(vec![]));
    // The query is normalized as the field's analyzer does: lower case.
    assert_eq!(w("G*"), Ok(ones(&[1, 2, 4, 5])));
    assert_eq!(w("g*to"), Ok(ones(&[1, 2, 4])));
    assert_eq!(w("\\?"), Ok(vec![]));
    // Each query and path that matches adds its score.
    let two = op("wildcard", q(vec![Value::from("gat*"), "cas*".into()], "t", true));
    assert_eq!(found(&two), Ok(vec![(4, 2.0), (1, 1.0), (2, 1.0)]));
    let paths = Value::Array(vec!["t".into(), "k".into()]);
    let both = op("wildcard", q(vec![Value::from("g*"), "ga*".into()], paths.clone(), true));
    assert_eq!(found(&both), Ok(vec![(1, 2.0), (2, 2.0), (4, 2.0), (5, 1.0)]));
    assert_eq!(
        found(&op("wildcard", q("*", paths, true))),
        Ok(vec![(3, 2.0), (1, 1.0), (2, 1.0), (4, 1.0), (5, 1.0)])
    );
    let any = op("wildcard", q("gat*", Document::new().with("wildcard", "*"), true));
    assert_eq!(found(&any), Ok(ones(&[1, 2, 4])));
    // A keyword field keeps the case of the query and needs no `allowAnalyzedField`.
    assert_eq!(found(&op("wildcard", q("GaT*", "k", false))), Ok(ones(&[3])));
    assert_eq!(found(&op("wildcard", q("gat*", "k", true))), Ok(vec![]));
    assert_eq!(found(&op("wildcard", q("a\\", "k", false))), Ok(vec![]));
    assert_eq!(found(&op("wildcard", q("g?to g*to", "k", false))), Ok(vec![]));
    assert_eq!(found(&op("wildcard", q("gat*", "nope", true))), Ok(vec![]));
}

#[test]
fn pattern_scores_follow_the_score_option() {
    let scored =
        |score: Document| found(&op("wildcard", q("GaT*", "k", false).with("score", score)));
    let value = |v: f64| Document::new().with("value", v);
    assert_eq!(scored(Document::new().with("constant", value(2.5))), Ok(vec![(3, 2.5)]));
    assert_eq!(scored(Document::new().with("boost", value(0.3))), Ok(vec![(3, f64::from(0.3f32))]));
    let by_id = Document::new().with("boost", Document::new().with("path", "_id"));
    assert_eq!(scored(by_id), Ok(vec![(3, 3.0)]));
    let missing = Document::new().with("boost", Document::new().with("path", "x"));
    let none = op("wildcard", q("gat*", "t", true).with("score", missing));
    assert_eq!(found(&none), Ok(vec![(1, 0.0), (2, 0.0), (4, 0.0)]));
    let tripled = Document::new().with("boost", value(3.0));
    let paths = Value::Array(vec!["t".into(), "k".into()]);
    let r = op("regex", q("gat.*", paths, true).with("score", tripled));
    assert_eq!(found(&r), Ok(vec![(1, 3.0), (2, 3.0), (4, 3.0)]));
    // In a compound it adds to the other clauses.
    let should = Value::Array(vec![
        op("wildcard", q("gat*", "t", true)).into(),
        op("text", Document::new().with("query", "gato").with("path", "t")).into(),
    ]);
    let c = found(&op("compound", Document::new().with("should", should))).unwrap();
    let want = [(1u8, 1.4394245147705078f64), (4, 1.3743780851364136), (2, 1.0)];
    assert_eq!(c, want);
}

#[test]
fn regex_matches_whole_terms() {
    let r = |query: &str| found(&op("regex", q(query, "t", true)));
    assert_eq!(r("gat.*"), Ok(ones(&[1, 2, 4])));
    assert_eq!(r("g.to"), Ok(ones(&[1, 4])));
    assert_eq!(r(".*"), Ok(ones(&[1, 2, 3, 4, 5])));
    assert_eq!(r("gat[oa]"), Ok(ones(&[1, 4])));
    assert_eq!(r("G.*"), Ok(ones(&[1, 2, 4, 5])));
    assert_eq!(r("(gato|casa)"), Ok(ones(&[1, 4])));
    assert_eq!(r("@"), Ok(ones(&[1, 2, 3, 4, 5])));
    assert_eq!(r("\\w"), Ok(ones(&[5])));
    assert_eq!(r(".{0,3}o"), Ok(ones(&[1, 4, 5])));
    assert_eq!(r("g.t[^a]"), Ok(ones(&[1, 4])));
    assert_eq!(r("\"gato\""), Ok(ones(&[1, 4])));
    for none in ["\\d+", "<1-5>", "a&b", "~a", "[[:alpha:]]", "#", "a**"] {
        assert_eq!(r(none), Ok(vec![]), "{none}");
    }
    let two = op("regex", q(vec![Value::from("gat.*"), "cas.*".into()], "t", true));
    assert_eq!(found(&two), Ok(vec![(4, 2.0), (1, 1.0), (2, 1.0)]));
    assert_eq!(found(&op("regex", q("GaT.*", "k", false))), Ok(ones(&[3])));
}

#[test]
fn pattern_errors_are_refsearch_s() {
    for name in ["wildcard", "regex"] {
        let err = |args: Document| found(&op(name, args)).unwrap_err();
        assert_eq!(err(q("g.*", "t", false)), ANALYZED);
        assert_eq!(err(q("g.*", "nope", false)), ANALYZED.replacen("Field t", "Field nope", 1));
        assert_eq!(err(Document::new().with("path", "t")), format!("\"{name}.query\" is required"));
        assert_eq!(err(Document::new().with("query", "x")), format!("\"{name}.path\" is required"));
        assert_eq!(err(q(5, "t", true)), format!("\"{name}.query\" must be a string"));
        assert_eq!(
            err(q(vec![Value::from(1)], "t", true)),
            format!("\"{name}.query\" must be a string")
        );
        assert_eq!(
            err(q(Value::Array(vec![]), "t", true)),
            format!("\"{name}.query\" cannot be empty")
        );
        assert_eq!(
            err(q("x", "t", true).with("foo", 1)),
            format!("\"{name}\" unrecognized field \"foo\"")
        );
        assert_eq!(
            err(Document::new()
                .with("query", "x")
                .with("path", "t")
                .with("allowAnalyzedField", "yes")),
            format!("\"{name}.allowAnalyzedField\" must be a boolean")
        );
    }
    let r = |query: &str| found(&op("regex", q(query, "t", true))).unwrap_err();
    assert_eq!(r("("), "unexpected end-of-string");
    assert_eq!(r("a|"), "unexpected end-of-string");
    assert_eq!(r("a{2"), "expected '}' at position 3");
    assert_eq!(r("a{2,1}"), "invalid repetition range(out of order): 2..1");
    assert_eq!(r("[z-a]"), "invalid range: from (122) cannot be > to (97)");
}

/// Measured on the keyword field `k` (`scratchpad/lane3i/w4.js`).
#[test]
fn regex_syntax_follows_refsearch() {
    let r = |query: &str| found(&op("regex", q(query, "k", false)));
    for ok in ["*a", "a}", "a>", "x", "()", "(|a)", "a||b", "g\\D+", "[\\d]"] {
        assert_eq!(r(ok), Ok(vec![]), "{ok}");
    }
    assert_eq!(r("\\S+"), Ok(ones(&[3])));
    let err = |query: &str| r(query).unwrap_err();
    assert_eq!(err("ab)"), "end-of-string expected at position 2");
    assert_eq!(err("(a]"), "expected ')' at position 3");
    assert_eq!(err("a{"), "integer expected at position 2");
    assert_eq!(err("a{x}"), "integer expected at position 2");
    assert_eq!(err("a{2x"), "expected '}' at position 3");
    assert_eq!(err("[]"), "expected ']' at position 2");
    assert_eq!(err("[^]"), "expected ']' at position 3");
    assert_eq!(err("[ab"), "expected ']' at position 3");
    assert_eq!(err("<1x2>"), "'1x2' not found");
    assert_eq!(err("<1-2"), "expected '>' at position 4");
    assert_eq!(err("a\\"), "unexpected end-of-string");
    assert_eq!(err("\"abc"), "expected '\"' at position 4");
    assert_eq!(err("<1-x>"), "interval syntax error at position 0");
    assert_eq!(err("a{1001}"), "invalid repetition range(out of order): 1001..1001");
    assert_eq!(err("(.{1000}){1000}"), "the pattern has too many states");
    let empty = Document::new().with("query", "").with("path", "k");
    assert_eq!(found(&op("regex", empty)).unwrap_err(), "\"regex.query\" cannot be empty");
}
