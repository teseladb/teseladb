//! The steps of an index build are timed as stages (012 FR-015): the creation of the index in
//! the catalog with its barriers, the read of each chunk of documents, the write of its entries
//! and the change to ready.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, docs, ok};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::Node;
use teseladb_stages::{Stage, count};

const STEPS: [Stage; 4] =
    [Stage::IndexBuildStart, Stage::IndexBuildScan, Stage::IndexBuildWrite, Stage::IndexBuildReady];

#[test]
fn an_index_build_times_each_of_its_steps() {
    let node = Node::start(common::config(Vec::new()), DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let insert = Document::new().with("insert", "c").with("documents", docs(0..300));
    assert!(ok(&c.run("app", insert)));
    let before = STEPS.map(count);
    let index = Document::new().with("key", Document::new().with("v", 1)).with("name", "v_1");
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![index.into()]));
    let r = c.run("app", create);
    assert!(ok(&r), "{r:?}");
    for (stage, before) in STEPS.into_iter().zip(before) {
        assert!(count(stage) > before, "{} was not timed", stage.label());
    }
    // An index that is there already builds nothing.
    let after = STEPS.map(count);
    let again = Document::new().with("key", Document::new().with("v", 1)).with("name", "v_1");
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![again.into()]));
    assert!(ok(&c.run("app", create)));
    assert_eq!(STEPS.map(count), after);
    node.stop();
}
