//! The `$text` and search stages first in the pipelines of `$lookup` and `$unionWith` read the
//! indexes of the collection the stage joins, and a `$match` with `$text` in a `$facet` has none
//! to read (011 T054, FR-004; answers measured on a `refnode` 9.0.4 with `refsearch` 1.70.6, whose
//! cases against a node are in `teseladb-wire/tests/text_search.rs`).

use std::cell::RefCell;
use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::search::Scored;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// What the environment was asked for: the index read and the collection it belongs to.
#[derive(Clone, Debug, PartialEq)]
enum Asked {
    /// The text index of the pipeline's own collection.
    OwnText,
    /// The text index of a joined collection, with the variables its filter sees.
    Text(String, Option<Document>),
    /// The search indexes of a joined collection: stage, collection, whether from a `$lookup`.
    Search(String, String, bool),
}

/// A node with collection `c` (`_id` 1 and 2) and `f` (`_id` 10 and 11), whose text and search
/// indexes of `f` give its two documents, each with `k` and a score.
#[derive(Default)]
struct JoinedEnv {
    asked: RefCell<Vec<Asked>>,
}

fn doc(id: i32, k: i32) -> Document {
    Document::new().with("_id", id).with("k", k)
}

fn foreign(score_name: &str) -> Vec<Scored> {
    [(10, 1, 0.75), (11, 2, 0.5)]
        .into_iter()
        .map(|(id, k, s)| {
            let mut meta = BTreeMap::new();
            meta.insert(score_name.to_owned(), Value::Double(s));
            Scored { doc: doc(id, k), meta }
        })
        .collect()
}

impl Env for JoinedEnv {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(match name {
            "c" => vec![doc(1, 1), doc(2, 2)],
            _ => panic!("a joined pipeline that starts with $text or a search reads no documents"),
        })
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn text_source(&self, _filter: &Document) -> Result<Vec<Scored>, MqlError> {
        self.asked.borrow_mut().push(Asked::OwnText);
        Ok(foreign("textScore"))
    }

    fn joined_text_source(
        &self,
        name: &str,
        _filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<Scored>, MqlError> {
        self.asked.borrow_mut().push(Asked::Text(name.to_owned(), vars.cloned()));
        Ok(foreign("textScore"))
    }

    fn joined_search_source(
        &self,
        name: &str,
        (stage, _spec): (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<Scored>, MqlError> {
        let asked = Asked::Search(stage.to_owned(), name.to_owned(), from_lookup);
        self.asked.borrow_mut().push(asked);
        Ok(foreign("searchScore"))
    }
}

fn raw(stages: &[Document]) -> Vec<Vec<u8>> {
    stages.iter().map(|s| s.to_bytes().expect("small")).collect()
}

fn values(bytes: &[Vec<u8>]) -> Vec<RawValue<'_>> {
    bytes.iter().map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document"))).collect()
}

/// The pipeline of an `aggregate` on a collection.
fn parse(stages: &[Document]) -> Result<Pipeline, MqlError> {
    let bytes = raw(stages);
    Pipeline::parse_command(&values(&bytes), &Ctx::default(), true)
}

/// Runs an `aggregate` on `c` with `stages`, and what it asked the environment for.
fn run(stages: &[Document]) -> (Result<Vec<Document>, MqlError>, Vec<Asked>) {
    let env = JoinedEnv::default();
    let docs = env.collection("c").expect("c");
    let out = parse(stages).and_then(|p| p.run(docs, &env));
    (out, env.asked.take())
}

fn code(r: Result<Vec<Document>, MqlError>) -> (i32, String) {
    let e = r.expect_err("an error");
    (e.code.code, e.message)
}

fn text() -> Document {
    Document::new()
        .with("$match", Document::new().with("$text", Document::new().with("$search", "fox")))
}

fn search() -> Document {
    Document::new().with("$search", Document::new().with("text", Document::new()))
}

fn pipeline(stages: Vec<Document>) -> Value {
    Value::Array(stages.into_iter().map(Value::Document).collect())
}

fn lookup(spec: Document) -> Document {
    Document::new().with("$lookup", spec.with("as", "x"))
}

/// Document `d` with `x` holding `joined`.
fn with_x(d: Document, joined: Vec<Document>) -> Document {
    d.with("x", pipeline(joined))
}

const TEXT_NOT_FIRST: &str = "$match with $text is only allowed as the first pipeline stage";
const TEXT_IN_FACET: &str = "Should never call getNext on a $match stage with $text clause";

#[test]
fn the_text_of_a_lookup_reads_the_text_index_of_from_with_the_let_variables() {
    let spec = Document::new().with("from", "f").with("pipeline", pipeline(vec![text()]));
    let (out, asked) = run(&[lookup(spec)]);
    let all = vec![doc(10, 1), doc(11, 2)];
    assert_eq!(out.unwrap(), [with_x(doc(1, 1), all.clone()), with_x(doc(2, 2), all)]);
    let once = Asked::Text("f".to_owned(), Some(Document::new()));
    assert_eq!(asked, [once.clone(), once]);
    // The variables of `let` reach the filter of the `$text`.
    let spec = Document::new()
        .with("from", "f")
        .with("let", Document::new().with("kk", "$k"))
        .with("pipeline", pipeline(vec![text()]));
    let (out, asked) = run(&[lookup(spec)]);
    assert_eq!(out.unwrap().len(), 2);
    let kk = |k: i32| Asked::Text("f".to_owned(), Some(Document::new().with("kk", k)));
    assert_eq!(asked, [kk(1), kk(2)]);
}

#[test]
fn a_lookup_with_local_field_applies_its_equality_to_what_its_search_gives() {
    let spec = Document::new()
        .with("from", "f")
        .with("localField", "k")
        .with("foreignField", "k")
        .with("pipeline", pipeline(vec![search()]));
    let (out, asked) = run(&[lookup(spec)]);
    assert_eq!(
        out.unwrap(),
        [with_x(doc(1, 1), vec![doc(10, 1)]), with_x(doc(2, 2), vec![doc(11, 2)])]
    );
    let once = Asked::Search("$search".to_owned(), "f".to_owned(), true);
    assert_eq!(asked, [once.clone(), once]);
}

#[test]
fn a_lookup_with_local_field_refuses_a_first_text_when_it_has_documents() {
    let spec = Document::new()
        .with("from", "f")
        .with("localField", "k")
        .with("foreignField", "k")
        .with("pipeline", pipeline(vec![text()]));
    let (out, asked) = run(&[lookup(spec.clone())]);
    assert_eq!(code(out), (17_313, TEXT_NOT_FIRST.to_owned()));
    assert!(asked.is_empty());
    // Without documents to join it answers nothing.
    let none = Document::new().with("$match", Document::new().with("_id", 99));
    let (out, _) = run(&[none, lookup(spec)]);
    assert_eq!(out.unwrap(), Vec::<Document>::new());
}

#[test]
fn the_text_and_search_of_a_union_with_read_the_indexes_of_its_collection() {
    let union = |first: Document| {
        Document::new().with(
            "$unionWith",
            Document::new().with("coll", "f").with("pipeline", pipeline(vec![first])),
        )
    };
    let (out, asked) = run(&[union(text())]);
    let want = [doc(1, 1), doc(2, 2), doc(10, 1), doc(11, 2)];
    assert_eq!(out.unwrap(), want);
    assert_eq!(asked, [Asked::Text("f".to_owned(), None)]);
    let (out, asked) = run(&[union(search())]);
    assert_eq!(out.unwrap(), want);
    assert_eq!(asked, [Asked::Search("$search".to_owned(), "f".to_owned(), false)]);
    // After a `$text`, each `$unionWith` adds documents with their own scores.
    let (out, asked) = run(&[text(), union(text()), union(search())]);
    let joined = [doc(10, 1), doc(11, 2)];
    assert_eq!(out.unwrap(), [joined.clone(), joined.clone(), joined].concat());
    let search = Asked::Search("$search".to_owned(), "f".to_owned(), false);
    assert_eq!(asked, [Asked::OwnText, Asked::Text("f".to_owned(), None), search]);
}

#[test]
fn a_text_in_a_facet_has_no_text_index_to_read() {
    let facet = |inner: Vec<Document>| {
        Document::new().with("$facet", Document::new().with("a", pipeline(inner)))
    };
    // Without a `$text` before it, the metadata check refuses it when the pipeline is parsed.
    assert_eq!(
        code(parse(&[facet(vec![text()])]).map(|_| Vec::new())),
        (40_218, "query requires text score metadata, but it is not available".to_owned())
    );
    // After one, it fails when it is planned and when it runs.
    let env = JoinedEnv::default();
    let after_text = parse(&[text(), facet(vec![text()])]).expect("parses");
    assert_eq!(
        code(after_text.check_plan(&env).map(|()| Vec::new())),
        (17_309, TEXT_IN_FACET.to_owned())
    );
    let (out, asked) = run(&[text(), facet(vec![text()])]);
    assert_eq!(code(out), (17_309, TEXT_IN_FACET.to_owned()));
    assert_eq!(asked, [Asked::OwnText]);
    // A `$facet` of a joined pipeline, which runs without metadata, fails the same way.
    let bytes = raw(&[facet(vec![text()])]);
    let joined = Pipeline::parse(&values(&bytes), &Ctx::default()).expect("parses");
    assert_eq!(code(joined.run(Vec::new(), &env)), (17_309, TEXT_IN_FACET.to_owned()));
    // A `$facet` without a `$text` plans, and so does a first `$text` on a node that checks
    // nothing before it runs.
    let plain = parse(&[text(), facet(vec![Document::new().with("$limit", 1)])]).expect("parses");
    assert_eq!(plain.check_plan(&env), Ok(()));
}
