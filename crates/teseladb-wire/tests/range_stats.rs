//! The statistics of a range a compaction rewrote (010 FR-029): the pass reads the entries of
//! the collection's indexes inside the range and publishes its documents and, for each indexed
//! field, its values; the planner then picks by its estimate, and runs a trial only between
//! plans whose estimates are within twice each other. One test, in its own process: the
//! registry of statistics and the queue of compacted ranges are the process's.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_plan::stats::{collection_stats, mark_compacted};
use teseladb_wire::commands::range_stats::pass;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    let r = m.run("p", json(cmd));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
    r
}

const DOCS: i32 = 3000;

/// The rejected plans and the winning plan of `filter`.
fn planned(m: &mut Mem, filter: &str) -> (usize, String) {
    let find = doc(&[("find", Value::from("c")), ("filter", Value::Document(json(filter)))]);
    let cmd =
        doc(&[("explain", Value::Document(find)), ("verbosity", Value::from("queryPlanner"))]);
    let r = m.run("p", cmd);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{filter}: {r:?}");
    // A node shows its plan as its shard's.
    let Value::Array(shards) = field(&r, "queryPlanner.winningPlan.shards") else {
        panic!("{r:?}")
    };
    let Some(Value::Document(shard)) = shards.first() else { panic!("{r:?}") };
    let rejected = match field(shard, "rejectedPlans") {
        Value::Array(a) => a.len(),
        other => panic!("{other:?}"),
    };
    (rejected, format!("{:?}", field(shard, "winningPlan")))
}

fn ids(r: &Document) -> Vec<i32> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "_id") {
                Value::Int32(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_compacted_range_gives_the_planner_its_statistics_and_far_apart_plans_skip_the_trial() {
    let mut m = Mem::new();
    for spec in [
        r#"{"key":{"a":1},"name":"a_1"}"#,
        r#"{"key":{"b":1},"name":"b_1"}"#,
        r#"{"key":{"c":1,"d":-1},"name":"c_1_d_-1"}"#,
        r#"{"key":{"s":1},"name":"s_1","collation":{"locale":"fr"}}"#,
    ] {
        run(&mut m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    }
    // `a` is unique, `b` takes two values, `c` and `d` share a compound index, `s` is only in
    // a collated index.
    let docs: Vec<String> = (0..DOCS)
        .map(|i| {
            format!(
                r#"{{"_id":{i},"a":{i},"b":{},"c":{},"d":{},"s":"x{}"}}"#,
                i % 2,
                i % 3,
                i % 7,
                i % 5
            )
        })
        .collect();
    for chunk in docs.chunks(1000) {
        run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
    }
    // Without statistics the two indexes run a trial.
    let filter = r#"{"a":7,"b":1}"#;
    let (rejected, _) = planned(&mut m, filter);
    assert_eq!(rejected, 1, "a trial between the two indexes");
    // Nothing compacted, nothing published.
    assert_eq!(pass(&mut m).expect("a pass"), 0);
    // A compaction rewrote the keys of the range that holds the whole data space.
    mark_compacted(1, &[teseladb_keys::space::DATA], &[]);
    assert_eq!(pass(&mut m).expect("a pass"), 1);
    let key = m.data.keys().find(|k| teseladb_keys::space::in_documents(k)).cloned().unwrap();
    let coll = teseladb_keys::space::collection_of(&key).expect("a collection").to_vec();
    let (_, s) = collection_stats(&coll).expect("published");
    assert_eq!(s.documents, DOCS as u64);
    assert_eq!(s.fields["a"].count, DOCS as u64);
    assert_eq!(s.fields["a"].distinct, DOCS as u64);
    assert_eq!(s.fields["b"].distinct, 2);
    assert_eq!(s.fields["c"].distinct, 3);
    assert_eq!(s.fields["d"].distinct, 7);
    // The keys of a collated index are sort keys, not values; the index of `_id` is not read.
    assert!(!s.fields.contains_key("s"), "{:?}", s.fields.keys());
    assert!(!s.fields.contains_key("_id"), "{:?}", s.fields.keys());
    // One document of `a` against half of them of `b`: the estimate picks `a_1` alone.
    let (rejected, plan) = planned(&mut m, filter);
    assert_eq!(rejected, 0, "{plan}");
    assert!(plan.contains("\"a_1\"") && !plan.contains("\"b_1\""), "{plan}");
    let r = run(&mut m, &format!(r#"{{"find":"c","filter":{filter}}}"#));
    assert_eq!(ids(&r), vec![7]);
    // Two estimates within twice each other still run a trial: `a` below 1500 and `b` equal
    // to 1 keep half of the documents each.
    let (rejected, _) = planned(&mut m, r#"{"a":{"$lt":1500},"b":1}"#);
    assert_eq!(rejected, 1, "a trial between close estimates");
    // The range compacted again after more writes replaces its statistics.
    let more: Vec<String> =
        (DOCS..DOCS + 500).map(|i| format!(r#"{{"_id":{i},"a":{i},"b":2}}"#)).collect();
    run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, more.join(",")));
    mark_compacted(1, &[teseladb_keys::space::DATA], &[]);
    assert_eq!(pass(&mut m).expect("a pass"), 1);
    let (_, s) = collection_stats(&coll).expect("published");
    assert_eq!(s.documents, DOCS as u64 + 500);
    assert_eq!(s.fields["b"].distinct, 3);
}
