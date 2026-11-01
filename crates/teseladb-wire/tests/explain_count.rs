//! `explain` of `count` with each access path (010, measured shapes of a `refrouter` 9.0.4): an
//! `_id` equality reads one document (`IDHACK`), an index that decides the whole filter counts
//! its keys (`IXSCAN` under `COUNT`) and an `$or` over two such indexes runs under `SUBPLAN`;
//! `queryPlanner` plans them without running, so every counter is 0.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn int(i: i32) -> Value {
    Value::Int32(i)
}

fn collection() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<Value> = (0..4)
        .map(|i| Value::Document(doc(&[("_id", int(i)), ("a", int(i)), ("b", int(i))])))
        .collect();
    m.run("db", doc(&[("insert", Value::from("c")), ("documents", arr(docs))]));
    for k in ["a", "b"] {
        let spec = doc(&[
            ("key", Value::Document(doc(&[(k, int(1))]))),
            ("name", Value::from(format!("{k}_1"))),
        ]);
        let create = doc(&[
            ("createIndexes", Value::from("c")),
            ("indexes", arr(vec![Value::Document(spec)])),
        ]);
        m.run("db", create);
    }
    m
}

/// The winning plan of the only shard.
fn plan(r: &Document) -> Document {
    let Value::Array(shards) = field(r, "queryPlanner.winningPlan.shards") else { panic!("{r:?}") };
    let Some(Value::Document(shard)) = shards.first() else { panic!("{r:?}") };
    match shard.get("winningPlan") {
        Some(Value::Document(p)) => p.clone(),
        _ => panic!("{r:?}"),
    }
}

fn input(stage: &Document) -> Document {
    match stage.get("inputStage") {
        Some(Value::Document(d)) => d.clone(),
        other => panic!("{other:?} in {stage:?}"),
    }
}

fn explain(m: &mut Mem, query: Document) -> Document {
    let count = doc(&[("count", Value::from("c")), ("query", Value::Document(query))]);
    m.run(
        "db",
        doc(&[("explain", Value::Document(count)), ("verbosity", Value::from("queryPlanner"))]),
    )
}

#[test]
fn count_plans_each_access_without_running_it() {
    let mut m = collection();
    let by_id = plan(&explain(&mut m, doc(&[("_id", int(2))])));
    assert_eq!(by_id.get("stage"), Some(&Value::from("COUNT")));
    assert_eq!(input(&by_id).get("stage"), Some(&Value::from("IDHACK")));
    // Two points of `a`: the index decides the filter alone.
    let points = doc(&[("a", Value::Document(doc(&[("$in", arr(vec![int(1), int(3)]))])))]);
    let ix = input(&plan(&explain(&mut m, points)));
    assert_eq!(ix.get("stage"), Some(&Value::from("IXSCAN")), "{ix:?}");
    assert_eq!(ix.get("indexName"), Some(&Value::from("a_1")));
    // `$or` of `a` and `b`, each with its index.
    let or = doc(&[(
        "$or",
        arr(vec![Value::Document(doc(&[("a", int(1))])), Value::Document(doc(&[("b", int(2))]))]),
    )]);
    let sub = input(&plan(&explain(&mut m, or)));
    assert_eq!(sub.get("stage"), Some(&Value::from("SUBPLAN")), "{sub:?}");
}
