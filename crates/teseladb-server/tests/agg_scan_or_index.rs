//! An aggregation that can split over the ranges (010 FR-034) reads its collection through the
//! ranges, and not by an index, when the collection's statistics say the index would bring a
//! large part of it document by document; a value few documents hold still goes by the index.
#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;
use teseladb_io::engine::disk::DiskPlace;

use common::{Client, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_server::client_listener::Hooks;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_stages::Stage;

const DB: &str = "scan_or_index";
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
fn an_aggregation_scans_through_its_ranges_when_its_index_would_fetch_a_quarter() {
    let cfg = NodeConfig {
        client_hooks: Hooks { stats_every: Duration::from_millis(50), ..Hooks::default() },
        ..config(Vec::new())
    };
    let n = Node::start(cfg, DiskPlace::Memory, None).expect("starts");
    assert!(n.wait_ready(common::READY));
    let mut c = Client::new(&n);
    let index = Document::new().with("key", Document::new().with("g", 1)).with("name", "g_1");
    run(&mut c, Document::new().with("createIndexes", "c").with("indexes", vec![index.into()]));
    run(&mut c, Document::new().with("insert", "c").with("documents", docs(0..DOCS)));
    let group = Document::new().with("$mod", vec![Value::from("$_id"), Value::from(GROUPS)]);
    let set = Document::new().with("$set", Document::new().with("g", group));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", vec![Value::Document(set)])
        .with("multi", true);
    run(&mut c, Document::new().with("update", "c").with("updates", vec![all.into()]));

    // Without statistics the index plan runs, as before.
    let by_index = teseladb_stages::count(Stage::AggByIndex);
    assert_eq!(count_of(&mut c, 1), DOCS / GROUPS);
    assert!(teseladb_stages::count(Stage::AggByIndex) > by_index);

    // The compaction rewrites the range's files and its statistics follow: a quarter of the
    // collection goes through the ranges.
    run(&mut c, Document::new().with("compact", "c").with("force", true));
    until("the scan through the ranges", || {
        let (scattered, by_index) = (
            teseladb_stages::count(Stage::AggScattered),
            teseladb_stages::count(Stage::AggByIndex),
        );
        assert_eq!(count_of(&mut c, 1), DOCS / GROUPS);
        (teseladb_stages::count(Stage::AggScattered) > scattered
            && teseladb_stages::count(Stage::AggByIndex) == by_index)
            .then_some(())
    });

    // A value no document holds still asks the index, which answers at once.
    let by_index = teseladb_stages::count(Stage::AggByIndex);
    assert_eq!(count_of(&mut c, GROUPS + 3), 0);
    assert!(teseladb_stages::count(Stage::AggByIndex) > by_index);
    n.stop();
}
