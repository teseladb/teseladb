//! An index that ends its build leaves the ranges of its collection due for statistics (010
//! FR-029): the next pass reads the values of its fields, and the planner knows them without
//! waiting for a compaction of each range. Seen here by an aggregation that can split over the
//! ranges: once the statistics say its new index would bring a quarter of the collection, it
//! reads through the ranges (FR-034).
#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;
use teseladb_io::engine::disk::DiskPlace;

use common::{Client, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_server::client_listener::Hooks;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_stages::Stage;

const DB: &str = "built";
const DOCS: i32 = 2000;
/// The values of `g`: a quarter of the documents each.
const GROUPS: i32 = 4;

fn run(c: &mut Client, cmd: Document) -> Document {
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    r
}

/// How many documents hold `g`, counted by an aggregation that can split.
fn count_of(c: &mut Client, g: i32) -> i32 {
    let matches = Document::new().with("$match", Document::new().with("g", g));
    let group = Document::new().with(
        "$group",
        Document::new().with("_id", Value::Null).with("n", Document::new().with("$sum", 1)),
    );
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", vec![Value::Document(matches), Value::Document(group)])
        .with("cursor", Document::new());
    let r = run(c, cmd);
    match batch(&r, "firstBatch").first() {
        Some(Value::Document(d)) => match d.get("n") {
            Some(Value::Int32(n)) => *n,
            other => panic!("{other:?}"),
        },
        Some(other) => panic!("{other:?}"),
        None => 0,
    }
}

#[test]
fn the_values_of_an_index_just_built_reach_the_planner_without_a_compaction() {
    let cfg = NodeConfig {
        client_hooks: Hooks { stats_every: Duration::from_millis(50), ..Hooks::default() },
        ..config(Vec::new())
    };
    let n = Node::start(cfg, DiskPlace::Memory, None).expect("starts");
    assert!(n.wait_ready(common::READY));
    let mut c = Client::new(&n);
    run(&mut c, Document::new().with("insert", "c").with("documents", docs(0..DOCS)));
    let group = Document::new().with("$mod", vec![Value::from("$_id"), Value::from(GROUPS)]);
    let set = Document::new().with("$set", Document::new().with("g", group));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", vec![Value::Document(set)])
        .with("multi", true);
    run(&mut c, Document::new().with("update", "c").with("updates", vec![all.into()]));
    // The index is built over the documents; nothing is compacted.
    let index = Document::new().with("key", Document::new().with("g", 1)).with("name", "g_1");
    run(&mut c, Document::new().with("createIndexes", "c").with("indexes", vec![index.into()]));
    until("the read through the ranges", || {
        let (scattered, by_index) = (
            teseladb_stages::count(Stage::AggScattered),
            teseladb_stages::count(Stage::AggByIndex),
        );
        assert_eq!(count_of(&mut c, 1), DOCS / GROUPS);
        (teseladb_stages::count(Stage::AggScattered) > scattered
            && teseladb_stages::count(Stage::AggByIndex) == by_index)
            .then_some(())
    });
    n.stop();
}
