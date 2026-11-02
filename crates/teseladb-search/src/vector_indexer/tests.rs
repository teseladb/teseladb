//! `$vectorSearch` over the corpus measured on a 9.0.4 replica set with `refsearch` 1.70.6
//! (01-11-2026): the documents of `scratchpad lane3g/v1.js`, query vector `[1, 0.2, 0.1]`.

use teseladb_bson::{Document, Value};

use super::*;

fn floats(xs: &[f64]) -> Value {
    Value::Array(xs.iter().map(|x| Value::Double(*x)).collect())
}

/// The measured documents, keyed by their `_id`.
fn corpus() -> Vec<(Vec<u8>, Option<Document>)> {
    let d = |id: i32, v: Option<Value>, g: Value| {
        let mut doc = Document::new().with("_id", id);
        if let Some(v) = v {
            doc.push("v", v);
        }
        (vec![id as u8], Some(doc.with("g", g).with("n", id)))
    };
    vec![
        d(1, Some(floats(&[1.0, 0.0, 0.0])), "a".into()),
        d(2, Some(floats(&[0.0, 1.0, 0.0])), "b".into()),
        d(3, Some(floats(&[0.5, 0.5, 0.0])), "a".into()),
        d(4, Some(floats(&[1.0, 1.0, 1.0])), "b".into()),
        d(5, Some(floats(&[-1.0, 0.0, 0.5])), "a".into()),
        d(6, Some(floats(&[1.0, 2.0])), "a".into()),
        d(7, None, "b".into()),
        d(8, Some("str".into()), "a".into()),
        d(9, Some(floats(&[0.2, 0.3, 0.9])), Value::Array(vec!["a".into(), "b".into()])),
    ]
}

fn definition(similarity: &str) -> VectorDefinition {
    let field = |t: &str, p: &str| Value::Document(Document::new().with("type", t).with("path", p));
    let vector = Document::new()
        .with("type", "vector")
        .with("path", "v")
        .with("numDimensions", 3)
        .with("similarity", similarity);
    let def = Document::new().with(
        "fields",
        Value::Array(vec![Value::Document(vector), field("filter", "g"), field("filter", "n")]),
    );
    VectorDefinition::parse(&def).expect("valid")
}

fn indexer(similarity: &str) -> VectorIndexer {
    let mut ix = VectorIndexer::new(definition(similarity));
    ix.apply(corpus());
    ix
}

fn query(edit: impl FnOnce(&mut Document)) -> VectorQuery {
    let mut d = Document::new()
        .with("index", "ve")
        .with("path", "v")
        .with("queryVector", floats(&[1.0, 0.2, 0.1]))
        .with("numCandidates", 10)
        .with("limit", 10);
    edit(&mut d);
    VectorQuery::parse(&d).expect("valid")
}

/// `(_id, score)` of what `q` finds.
fn run(ix: &VectorIndexer, q: &VectorQuery) -> Vec<(i32, f64)> {
    ix.search(q)
        .expect("runs")
        .into_iter()
        .map(|f| match f.doc.get("_id") {
            Some(Value::Int32(id)) => (*id, f64::from(f.score)),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ids(ix: &VectorIndexer, q: &VectorQuery) -> Vec<i32> {
    run(ix, q).into_iter().map(|(id, _)| id).collect()
}

#[test]
fn scores_like_refsearch() {
    let measured: [(&str, [(i32, f64); 6]); 3] = [
        (
            "euclidean",
            [
                (1, 0.9523810148239136),
                (3, 0.7407407164573669),
                (9, 0.43668124079704285),
                (4, 0.40816324949264526),
                (2, 0.37735846638679504),
                (5, 0.19230769574642181),
            ],
        ),
        (
            "cosine",
            [
                (1, 0.9879500865936279),
                (3, 0.9140393733978271),
                (4, 0.8662335276603699),
                (9, 0.6761487126350403),
                (2, 0.5975900292396545),
                (5, 0.0853860080242157),
            ],
        ),
        (
            "dotProduct",
            [
                (4, 1.1500000953674316),
                (1, 1.0),
                (3, 0.800000011920929),
                (9, 0.675000011920929),
                (2, 0.6000000238418579),
                (5, 0.025000005960464478),
            ],
        ),
    ];
    for (sim, want) in measured {
        let ix = indexer(sim);
        assert_eq!(ix.len(), 9);
        assert_eq!(run(&ix, &query(|_| {})), want.to_vec(), "{sim}");
        let exact = query(|d| {
            d.remove("numCandidates");
            d.push("exact", true);
        });
        assert_eq!(run(&ix, &exact), want.to_vec(), "{sim} exact");
    }
}

#[test]
fn limits_and_prefilters_like_refsearch() {
    let ix = indexer("euclidean");
    assert_eq!(ids(&ix, &query(|d| drop(d.insert("limit", 2)))), [1, 3]);
    let f = |f: Document| query(|d| d.push("filter", f));
    let op =
        |field: &str, op: &str, v: Value| Document::new().with(field, Document::new().with(op, v));
    assert_eq!(ids(&ix, &f(Document::new().with("g", "a"))), [1, 3, 9, 5]);
    let range = op("n", "$gte", 3.into())
        .with("g", Document::new().with("$in", Value::Array(vec!["b".into()])));
    assert_eq!(ids(&ix, &f(range)), [9, 4]);
    let or = Document::new().with(
        "$or",
        Value::Array(vec![
            Value::Document(Document::new().with("g", "b")),
            Value::Document(Document::new().with("n", 1)),
        ]),
    );
    assert_eq!(ids(&ix, &f(or)), [1, 9, 4, 2]);
    assert_eq!(ids(&ix, &f(op("n", "$eq", 3.into()))), [3]);
    assert_eq!(ids(&ix, &f(op("n", "$ne", 3.into()))), [1, 9, 4, 2, 5]);
    assert_eq!(ids(&ix, &f(op("g", "$nin", Value::Array(vec!["a".into()])))), [4, 2]);
    assert_eq!(ids(&ix, &f(op("n", "$exists", true.into()))), [1, 3, 9, 4, 2, 5]);
    assert_eq!(ids(&ix, &f(op("n", "$exists", false.into()))), Vec::<i32>::new());
    assert_eq!(ids(&ix, &f(op("n", "$not", Document::new().with("$gt", 3).into()))), [1, 3, 2]);
    assert_eq!(ids(&ix, &f(op("g", "$lt", "b".into()))), [1, 3, 9, 5]);
    assert_eq!(ids(&ix, &f(Document::new().with("g", Value::Null))), Vec::<i32>::new());
    assert_eq!(ids(&ix, &f(op("n", "$gt", "a".into()))), Vec::<i32>::new());
    assert_eq!(ids(&ix, &f(Document::new().with("n", 3.0))), [3]);
    let not_in =
        op("n", "$not", Document::new().with("$in", Value::Array(vec![1.into(), 2.into()])).into());
    assert_eq!(ids(&ix, &f(not_in)), [3, 9, 4, 5]);
    // Errors of the index come first.
    let bad = query(|d| drop(d.insert("path", "w")));
    assert_eq!(ix.search(&bad).expect_err("not vector").0, "w is not indexed as vector");
}

#[test]
fn segments_split_and_merge_without_changing_results() {
    let mut ix = indexer("cosine");
    let whole = run(&ix, &query(|_| {}));
    ix.reshape(&[(Vec::new(), vec![4]), (vec![4], vec![7]), (vec![7], Vec::new())]);
    assert_eq!(ix.segments(), 3);
    assert_eq!(ix.len(), 9);
    assert_eq!(run(&ix, &query(|_| {})), whole);
    let two = query(|d| drop(d.insert("limit", 2)));
    assert_eq!(ids(&ix, &two), [1, 3]);
    ix.reshape(&[]);
    assert_eq!(ix.segments(), 1);
    assert_eq!(run(&ix, &query(|_| {})), whole);
    // A delete and a replacement.
    ix.apply([
        (vec![1], None),
        (vec![2], Some(Document::new().with("_id", 2).with("v", floats(&[1.0, 0.2, 0.1])))),
    ]);
    assert_eq!(ids(&ix, &query(|d| drop(d.insert("limit", 2)))), [2, 3]);
    assert!(!ix.is_empty());
    assert_eq!(ix.definition().fields.len(), 3);
    // A key outside every segment is left out.
    ix.reshape(&[(vec![5], Vec::new())]);
    ix.apply([(vec![1], Some(Document::new().with("v", floats(&[1.0, 0.0, 0.0]))))]);
    assert_eq!(ix.len(), 5);
}

#[test]
fn reads_bson_vectors() {
    let mut f32_bytes = vec![FLOAT32, 0];
    for x in [1.0f32, 0.0, 0.0] {
        f32_bytes.extend_from_slice(&x.to_le_bytes());
    }
    let bin = |bytes: Vec<u8>| Value::Binary { subtype: VECTOR_SUBTYPE, bytes };
    assert_eq!(vector_of(&bin(f32_bytes)), Some(vec![1.0, 0.0, 0.0]));
    assert_eq!(vector_of(&bin(vec![INT8, 0, 1, 0xff])), Some(vec![1.0, -1.0]));
    assert_eq!(vector_of(&bin(vec![FLOAT32, 0, 1])), None);
    assert_eq!(vector_of(&bin(vec![0x10, 0, 1])), None);
    assert_eq!(vector_of(&Value::Binary { subtype: 0, bytes: vec![] }), None);
    assert_eq!(
        vector_of(&Value::Array(vec![Value::Int64(2), Value::Int32(1)])),
        Some(vec![2.0, 1.0])
    );
    assert_eq!(vector_of(&Value::Array(vec!["a".into()])), None);
    let mut ix = VectorIndexer::new(definition("euclidean"));
    let mut bytes = vec![FLOAT32, 0];
    for x in [1.0f32, 0.2, 0.1] {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    ix.apply([(vec![1], Some(Document::new().with("_id", 1).with("v", bin(bytes))))]);
    assert_eq!(run(&ix, &query(|_| {})), vec![(1, 1.0)]);
}

/// Equal scores go in `_id` order, a segment whose range stays keeps its graph, and an empty
/// segment finds nothing.
#[test]
fn ties_kept_segments_and_empty_segments() {
    let mut ix = VectorIndexer::new(definition("euclidean"));
    let same = |id: i32| {
        let d = Document::new().with("_id", id).with("v", floats(&[1.0, 0.0, 0.0]));
        (vec![id as u8], Some(d))
    };
    ix.apply([same(3), same(1), same(2)]);
    assert_eq!(ids(&ix, &query(|_| {})), [1, 2, 3]);
    // A document without `_id` keeps its place among the ties.
    ix.apply([(vec![4], Some(Document::new().with("v", floats(&[1.0, 0.0, 0.0]))))]);
    assert_eq!(ix.search(&query(|_| {})).expect("runs").len(), 4);
    ix.reshape(&[(Vec::new(), vec![3]), (vec![3], Vec::new())]);
    let four =
        [(Vec::new(), vec![3]), (vec![3], vec![4]), (vec![4], vec![9]), (vec![9], Vec::new())];
    ix.reshape(&four);
    assert_eq!(ix.segments(), 4);
    assert_eq!(ix.search(&query(|_| {})).expect("runs").len(), 4);
}
