//! What RefDb does with a pipeline when it parses it (010 FR-003, FR-005, T018), measured on a
//! `refrouter` 9.0.4 on 01-11-2026: a constant that fails, in any stage, fails the command with
//! «Failed to optimize pipeline»; and the fields of the documents read that the stages use
//! (the projection `explain` shows under its query plan).

use teseladb_exec::pipeline::deps::needed;
use teseladb_exec::pipeline::optimize::optimize_stages;
use teseladb_bson::{Document, RawDoc, Value, extjson};

fn stages(text: &str) -> Vec<Document> {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("a pipeline");
    match holder.get("p") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn optimized(text: &str, vars: Option<&Document>) -> Result<(), String> {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("a pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("a document");
    let Some(teseladb_bson::RawValue::Array(a)) = raw.get("p") else { panic!("an array") };
    let raws: Vec<_> = a.iter().collect();
    optimize_stages(&raws, vars).map_err(|e| format!("{} {}", e.code.code, e.message))
}

const DIVIDE: &str = "2 Failed to optimize pipeline :: caused by :: can't $divide by zero";

#[test]
fn a_failing_constant_fails_the_pipeline() {
    for p in [
        r#"[{"$project":{"z":{"$divide":[1,0]}}}]"#,
        r#"[{"$limit":1},{"$project":{"z":{"$divide":[1,0]}}}]"#,
        r#"[{"$addFields":{"z":{"$divide":[1,0]}}}]"#,
        r#"[{"$set":{"o":{"z":{"$divide":[1,0]}}}}]"#,
        r#"[{"$group":{"_id":"$g","x":{"$sum":{"$divide":[1,0]}}}}]"#,
        r#"[{"$group":{"_id":{"$divide":[1,0]}}}]"#,
        r#"[{"$group":{"_id":null,"t":{"$topN":{"n":{"$divide":[1,0]},"output":"$s","sortBy":{"s":1}}}}}]"#,
        r#"[{"$bucket":{"groupBy":{"$divide":[1,0]},"boundaries":[0,1]}}]"#,
        r#"[{"$bucketAuto":{"groupBy":"$n","buckets":2,"output":{"x":{"$max":{"$divide":[1,0]}}}}}]"#,
        r#"[{"$replaceRoot":{"newRoot":{"$divide":[1,0]}}}]"#,
        r#"[{"$replaceWith":{"$divide":[1,0]}}]"#,
        r#"[{"$sortByCount":{"$divide":[1,0]}}]"#,
        r#"[{"$facet":{"a":[{"$project":{"z":{"$divide":[1,0]}}}]}}]"#,
        r#"[{"$project":{"o":{"z":{"$divide":[1,0]}}}}]"#,
    ] {
        assert_eq!(optimized(p, None), Err(DIVIDE.to_owned()), "{p}");
    }
    assert_eq!(
        optimized(r#"[{"$match":{"$expr":{"$divide":[1,0]}}}]"#, None),
        Err("2 Failed to optimize pipeline :: caused by :: Failed to optimize expression :: caused by :: can't $divide by zero".to_owned())
    );
}

#[test]
fn what_reads_documents_or_is_no_constant_passes() {
    let vars = Document::new().with("v", 2);
    for p in [
        r#"[{"$project":{"z":{"$divide":["$n",0]},"a":1,"b":false,"o":{"x":1}}}]"#,
        r#"[{"$redact":{"$cond":[true,"$$KEEP","$$PRUNE"]}}]"#,
        r#"[{"$group":{"_id":null,"t":{"$top":{"output":"$s","sortBy":{"s":1}}}}}]"#,
        r#"[{"$facet":{"a":[{"$limit":1}],"b":5}},{"$sort":{"a":1}}]"#,
        r#"[{"$replaceRoot":{"other":1}},{"$group":{"_id":null,"c":1}}]"#,
        r#"[{"$project":{"z":{"$divide":[1,"$$v"]}}}]"#,
        r#"[{"$bucket":{"boundaries":[0,1],"output":5}}]"#,
        r#"[{"$facet":{"a":[5]}}]"#,
    ] {
        assert_eq!(optimized(p, Some(&vars)), Ok(()), "{p}");
    }
}

fn fields(text: &str) -> Option<Vec<String>> {
    needed(&stages(text))
}

fn list(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|s| (*s).to_owned()).collect())
}

/// The projections measured under `explain`'s query plans (`transformBy`).
#[test]
fn the_fields_a_pipeline_uses_are_those_it_reads() {
    assert_eq!(fields(r#"[{"$group":{"_id":"$g","x":{"$sum":"$n"}}}]"#), list(&["g", "n"]));
    assert_eq!(fields(r#"[{"$unwind":"$t"},{"$group":{"_id":"$t"}}]"#), list(&["t"]));
    assert_eq!(fields(r#"[{"$unwind":{"path":"$t"}},{"$group":{"_id":"$t"}}]"#), list(&["t"]));
    assert_eq!(
        fields(
            r#"[{"$lookup":{"from":"o","localField":"n","foreignField":"k","as":"j"}},{"$group":{"_id":"$g"}}]"#
        ),
        list(&["g", "n"])
    );
    assert_eq!(fields(r#"[{"$set":{"z":"$n"}},{"$group":{"_id":"$z"}}]"#), list(&["n"]));
    assert_eq!(fields(r#"[{"$replaceRoot":{"newRoot":"$o"}}]"#), list(&["o"]));
    assert_eq!(fields(r#"[{"$replaceWith":"$o"}]"#), list(&["o"]));
    assert_eq!(
        fields(
            r#"[{"$fill":{"sortBy":{"d":1},"partitionBy":"$g","output":{"x":{"value":0}}}},{"$count":"c"}]"#
        ),
        list(&["d", "g", "x"])
    );
    // Measured on 02-11-2026: the partition, the order and the window function's argument.
    assert_eq!(
        fields(
            r#"[{"$setWindowFields":{"partitionBy":"$g","sortBy":{"s":1},"output":{"t":{"$sum":"$n","window":{"documents":["unbounded","current"]}}}}},{"$group":{"_id":"$t"}}]"#
        ),
        list(&["g", "n", "s"])
    );
    assert_eq!(
        fields(
            r#"[{"$fill":{"sortBy":{"d":1},"output":{"x":{"method":"linear"}}}},{"$count":"c"}]"#
        ),
        list(&["d", "x"])
    );
    assert_eq!(fields(r#"[{"$addFields":{"z":1}},{"$group":{"_id":"$z"}}]"#), list(&[]));
    assert_eq!(fields(r#"[{"$project":{"g":0}},{"$group":{"_id":"$s"}}]"#), list(&["s"]));
    assert_eq!(fields(r#"[{"$unset":"g"},{"$sortByCount":"$s"}]"#), list(&["s"]));
    assert_eq!(fields(r#"[{"$group":{"_id":"$o.k","o":{"$first":"$o"}}}]"#), list(&["o"]));
    assert_eq!(
        fields(r#"[{"$project":{"s":1,"o":{"k":1},"z":"$n"}}]"#),
        list(&["_id", "n", "o.k", "s"])
    );
    assert_eq!(fields(r#"[{"$project":{"_id":0,"s":1}}]"#), list(&["s"]));
    // A projection of `_id` alone keeps it (measured: `PROJECTION_COVERED` over `_id_`).
    assert_eq!(fields(r#"[{"$project":{"_id":1}}]"#), list(&["_id"]));
    assert_eq!(fields(r#"[{"$project":{"_id":0}},{"$group":{"_id":"$s"}}]"#), list(&["s"]));
    assert_eq!(
        fields(
            r#"[{"$match":{"g":"a","$or":[{"n":1},{"$expr":{"$eq":["$$ROOT.x",1]}}],"$comment":"c"}},{"$sort":{"s":1}},{"$skip":1},{"$limit":2},{"$bucket":{"groupBy":"$d","boundaries":[0,1]}}]"#
        ),
        list(&["d", "g", "n", "s", "x"])
    );
    assert_eq!(
        fields(r#"[{"$bucketAuto":{"groupBy":"$n","buckets":2,"output":{"m":{"$max":"$x"}}}}]"#),
        list(&["n", "x"])
    );
    assert_eq!(
        fields(r#"[{"$group":{"_id":{"$literal":"$x"},"r":{"$sum":{"$rand":{}}}}}]"#),
        list(&[])
    );
}

#[test]
fn a_pipeline_that_passes_whole_documents_uses_every_field() {
    for p in [
        r#"[]"#,
        r#"[{"$match":{"g":"a"}}]"#,
        r#"[{"$group":{"_id":"$$ROOT"}}]"#,
        r#"[{"$group":{"_id":"$$CURRENT"}}]"#,
        r#"[{"$group":{"_id":null,"m":{"$meta":"textScore"}}}]"#,
        r#"[{"$match":{"$where":"1"}},{"$count":"c"}]"#,
        r#"[{"$match":{"$or":[5]}},{"$count":"c"}]"#,
        r#"[{"$sample":{"size":1}},{"$count":"c"}]"#,
        r#"[{"$unwind":{"path":5}},{"$count":"c"}]"#,
        r#"[{"$unwind":5},{"$count":"c"}]"#,
        r#"[{"$lookup":{"from":"o","as":"j","pipeline":[]}},{"$count":"c"}]"#,
        r#"[{"$lookup":{"from":"o","localField":"n"}},{"$count":"c"}]"#,
        r#"[{}]"#,
    ] {
        assert_eq!(fields(p), None, "{p}");
    }
}

#[test]
fn fields_of_the_edges_of_stages() {
    assert_eq!(fields(r#"[{"$fill":{"sortBy":{"d":1}}},{"$count":"c"}]"#), list(&["d"]));
    assert_eq!(
        fields(r#"[{"$project":{"a":0.0}},{"$project":{"_id":0.0,"b":1.0}}]"#),
        list(&["b"])
    );
    // A computed field inside `o` needs the whole of `o` (measured on 9.0.4).
    assert_eq!(fields(r#"[{"$project":{"o":{"k":"$p"}}}]"#), list(&["_id", "o", "p"]));
    assert_eq!(fields(r#"[{"$project":{"o.q.k":"$p"}}]"#), list(&["_id", "o", "p"]));
    assert_eq!(
        fields(
            r#"[{"$set":{"o":{"k":"$p"}}},{"$group":{"_id":"$o.k","q":{"$first":"$$ROOT.q"},"r":{"$first":"$$v"}}}]"#
        ),
        list(&["p", "q"])
    );
    assert_eq!(optimized(r#"[5,{},{"$group":{"_id":null,"x":{}}}]"#, None), Ok(()));
}

#[test]
fn fields_of_facets_projections_windows_and_named_arguments() {
    assert_eq!(
        fields(r#"[{"$facet":{"a":[{"$group":{"_id":"$x"}}],"b":[{"$count":"n"}]}}]"#),
        list(&["x"])
    );
    assert_eq!(fields(r#"[{"$facet":{"a":5}}]"#), None);
    assert_eq!(fields(r#"[{"$facet":{"a":[5]}}]"#), None);
    assert_eq!(fields(r#"[{"$facet":{"a":[{"$match":{}}]}}]"#), None);
    assert_eq!(
        fields(
            r#"[{"$project":{"_id":0,"a":{"b":1,"c":false},"d":{"$add":["$e",1]},"f.g":"$h"}}]"#
        ),
        list(&["a.b", "e", "f", "h"])
    );
    assert_eq!(fields(r#"[{"$project":{"_id":1}},{"$count":"n"}]"#), list(&["_id"]));
    assert_eq!(fields(r#"[{"$project":{"_id":0.0,"a":1.5}}]"#), list(&["a"]));
    assert_eq!(fields(r#"[{"$project":{"_id":false,"a":{"$literal":1}}}]"#), Some(Vec::new()));
    assert_eq!(fields(r#"[{"$project":{"a":{"$meta":"textScore"}}}]"#), None);
    assert_eq!(fields(r#"[{"$project":{"a":{"$rand":{}}}}]"#), list(&["_id"]));
    // A `distinct` on a view reads the values at its key's path, which the expression names as
    // a constant (measured on a `refrouter` 9.0.4: the plan cache shape keeps `s`).
    assert_eq!(
        fields(
            r#"[{"$replaceRoot":{"newRoot":{"u":{"$_internalFindAllValuesAtPath":[{"$const":"s"}]}}}},{"$unwind":"$u"},{"$group":{"_id":"$u"}}]"#
        ),
        list(&["s"])
    );
    assert_eq!(
        fields(r#"[{"$group":{"_id":null,"t":{"$topN":{"n":2,"output":"$s","sortBy":{"k":1}}}}}]"#),
        list(&["k", "s"])
    );
    assert_eq!(
        fields(
            r#"[{"$setWindowFields":{"partitionBy":"$p","sortBy":{"o":1},"output":{"r":{"$sum":"$v","window":{"documents":[-1,0]}},"q":5}}},{"$group":{"_id":"$r","x":{"$first":"$w"}}}]"#
        ),
        list(&["o", "p", "v", "w"])
    );
    assert_eq!(
        fields(
            r#"[{"$addFields":{"n":{"m":"$a","k":{"$add":["$b",1]}}}},{"$group":{"_id":"$n.m"}}]"#
        ),
        list(&["a", "b"])
    );
    assert_eq!(fields(r#"[{"$unwind":{"preserve":true}},{"$count":"c"}]"#), None);
    assert_eq!(fields(r#"[{"$unwind":5}]"#), None);
    assert_eq!(fields(r#"[{"$lookup":{"from":"o","localField":1,"as":"j"}}]"#), None);
}
