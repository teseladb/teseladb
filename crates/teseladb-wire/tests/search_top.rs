//! `aggregate` with `$search` and a `$limit` after it gives the first documents of the whole
//! result, scores and tokens included (011 FR-030, FR-031, T058, T059): the node hands the
//! limit to the index, which keeps only the best, and reads the documents a batch at a time.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::extjson;
use teseladb_bson::{Document, RawDoc, Value};

const DB: &str = "t";

fn parse(json: &str) -> Document {
    extjson::parse(json).unwrap()
}

fn canonical(v: &[Value]) -> String {
    let d = Document::new().with("v", v.to_vec());
    extjson::to_relaxed(RawDoc::from_bytes(&d.to_bytes().unwrap()).unwrap())
}

fn aggregate(m: &mut Mem, pipeline: &str) -> Vec<Value> {
    let wrapped = parse(&format!(r#"{{"p": {pipeline}}}"#));
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", wrapped.get("p").unwrap().clone())
        .with("cursor", Document::new().with("batchSize", 10_000));
    let r = m.run(DB, cmd);
    assert!(r.get("errmsg").is_none(), "{r:?}");
    let Value::Array(batch) = field(&r, "cursor.firstBatch").clone() else { panic!("a batch") };
    batch
}

const WORDS: [&str; 8] = ["gato", "perro", "raton", "queso", "casa", "rio", "sol", "luna"];

/// 300 documents of a few words, many with equal scores, and an index with `storedSource`.
fn corpus() -> Mem {
    let mut m = Mem::new();
    m.now = 1_793_559_095_123;
    let docs: Vec<Value> = (0..300u32)
        .map(|i| {
            let words: Vec<&str> =
                (0..1 + i % 5).map(|j| WORDS[((i * 7 + j * 3) % 8) as usize]).collect();
            Document::new()
                .with("_id", ((i * 37) % 300) as i32)
                .with("t", words.join(" "))
                .with("n", (i % 17) as i32)
                .into()
        })
        .collect();
    m.run(DB, Document::new().with("insert", "c").with("documents", docs));
    let definition =
        parse(r#"{"mappings": {"dynamic": true}, "storedSource": {"include": ["t"]}}"#);
    let index = doc(&[("name", "default".into()), ("definition", definition.into())]);
    let indexes = Value::Array(vec![index.into()]);
    m.run(DB, Document::new().with("createSearchIndexes", "c").with("indexes", indexes));
    m
}

const PROJECT: &str = r#"{"$project": {"t": 1, "n": 1, "s": {"$meta": "searchScore"},
    "k": {"$meta": "searchSequenceToken"}}}"#;

#[test]
fn a_limit_gives_the_first_documents_of_the_whole_result() {
    let mut m = corpus();
    for search in [
        r#"{"text": {"path": "t", "query": "gato perro sol"}}"#,
        r#"{"text": {"path": "t", "query": "queso", "score": {"boost": {"value": 2}}}}"#,
        r#"{"compound": {"must": [{"text": {"path": "t", "query": "gato luna"}}],
            "filter": [{"range": {"path": "n", "gte": 5}}],
            "mustNot": [{"equals": {"path": "n", "value": 9}}]}}"#,
        r#"{"text": {"path": "t", "query": "rio"}, "returnStoredSource": true}"#,
    ] {
        let all = aggregate(&mut m, &format!(r#"[{{"$search": {search}}}, {PROJECT}]"#));
        assert!(all.len() > 20, "{search}");
        for (skip, limit) in [(0, 1), (0, 5), (3, 10), (0, 64), (60, 70), (0, 1000)] {
            let got = aggregate(
                &mut m,
                &format!(
                    r#"[{{"$search": {search}}}, {{"$skip": {skip}}}, {{"$limit": {limit}}},
                        {PROJECT}]"#
                ),
            );
            let want: Vec<Value> = all.iter().skip(skip).take(limit).cloned().collect();
            assert_eq!(canonical(&got), canonical(&want), "{search} {skip} {limit}");
        }
    }
}
