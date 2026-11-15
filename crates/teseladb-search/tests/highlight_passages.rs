//! The `highlight` option (011 FR-004, T035, T047): the checks of its fields with the messages
//! of `query/search/highlight.rs`, and passages cut by the sentence rules it documents (a `!`,
//! `?` or `。` ends a sentence with the closing marks and spaces after it; a period ends one
//! when more than one space follows or the character after one space is no lower-case letter
//! or digit; U+2029 ends one, and between the values of an array it is cut out), each passage
//! holding the query's terms as `hit` pieces and the rest as `text` pieces.

use teseladb_bson::{Document, Value};
use teseladb_search::definition::Definition;
use teseladb_search::indexer::Indexer;

fn index(docs: &[Document]) -> Indexer {
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    let mut ix = Indexer::new(Definition::parse(&def).expect("valid"));
    let changes = docs.iter().enumerate().map(|(i, d)| (vec![i as u8], Some(d.clone())));
    ix.apply(changes.collect::<Vec<_>>()).expect("indexed");
    ix
}

fn text(query: &str) -> Document {
    Document::new().with("text", Document::new().with("query", query).with("path", "t"))
}

fn with_highlight(op: Document, highlight: impl Into<Value>) -> Document {
    op.with("highlight", highlight)
}

/// Each passage of the first document found as its pieces `(type, value)`.
fn passages(ix: &Indexer, spec: &Document) -> Vec<Vec<(String, String)>> {
    let found = ix.search(spec).expect("searched");
    let Some(Value::Array(ps)) = found[0].highlights.clone() else { panic!("highlights") };
    ps.iter()
        .map(|p| {
            let Value::Document(p) = p else { panic!("a passage") };
            let Some(Value::Array(texts)) = p.get("texts") else { panic!("texts") };
            texts
                .iter()
                .map(|t| {
                    let Value::Document(t) = t else { panic!("a piece") };
                    let s = |k: &str| match t.get(k) {
                        Some(Value::String(s)) => s.clone(),
                        _ => panic!("{k}"),
                    };
                    (s("type"), s("value"))
                })
                .collect()
        })
        .collect()
}

/// The text of each passage.
fn joined(ps: &[Vec<(String, String)>]) -> Vec<String> {
    ps.iter().map(|p| p.iter().map(|(_, v)| v.as_str()).collect()).collect()
}

fn hits(ps: &[Vec<(String, String)>]) -> Vec<String> {
    ps.iter().flatten().filter(|(k, _)| k == "hit").map(|(_, v)| v.clone()).collect()
}

#[test]
fn the_fields_of_highlight_are_checked() {
    let ix = index(&[Document::new().with("t", "gato")]);
    let err = |h: Value| ix.search(&with_highlight(text("gato"), h)).map(|_| ()).unwrap_err().0;
    let path = |extra: (&str, Value)| Document::new().with("path", "t").with(extra.0, extra.1);
    assert_eq!(err(Value::from(3)), "\"highlight\" must be a document");
    assert_eq!(
        err(path(("other", Value::from(1))).into()),
        "\"highlight\" unrecognized field \"other\""
    );
    assert_eq!(err(Document::new().into()), "\"highlight.path\" is required");
    let passages = |v: Value| err(path(("maxNumPassages", v)).into());
    assert_eq!(passages(Value::from(0)), "\"highlight.maxNumPassages\" must be positive");
    assert_eq!(passages(Value::from(i32::MAX)), "IllegalArgumentException");
    assert_eq!(
        passages(Value::Double(f64::NAN)),
        "\"highlight.maxNumPassages\" must be an integer"
    );
    assert_eq!(passages(Value::Double(1.5)), "\"highlight.maxNumPassages\" must be an integer");
    assert_eq!(passages(Value::from("2")), "\"highlight.maxNumPassages\" must be a integer");
    let chars = |v: Value| err(path(("maxCharsToExamine", v)).into());
    assert_eq!(chars(Value::from(i32::MAX)), "maxLength must be < Integer.MAX_VALUE");
    assert_eq!(
        chars(Value::Int64(1 << 40)),
        "\"highlight.maxCharsToExamine\" overflowed, must fit in a 32 bit integer"
    );
    assert_eq!(
        chars(Value::Double(f64::NEG_INFINITY)),
        "\"highlight.maxCharsToExamine\" underflowed, must fit in a 32 bit integer"
    );
    assert_eq!(
        chars(Value::Double(f64::INFINITY)),
        "\"highlight.maxCharsToExamine\" overflowed, must fit in a 32 bit integer"
    );
    // A null takes the default, and a whole double counts as its integer.
    let ok = |extra: (&str, Value)| ix.search(&with_highlight(text("gato"), path(extra)));
    assert!(ok(("maxNumPassages", Value::Null)).is_ok());
    assert!(ok(("maxNumPassages", Value::Double(2.0))).is_ok());
    assert!(ok(("maxCharsToExamine", Value::Int64(100))).is_ok());
}

#[test]
fn passages_follow_the_sentence_rules() {
    let t =
        "Gato uno! Perro dos? Gato tres. gato cuatro. Gato cinco.  Gato seis。gato siete. 2 gatos";
    let ix = index(&[Document::new().with("t", t)]);
    let ps = passages(
        &ix,
        &with_highlight(text("gato"), Document::new().with("path", "t").with("maxNumPassages", 10)),
    );
    // `!` and `?` end a sentence; a period followed by a lower-case letter or a digit does
    // not; two spaces do; `。` does.
    assert_eq!(
        joined(&ps),
        [
            "Gato uno! ",
            "Gato tres. gato cuatro. ",
            "Gato cinco.  ",
            "Gato seis。",
            "gato siete. 2 gatos",
        ]
    );
    assert_eq!(hits(&ps), ["Gato", "Gato", "gato", "Gato", "Gato", "gato"]);
    // The best passages, as many as asked, come back in text order.
    let two = passages(
        &ix,
        &with_highlight(text("gato"), Document::new().with("path", "t").with("maxNumPassages", 2)),
    );
    assert_eq!(two.len(), 2);
    let all = joined(&ps);
    let order: Vec<usize> =
        joined(&two).iter().map(|p| all.iter().position(|a| a == p).unwrap()).collect();
    assert!(order[0] < order[1]);
}

#[test]
fn values_of_an_array_are_passages_apart_and_the_examined_text_is_cut() {
    let doc = Document::new().with(
        "t",
        vec![Value::from("gato negro"), Value::from(7), Value::from("otro gato\u{2029}gato")],
    );
    let ix = index(&[doc]);
    let spec = with_highlight(text("gato"), Document::new().with("path", "t"));
    let ps = passages(&ix, &spec);
    assert_eq!(joined(&ps), ["gato negro", "otro gato", "gato"]);
    // Only the first 12 UTF-16 units are examined: the hits after them are left out.
    let short = Document::new().with("path", "t").with("maxCharsToExamine", 12);
    let ps = passages(&ix, &with_highlight(text("gato"), short));
    assert_eq!(joined(&ps), ["gato negro"]);
    // In one string, U+2029 ends a sentence and stays in it.
    let ix = index(&[Document::new().with("t", "gato uno\u{2029}dos gato")]);
    assert_eq!(joined(&passages(&ix, &spec)), ["gato uno\u{2029}", "dos gato"]);
    // A path with no string gives no passages.
    let ix = index(&[Document::new().with("t", "gato").with("n", 3)]);
    let found =
        ix.search(&with_highlight(text("gato"), Document::new().with("path", "n"))).unwrap();
    assert_eq!(found[0].highlights, Some(Value::Array(vec![])));
}

#[test]
fn phrases_patterns_fuzzy_terms_and_compounds_highlight_their_terms() {
    let t = "el gato negro duerme y el perro negro come";
    let ix = index(&[Document::new().with("t", t)]);
    let h = || Document::new().with("path", "t");
    let phrase = Document::new()
        .with("phrase", Document::new().with("query", "gato negro").with("path", "t"));
    assert_eq!(hits(&passages(&ix, &with_highlight(phrase, h()))), ["gato", "negro"]);
    let slop = Document::new().with(
        "phrase",
        Document::new().with("query", "gato duerme").with("path", "t").with("slop", 1),
    );
    assert_eq!(hits(&passages(&ix, &with_highlight(slop, h()))), ["gato", "duerme"]);
    let wildcard = Document::new().with(
        "wildcard",
        Document::new().with("query", "ne*").with("path", "t").with("allowAnalyzedField", true),
    );
    assert_eq!(hits(&passages(&ix, &with_highlight(wildcard, h()))), ["negro", "negro"]);
    let fuzzy = Document::new().with(
        "text",
        Document::new()
            .with("query", "pero")
            .with("path", "t")
            .with("fuzzy", Document::new().with("maxEdits", 1)),
    );
    assert_eq!(hits(&passages(&ix, &with_highlight(fuzzy, h()))), ["perro"]);
    // `mustNot` asks for nothing; `must`, `should` and `filter` do.
    let compound = Document::new().with(
        "compound",
        Document::new()
            .with("must", vec![Value::from(text("gato"))])
            .with("should", vec![Value::from(text("come"))])
            .with("filter", vec![Value::from(text("duerme"))])
            .with("mustNot", vec![Value::from(text("pez"))]),
    );
    assert_eq!(hits(&passages(&ix, &with_highlight(compound, h()))), ["gato", "duerme", "come"]);
    let constant = Document::new().with("constant", Document::new().with("value", 3));
    let boosted = Document::new().with(
        "text",
        Document::new().with("query", "perro").with("path", "t").with("score", constant),
    );
    assert_eq!(hits(&passages(&ix, &with_highlight(boosted, h()))), ["perro"]);
}
