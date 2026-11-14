//! `$vectorSearch` over the corpus measured on a 9.0.4 replica set with the reference search server 1.70.6
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
fn scores_like_the_reference() {
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
fn limits_and_prefilters_like_the_reference() {
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

/// Deterministic pseudo-random vectors (SplitMix64) with components in [-1, 1).
fn random_vectors(n: usize, dims: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f64 / (1u64 << 23) as f64 * 2.0 - 1.0
    };
    (0..n).map(|_| (0..dims).map(|_| next()).collect()).collect()
}

const DIMS: usize = 16;

fn wide_definition() -> VectorDefinition {
    let vector = Document::new()
        .with("type", "vector")
        .with("path", "v")
        .with("numDimensions", DIMS as i32)
        .with("similarity", "euclidean");
    let def = Document::new().with("fields", Value::Array(vec![Value::Document(vector)]));
    VectorDefinition::parse(&def).expect("valid")
}

/// Document `id` with vector `v`, under a key in `_id` order.
fn numbered(id: usize, v: &[f64]) -> (Vec<u8>, Option<Document>) {
    let doc = Document::new().with("_id", id as i32).with("v", floats(v));
    ((id as u32).to_be_bytes().to_vec(), Some(doc))
}

fn key(id: usize) -> Vec<u8> {
    (id as u32).to_be_bytes().to_vec()
}

fn near(v: &[f64], limit: i32, exact: bool) -> VectorQuery {
    let mut d =
        Document::new().with("index", "ve").with("path", "v").with("queryVector", floats(v));
    d = d.with("limit", limit);
    d = if exact { d.with("exact", true) } else { d.with("numCandidates", limit * 10) };
    VectorQuery::parse(&d).expect("valid")
}

/// The mean recall@10 of `ix` over `queries` against the exact search.
fn recall(ix: &VectorIndexer, queries: &[Vec<f64>]) -> f64 {
    let (mut hits, mut total) = (0, 0);
    for q in queries {
        let want = ids(ix, &near(q, 10, true));
        let got = ids(ix, &near(q, 10, false));
        hits += want.iter().filter(|d| got.contains(d)).count();
        total += want.len();
    }
    hits as f64 / total as f64
}

/// Documents linked into every graph of `ix` so far: the work of building it.
fn insertions(ix: &VectorIndexer) -> u64 {
    ix.segments.iter().flat_map(|s| &s.graphs).map(|g| g.hnsw.insertions()).sum()
}

fn dead(ix: &VectorIndexer) -> usize {
    ix.segments.iter().map(|s| s.dead).sum()
}

/// Linking documents one change at a time, in an order that is not the key order, finds what a
/// segment built whole in key order finds.
#[test]
fn incremental_links_find_what_a_whole_build_finds() {
    let n = 1_500;
    let data = random_vectors(n, DIMS, 11);
    let queries = random_vectors(50, DIMS, 12);
    let mut ix = VectorIndexer::new(wide_definition());
    for i in 0..n {
        let id = i * 7_919 % n;
        ix.apply([numbered(id, &data[id])]);
    }
    assert_eq!((ix.len(), insertions(&ix), dead(&ix)), (n, n as u64, 0));
    let mut whole = ix.clone();
    let fields = whole.fields.clone();
    whole.segments.iter_mut().for_each(|s| s.rebuild(&fields));
    let (linked, built) = (recall(&ix, &queries), recall(&whole, &queries));
    assert!(linked >= 0.95, "incremental recall {linked}");
    assert!(linked >= built - 0.01, "incremental {linked}, whole {built}");
    // The same changes in the same order build the same graph.
    let mut again = VectorIndexer::new(wide_definition());
    for i in 0..n {
        let id = i * 7_919 % n;
        again.apply([numbered(id, &data[id])]);
    }
    let graph = |ix: &VectorIndexer| ix.segments[0].graphs[0].hnsw.encode();
    assert_eq!(graph(&again), graph(&ix));
}

/// Deleted and replaced versions stay in the graph as tombstones and are never returned; past
/// the share of tombstones the segment is built again without them.
#[test]
fn deleted_and_replaced_versions_are_never_returned() {
    let n = 400;
    let data = random_vectors(n, DIMS, 21);
    let mut ix = VectorIndexer::new(wide_definition());
    ix.apply((0..n).map(|i| numbered(i, &data[i])));
    let far = [5.0; DIMS];
    let deleted: Vec<usize> = (0..n).step_by(10).collect();
    let replaced: Vec<usize> = (1..n).step_by(10).collect();
    ix.apply(deleted.iter().map(|&i| (key(i), None)));
    ix.apply(replaced.iter().map(|&i| numbered(i, &far)));
    assert_eq!((ix.len(), dead(&ix)), (n - deleted.len(), deleted.len() + replaced.len()));
    let current = |id: i32| match id as usize {
        i if replaced.contains(&i) => far.to_vec(),
        i => data[i].clone(),
    };
    let check = |ix: &VectorIndexer, q: &[f64], exact: bool| {
        let found = run(ix, &near(q, 50, exact));
        assert_eq!(found.len(), 50);
        for (id, score) in found {
            assert!(!deleted.contains(&(id as usize)), "deleted {id} returned");
            let v: Vec<f32> = current(id).iter().map(|x| *x as f32).collect();
            let q: Vec<f32> = q.iter().map(|x| *x as f32).collect();
            assert_eq!(score, f64::from(Similarity::Euclidean.score(&v, &q)), "{id}");
        }
    };
    for &i in deleted.iter().chain(&replaced).take(20) {
        check(&ix, &data[i], true);
        check(&ix, &data[i], false);
    }
    // The replaced documents are found at their new vector, all copies of the same one: in the
    // graph and once built again whole.
    let replaced_ids: Vec<i32> = replaced.iter().map(|&i| i as i32).collect();
    let at_far = near(&far, replaced.len() as i32, false);
    assert_eq!(ids(&ix, &at_far), replaced_ids);
    let mut whole = ix.clone();
    let fields = whole.fields.clone();
    whole.segments.iter_mut().for_each(|s| s.rebuild(&fields));
    assert_eq!(ids(&whole, &at_far), replaced_ids);
    // More than a quarter of the nodes as tombstones builds the segment again, in key order.
    ix.apply((2..n).step_by(10).map(|i| (key(i), None)));
    assert_eq!(dead(&ix), 0);
    assert_eq!(ix.segments[0].nodes.len(), ix.len());
    assert!(ix.segments[0].docs.values().enumerate().all(|(i, d)| d.node as usize == i));
    check(&ix, &data[3], false);
}

/// The work of building grows with the changes, not with their square: one document per change,
/// then one replacement per change, then deletes.
#[test]
fn building_costs_grow_linearly_with_the_changes() {
    let cost = |n: usize| {
        let data = random_vectors(n, DIMS, 31);
        let mut ix = VectorIndexer::new(wide_definition());
        for (i, v) in data.iter().enumerate() {
            ix.apply([numbered(i, v)]);
        }
        assert_eq!(insertions(&ix), n as u64, "one link per new document");
        for (i, v) in data.iter().enumerate() {
            ix.apply([numbered(i, &v.iter().map(|x| -x).collect::<Vec<_>>())]);
        }
        for i in (0..n).step_by(2) {
            ix.apply([(key(i), None)]);
        }
        insertions(&ix)
    };
    let (small, large) = (cost(200), cost(400));
    // Each change links at most a few documents, rebuilds included.
    assert!(small <= 4 * 200 * 2, "{small}");
    assert!(large <= 4 * 400 * 2, "{large}");
    assert!(large as f64 <= 2.2 * small as f64, "200: {small}, 400: {large}");
}

/// Splitting and merging ranges builds the segments that take documents again: a query finds
/// the same, and no tombstone travels.
#[test]
fn reshaping_builds_the_segments_that_take_documents() {
    let n = 300;
    let data = random_vectors(n, DIMS, 41);
    let mut ix = VectorIndexer::new(wide_definition());
    for (i, v) in data.iter().enumerate() {
        ix.apply([numbered(i, v)]);
    }
    ix.apply((0..n).step_by(7).map(|i| (key(i), None)));
    assert!(dead(&ix) > 0);
    let q = near(&data[5], 10, true);
    let before = run(&ix, &q);
    ix.reshape(&[(Vec::new(), key(150)), (key(150), Vec::new())]);
    assert_eq!((ix.segments(), dead(&ix)), (2, 0));
    assert_eq!(run(&ix, &q), before);
    // A segment whose range stays keeps its graph and its tombstones.
    ix.apply([(key(1), None)]);
    ix.reshape(&[(Vec::new(), key(150)), (key(150), key(200)), (key(200), Vec::new())]);
    assert_eq!((ix.segments(), dead(&ix)), (3, 1));
    assert_eq!(ix.segments[0].graphs[0].hnsw.insertions(), ix.segments[0].nodes.len() as u64);
    let found = ids(&ix, &q);
    assert_eq!(found.len(), 10);
    assert!(!found.contains(&1));
}
