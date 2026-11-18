//! An index that ends its build leaves the ranges of its collection due for the next pass of the
//! statistics (010 FR-029), so the planner knows the values of its fields without a compaction
//! of each range. A build that read no document marks nothing: an empty collection has no values
//! to learn, and statistics of nothing would stand for the collection once it is written, with
//! the planner picking by them without a trial until its range's next compaction. A node whose
//! routes do not answer marks nothing either, and its build ends well all the same: the mark is
//! a hint, and the statistics come with the range's next compaction.
//! One test, in its own process: the registry of statistics and the queue of ranges due are the
//! process's.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_plan::stats::collection_stats;
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::range_stats::pass;

const DOCS: i32 = 300;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    let r = m.run("p", json(cmd));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
    r
}

/// The names of the indexes of collection `c`.
fn index_names(m: &mut Mem) -> Vec<String> {
    let r = m.run("p", doc(&[("listIndexes", Value::from("c"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => match field(d, "name") {
                Value::String(n) => n.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_built_index_leaves_its_ranges_due_and_builds_without_routes_too() {
    let mut m = Mem::new();
    // One range holds the whole data space.
    m.routes = vec![RouteEntry {
        group: GroupId(1),
        start: vec![teseladb_keys::space::DATA],
        end: Vec::new(),
        core: 0,
        voters: vec![NodeId(1)],
        generation: 1,
        leader_hint: Some(NodeId(1)),
        moved_ms: 0,
        replica_epoch: 1,
    }];
    // An index built over a collection without documents: nothing to learn, no range due.
    run(&mut m, r#"{"create":"c"}"#);
    run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"e":1},"name":"e_1"}]}"#);
    assert_eq!(pass(&mut m).expect("a pass"), 0);

    let docs: Vec<String> =
        (0..DOCS).map(|i| format!(r#"{{"_id":{i},"g":{},"h":{}}}"#, i % 4, i % 5)).collect();
    run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(",")));
    // Nothing compacted, no index built: no range is due.
    assert_eq!(pass(&mut m).expect("a pass"), 0);

    run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"g":1},"name":"g_1"}]}"#);
    // The build left the collection's range due: the next pass reads the new index's values.
    assert_eq!(pass(&mut m).expect("a pass"), 1);
    let key = m.data.keys().find(|k| teseladb_keys::space::in_documents(k)).cloned().unwrap();
    let coll = teseladb_keys::space::collection_of(&key).expect("a collection").to_vec();
    let (_, stats) = collection_stats(&coll).expect("published");
    assert_eq!(stats.documents, DOCS as u64);
    assert_eq!(stats.fields["g"].distinct, 4);

    // The routes do not answer: the build ends well and its index is there.
    m.routes_fail = true;
    run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"h":1},"name":"h_1"}]}"#);
    m.routes_fail = false;
    assert_eq!(index_names(&mut m), ["_id_", "e_1", "g_1", "h_1"]);
    // Nothing was marked then: `h` waits for a compaction of the range.
    assert_eq!(pass(&mut m).expect("a pass"), 0);
    let (_, stats) = collection_stats(&coll).expect("published");
    assert!(!stats.fields.contains_key("h"), "{:?}", stats.fields.keys());
}
