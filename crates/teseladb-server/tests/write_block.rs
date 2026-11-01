//! 007 T042 (FR-029): the `fsync` lock and the blocked user writes hold for every connection of
//! the node, from the moment the command answers. 007 T040 (FR-027): a node keeps the cluster's
//! feature compatibility version in Group 0 and shows it with `getParameter`.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

fn write_error_code(r: &Document) -> Option<Value> {
    let Some(Value::Array(errors)) = r.get("writeErrors") else { return None };
    match errors.first() {
        Some(Value::Document(d)) => d.get("code").cloned(),
        _ => None,
    }
}

#[test]
fn a_lock_holds_the_other_connections_writes_and_a_block_refuses_them() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut admin = Client::new(&node);
    let mut app = Client::new(&node);
    let insert = |n| Document::new().with("insert", "c").with("documents", docs(n..n + 1));
    assert!(ok(&app.run("app", insert(0))));
    let r = admin.run("admin", Document::new().with("fsync", 1).with("lock", true));
    assert!(ok(&r), "{r:?}");
    let r = app.run("app", insert(1).with("maxTimeMS", 200));
    assert_eq!(write_error_code(&r), Some(Value::Int32(50)), "{r:?}");
    let r = app.run("app", Document::new().with("find", "c"));
    assert_eq!(batch(&r, "firstBatch").len(), 1, "reads go on: {r:?}");
    let r = admin.run("admin", Document::new().with("fsyncUnlock", 1));
    assert!(ok(&r), "{r:?}");
    assert!(ok(&app.run("app", insert(2))));
    let block = |on: bool| Document::new().with("setUserWriteBlockMode", 1).with("global", on);
    assert!(ok(&admin.run("admin", block(true))));
    let r = app.run("app", insert(3));
    assert_eq!(write_error_code(&r), Some(Value::Int32(371)), "{r:?}");
    assert!(ok(&admin.run("admin", block(false))));
    until("a write once unblocked", || ok(&app.run("app", insert(4))).then_some(()));
    node.stop();
}

fn version(c: &mut Client) -> Option<Value> {
    let r = c.run(
        "admin",
        Document::new().with("getParameter", 1).with("featureCompatibilityVersion", 1),
    );
    match r.get("featureCompatibilityVersion") {
        Some(Value::Document(d)) => d.get("version").cloned(),
        _ => None,
    }
}

#[test]
fn the_version_lives_in_group0() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert_eq!(version(&mut c), Some(Value::from("9.0")));
    let r = c.run(
        "admin",
        Document::new().with("setFeatureCompatibilityVersion", "8.0").with("confirm", true),
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(version(&mut Client::new(&node)), Some(Value::from("8.0")));
    node.stop();
}
