//! `configureFailPoint` on a running node started with `enableTestCommands` (006 T056, FR-034):
//! a closed connection, and a blocked one that holds only its own client.

mod common;

use std::io::Read;
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

fn fail(data: Document) -> Document {
    Document::new()
        .with("configureFailPoint", "failCommand")
        .with("mode", Document::new().with("times", 1))
        .with("data", data)
}

fn names(n: &str) -> Value {
    Value::Array(vec![Value::from(n)])
}

#[test]
fn a_failpoint_closes_or_holds_only_its_connection() {
    let mut cfg = config(Vec::new());
    if let Some(c) = cfg.client.as_mut() {
        c.test_commands = true;
    }
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut admin = Client::new(&node);
    let r = admin.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)));
    assert!(ok(&r), "{r:?}");
    let close = Document::new().with("failCommands", names("find")).with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(close))));
    let mut victim = Client::new(&node);
    victim.send("app", Document::new().with("find", "c"));
    let mut b = [0u8; 1];
    assert!(matches!(victim.s.read(&mut b), Ok(0) | Err(_)));
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 400);
    assert!(ok(&admin.run("admin", fail(block))));
    let mut held = Client::new(&node);
    let start = Instant::now();
    held.send("app", Document::new().with("find", "c"));
    // Another client goes on while that one waits.
    let r = until("a ping", || Some(admin.run("admin", Document::new().with("ping", 1))));
    assert!(ok(&r) && start.elapsed() < Duration::from_millis(400));
    let r = held.receive();
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(batch(&r, "firstBatch").len(), 2);
    // Held, then closed.
    let both = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200)
        .with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(both))));
    let mut closing = Client::new(&node);
    let start = Instant::now();
    closing.send("app", Document::new().with("find", "c"));
    assert!(matches!(closing.s.read(&mut b), Ok(0) | Err(_)));
    assert!(start.elapsed() >= Duration::from_millis(200));
    // A held client that goes away is forgotten at the deadline.
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200);
    assert!(ok(&admin.run("admin", fail(block))));
    let mut gone = Client::new(&node);
    gone.send("app", Document::new().with("find", "c"));
    drop(gone);
    std::thread::sleep(Duration::from_millis(400));
    assert!(ok(&admin.run("admin", Document::new().with("ping", 1))));
    drop(node.stop());
}
