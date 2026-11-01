//! 007 T042 (FR-029): the `fsync` lock and the blocked user writes hold for every connection of
//! every node, from the moment the command answers. 007 T040 (FR-027): a node keeps the cluster's
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

/// The node configuration of a test that starts several nodes.
fn joining(join: Vec<String>) -> teseladb_server::node::NodeConfig {
    let admin = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    teseladb_server::node::NodeConfig { admin_listen: Some(admin), ..config(join) }
}

#[test]
fn the_gates_hold_on_every_node_once_the_command_answers() {
    let n1 = Node::start(joining(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n1.wait_ready(READY));
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = Node::start(joining(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    // The third node turns slowly: it publishes what Group 0 applied up to 300 ms late.
    let slow = teseladb_server::node::NodeConfig {
        tick: std::time::Duration::from_millis(300),
        ..joining(seed)
    };
    let n3 = Node::start(slow, || Ok(MemDisk::new()), None).unwrap();
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let mut admin = Client::new(&n1);
    let mut others = [Client::new(&n2), Client::new(&n3)];
    let insert = |n: i32| {
        Document::new().with("insert", "c").with("documents", docs(n..n + 1)).with("maxTimeMS", 100)
    };
    until("the collection on every node", || {
        others
            .iter_mut()
            .all(|c| ok(&c.run("app", insert(-1).with("ordered", false))))
            .then_some(())
    });
    let block = |on: bool| Document::new().with("setUserWriteBlockMode", 1).with("global", on);
    // The other nodes see each change the moment the command answers, also right after it.
    for round in 0..10 {
        let r = admin.run("admin", Document::new().with("fsync", 1).with("lock", true));
        assert!(ok(&r), "{r:?}");
        for c in &mut others {
            let r = c.run("app", insert(round * 10));
            assert_eq!(write_error_code(&r), Some(Value::Int32(50)), "round {round}: {r:?}");
        }
        assert!(ok(&admin.run("admin", Document::new().with("fsyncUnlock", 1))));
        assert!(ok(&admin.run("admin", block(true))));
        for c in &mut others {
            let r = c.run("app", insert(round * 10 + 1));
            assert_eq!(write_error_code(&r), Some(Value::Int32(371)), "round {round}: {r:?}");
        }
        assert!(ok(&admin.run("admin", block(false))));
        for (i, c) in others.iter_mut().enumerate() {
            let r = c.run("app", insert(round * 10 + 2 + i as i32));
            assert!(ok(&r) && write_error_code(&r).is_none(), "round {round}: {r:?}");
        }
    }
    for n in [n1, n2, n3] {
        n.stop();
    }
}

/// Sets the cluster's feature compatibility version to `v`.
fn set_fcv(c: &mut Client, v: &str) -> Document {
    c.run("admin", Document::new().with("setFeatureCompatibilityVersion", v).with("confirm", true))
}

#[test]
fn the_version_lives_in_group0() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&set_fcv(&mut c, "8.0")));
    // From 8.0 only to 9.0: another connection sees the version Group 0 keeps.
    let r = set_fcv(&mut Client::new(&node), "8.3");
    assert_eq!(r.get("code"), Some(&Value::Int32(5_147_403)), "{r:?}");
    assert!(ok(&set_fcv(&mut c, "9.0")));
    node.stop();
}
