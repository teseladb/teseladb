//! The key layout of new collections on a node (003 FR-012, D-153): every node of a fresh
//! cluster records in Group 0 that it reads the `_id` order, so a collection created by an insert
//! or by `create` keeps its documents in `_id` order and gives them so without a sort.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

/// Documents 0 to `n - 1` of [`docs`], in `_id` order.
fn ascending(n: i32) -> Vec<Value> {
    let Value::Array(all) = docs(0..n) else { unreachable!() };
    all
}

fn natural(c: &mut Client, coll: &str) -> Vec<Value> {
    let find = Document::new().with("find", coll).with("filter", Document::new());
    batch(&c.run("app", find.with("batchSize", 1000)), "firstBatch")
}

#[test]
fn new_collections_keep_their_documents_in_id_order() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "made"))));
    let ascending = ascending(300);
    let reversed: Vec<Value> = ascending.iter().rev().cloned().collect();
    for coll in ["implicit", "made"] {
        let insert = Document::new().with("insert", coll).with("documents", reversed.clone());
        let r = c.run("app", insert);
        assert_eq!(r.get("n"), Some(&Value::Int32(300)), "{coll}: {r:?}");
        assert_eq!(natural(&mut c, coll), ascending, "{coll} in `_id` order");
        let one =
            Document::new().with("find", coll).with("filter", Document::new().with("_id", 77));
        assert_eq!(batch(&c.run("app", one), "firstBatch"), vec![ascending[77].clone()]);
        until("the count of the collection", || {
            let stats = c.run("app", Document::new().with("collStats", coll));
            matches!(stats.get("count"), Some(Value::Int32(300) | Value::Int64(300))).then_some(())
        });
    }
    drop(c);
    node.stop();
}
