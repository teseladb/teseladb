//! The distributed run of a pipeline (010 FR-008, T021, T022) against the reference evaluator
//! (T003) over the same documents: each pipeline, its documents cut into ranges in every way of a
//! seeded sequence, gives what the reference gives over all of them (as sets where RefDb
//! leaves the order open).

use teseladb_bson::{Document, Value, extjson};
use teseladb_exec::distribute::{Merge, run_distributed, shuffle, split};
use teseladb_exec::reference::Reference;

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn pipeline(text: &str) -> Vec<Document> {
    match doc(&format!(r#"{{"p":{text}}}"#)).get("p") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn shown(docs: &[Document], sorted: bool) -> Vec<String> {
    let mut out: Vec<String> = docs
        .iter()
        .map(|d| {
            let b = d.to_bytes().expect("small");
            extjson::to_canonical(teseladb_bson::RawDoc::from_bytes(&b).expect("a document"))
        })
        .collect();
    if sorted {
        out.sort();
    }
    out
}

fn data() -> Vec<Document> {
    (0..60)
        .map(|i| {
            let mut d = Document::new()
                .with("_id", i)
                .with("g", ["a", "b", "c", "d"][i as usize % 4])
                .with("n", (i * 7) % 11)
                .with("t", vec![Value::Int32(i % 3), Value::Int32(i % 5)]);
            if i % 6 != 0 {
                d.push("x", f64::from(i) / 4.0);
            }
            d
        })
        .collect()
}

/// `docs` cut into `n` ranges at seeded places.
fn cut(docs: &[Document], n: usize, seed: u64) -> Vec<Vec<Document>> {
    let mut state = seed;
    let mut marks: Vec<usize> = (1..n)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 33) as usize % (docs.len() + 1)
        })
        .collect();
    marks.sort_unstable();
    let mut out = Vec::new();
    let mut from = 0;
    for m in marks.into_iter().chain([docs.len()]) {
        out.push(docs[from..m.max(from)].to_vec());
        from = m.max(from);
    }
    out
}

fn check(text: &str, unordered: bool) {
    let stages = pipeline(text);
    let reference = Reference::new(1).with_collection("c", data());
    let want = shown(&reference.evaluate("c", &stages).expect("reference"), unordered);
    for seed in 0..20 {
        for n in [1, 2, 3, 7] {
            let parts = cut(&data(), n, seed);
            let got = run_distributed(&stages, parts, &reference).expect("distributed");
            assert_eq!(shown(&got, unordered), want, "{text} with {n} ranges, seed {seed}");
        }
    }
}

#[test]
fn streamed_stages_run_in_each_range() {
    check(r#"[{"$match":{"n":{"$gt":3}}},{"$project":{"g":1,"n":1}},{"$unwind":"$g"}]"#, false);
    check(r#"[{"$unwind":"$t"},{"$set":{"y":{"$add":["$t",1]}}},{"$unset":"x"}]"#, false);
}

#[test]
fn sorts_merge_the_sorted_ranges_and_limits_keep_their_top() {
    check(r#"[{"$sort":{"n":-1,"_id":1}},{"$limit":7}]"#, false);
    check(r#"[{"$match":{"g":"b"}},{"$sort":{"x":1,"_id":-1}},{"$skip":2},{"$limit":3}]"#, false);
    check(r#"[{"$limit":9},{"$project":{"_id":1}}]"#, false);
}

#[test]
fn groups_combine_their_partial_groups() {
    check(
        r#"[{"$group":{"_id":"$g","s":{"$sum":"$n"},"c":{"$count":{}},"lo":{"$min":"$x"},"hi":{"$max":"$n"},"f":{"$first":"$_id"},"l":{"$last":"$_id"},"m":{"$avg":"$n"}}}]"#,
        true,
    );
    check(
        r#"[{"$group":{"_id":{"$mod":["$n",3]},"p":{"$push":"$_id"}}},{"$sort":{"_id":1}}]"#,
        false,
    );
    check(
        r#"[{"$unwind":"$t"},{"$group":{"_id":"$t","u":{"$addToSet":"$g"}}},{"$project":{"_id":1,"n":{"$size":"$u"}}},{"$sort":{"_id":1}}]"#,
        false,
    );
    check(r#"[{"$group":{"_id":null,"a":{"$avg":"$x"},"o":{"$mergeObjects":{"k":"$g"}}}}]"#, true);
    // An accumulator that does not combine runs whole at the coordinator.
    check(r#"[{"$group":{"_id":"$g","d":{"$stdDevPop":"$n"}}},{"$sort":{"_id":1}}]"#, false);
}

#[test]
fn the_split_follows_the_partition_table() {
    let s =
        split(&pipeline(r#"[{"$match":{"g":"a"}},{"$sort":{"n":1}},{"$limit":2},{"$count":"k"}]"#));
    assert_eq!(s.shard.len(), 3);
    assert!(matches!(s.merge, Merge::Sorted { limit: Some(2), .. }));
    assert_eq!(s.rest.len(), 2);
    let s = split(&pipeline(r#"[{"$project":{"g":1}}]"#));
    assert_eq!((s.shard.len(), s.merge.clone(), s.rest.len()), (1, Merge::Concat, 0));
    let s = split(&pipeline(r#"[{"$skip":1},{"$match":{}}]"#));
    assert_eq!((s.shard.len(), s.rest.len()), (0, 2));
    let s = split(&pipeline(r#"[{"$group":{"_id":"$g","a":{"$avg":"$n"}}}]"#));
    assert!(matches!(&s.merge, Merge::Group { finish, .. } if finish.len() == 2));
}

#[test]
fn partial_groups_go_to_the_node_of_their_key() {
    let reference = Reference::new(1);
    let docs: Vec<Document> =
        (0..40).map(|i| Document::new().with("_id", i % 10).with("v", i)).collect();
    let nodes = shuffle(docs, 4, &reference);
    assert_eq!(nodes.len(), 4);
    assert_eq!(nodes.iter().map(Vec::len).sum::<usize>(), 40);
    for node in &nodes {
        for d in node {
            let key = d.get("_id");
            assert!(nodes.iter().filter(|n| n.iter().any(|e| e.get("_id") == key)).count() == 1);
        }
    }
    assert_eq!(shuffle(Vec::new(), 0, &reference).len(), 1);
}
