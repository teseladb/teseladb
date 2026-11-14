//! The `config` database (007 T031, FR-017, contract `config-db.md`): a node shows itself as a
//! shard and a router, the catalog's databases and the cluster's identifier, generated from
//! Group 0; it lists those collections and refuses to write them.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

fn find(c: &mut Client, coll: &str) -> Vec<Value> {
    until(coll, || {
        let r = c.run("config", Document::new().with("find", coll));
        ok(&r).then(|| batch(&r, "firstBatch"))
    })
}

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    match v {
        Value::Document(d) => d.get(name),
        _ => None,
    }
}

#[test]
fn config_shows_the_cluster_from_group0() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)))));
    let shards = find(&mut c, "shards");
    assert_eq!(shards.len(), 1);
    assert_eq!(field(&shards[0], "_id"), Some(&Value::from("node1")));
    assert_eq!(field(&shards[0], "state"), Some(&Value::Int32(1)));
    let dbs = find(&mut c, "databases");
    assert!(dbs.iter().any(|d| field(d, "_id") == Some(&Value::from("app"))), "{dbs:?}");
    let version = find(&mut c, "version");
    assert!(matches!(field(&version[0], "clusterId"), Some(Value::ObjectId(_))), "{version:?}");
    assert_eq!(find(&mut c, teseladb_names::ROUTER).len(), 1);
    assert!(find(&mut c, "chunks").is_empty());
    // A filter applies as on any collection.
    let r = c.run(
        "config",
        Document::new().with("count", "shards").with("query", Document::new().with("_id", "node9")),
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(0)), "{r:?}");
    let listed = c.run("config", Document::new().with("listCollections", 1).with("nameOnly", true));
    let names: Vec<Value> =
        batch(&listed, "firstBatch").iter().filter_map(|d| field(d, "name").cloned()).collect();
    for name in
        ["shards", "databases", "collections", "chunks", teseladb_names::ROUTER, "version", "tags"]
    {
        assert!(names.contains(&Value::from(name)), "{name}: {names:?}");
    }
    // Written: refused, Group 0 is what it shows.
    let insert = Document::new().with("insert", "shards").with("documents", docs(0..1));
    let r = c.run("config", insert);
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    assert_eq!(field(&errors[0], "code"), Some(&Value::Int32(20)), "{r:?}");
    let r = c.run("config", Document::new().with("create", "chunks"));
    assert_eq!(r.get("code"), Some(&Value::Int32(20)), "{r:?}");
    node.stop();
}

#[test]
fn shutdown_asks_the_node_to_stop() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(!node.shutdown_requested());
    let r = c.run("admin", Document::new().with("shutdown", 1));
    assert!(ok(&r), "{r:?}");
    assert!(node.shutdown_requested());
    node.stop();
}

fn code_of(r: &Document) -> Option<&Value> {
    match r.get("writeErrors") {
        Some(Value::Array(errors)) => field(&errors[0], "code"),
        _ => r.get("code"),
    }
}

#[test]
fn config_settings_takes_the_documents_its_validator_lets_through() {
    use teseladb_wire::commands::config_settings::{
        AUTO_MERGE, BALANCER, CHUNK_SIZE, RW_CONCERN_DEFAULTS,
    };
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let insert = |c: &mut Client, d: Document| {
        c.run(
            "config",
            Document::new().with("insert", "settings").with("documents", vec![Value::Document(d)]),
        )
    };
    // A chunk size out of range and an unknown document fail with their errInfo.
    let r = insert(&mut c, Document::new().with("_id", CHUNK_SIZE).with("value", 0));
    assert_eq!(code_of(&r), Some(&Value::Int32(121)), "{r:?}");
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let info = field(&errors[0], "errInfo").expect("errInfo");
    assert_eq!(field(info, "failingDocumentId"), Some(&Value::from(CHUNK_SIZE)), "{info:?}");
    let r = insert(&mut c, Document::new().with("_id", "other").with("value", 1));
    assert_eq!(code_of(&r), Some(&Value::Int32(121)), "{r:?}");
    let r = insert(&mut c, Document::new().with("_id", BALANCER).with("mode", "half"));
    assert_eq!(code_of(&r), Some(&Value::Int32(121)), "{r:?}");
    // Valid ones go in, and a second one with the same `_id` is a duplicate.
    let r = insert(&mut c, Document::new().with("_id", CHUNK_SIZE).with("value", 64));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let r = insert(&mut c, Document::new().with("_id", CHUNK_SIZE).with("value", 32));
    assert_eq!(code_of(&r), Some(&Value::Int32(11000)), "{r:?}");
    let r = insert(&mut c, Document::new().with("_id", AUTO_MERGE).with("enabled", false));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    // An update that breaks the validator fails like a `refrouter`'s.
    let set = Document::new().with("$set", Document::new().with("value", 2000));
    let u = Document::new().with("q", Document::new().with("_id", CHUNK_SIZE)).with("u", set);
    let r = c.run(
        "config",
        Document::new().with("update", "settings").with("updates", vec![Value::Document(u)]),
    );
    assert_eq!(code_of(&r), Some(&Value::Int32(121)), "{r:?}");
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let msg = field(&errors[0], "errmsg");
    assert_eq!(
        msg,
        Some(&Value::from(
            "Plan executor error during update :: caused by :: Document failed validation"
        ))
    );
    // The defaults of `setDefaultRWConcern` are one of its documents.
    let wc = Document::new().with("w", "majority");
    let r = c.run(
        "admin",
        Document::new().with("setDefaultRWConcern", 1).with("defaultWriteConcern", wc),
    );
    assert!(ok(&r), "{r:?}");
    let ids: Vec<Value> = until("settings", || {
        let r = c.run("config", Document::new().with("find", "settings"));
        let ids: Vec<Value> =
            batch(&r, "firstBatch").iter().filter_map(|d| field(d, "_id").cloned()).collect();
        (ids.len() == 3).then_some(ids)
    });
    assert_eq!(ids[0], Value::from(RW_CONCERN_DEFAULTS), "{ids:?}");
    assert!(ids.contains(&Value::from(CHUNK_SIZE)) && ids.contains(&Value::from(AUTO_MERGE)));
    // Removed.
    let d = Document::new().with("q", Document::new().with("_id", AUTO_MERGE)).with("limit", 1);
    let r = c.run(
        "config",
        Document::new().with("delete", "settings").with("deletes", vec![Value::Document(d)]),
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    // `listCollections` shows it with its validator.
    let listed = c.run(
        "config",
        Document::new()
            .with("listCollections", 1)
            .with("filter", Document::new().with("name", "settings")),
    );
    let first = batch(&listed, "firstBatch");
    let options = field(&first[0], "options").expect("options");
    assert!(field(options, "validator").is_some(), "{options:?}");
    node.stop();
}
