//! A filtered read that goes by the plain scan gives its shard back between two batches of its
//! filter (023 FR-044): on a node of one shard, a `count` whose filter costs each document a
//! long loop is seen by `currentOp` and killed by `killOp` from another connection before it
//! ends. The read that the range's view serves already did; this one held the shard for its
//! whole part, and the kill waited for the end of the read it was to stop.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};

/// What the filter's loop counts to for each document.
const LOOP: u32 = 200_000;

#[test]
fn a_long_filter_by_the_plain_scan_lets_its_shard_serve_a_kill() {
    // One shard: the connection that kills is served by the shard that runs the filter.
    let cfg = NodeConfig { cores: Some(1), ..config(Vec::new()) };
    let node = Node::start(cfg, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..400)))));
    // The read preference keeps the read off the range's view: it goes by the plain scan.
    let filter = Document::new().with(
        "$where",
        format!("function() {{ for (var i = 0; i < {LOOP}; i++) {{}} return true; }}").as_str(),
    );
    let mut slow = Client::new(&node);
    slow.send(
        "db",
        Document::new()
            .with("count", "c")
            .with("query", filter)
            .with("comment", "slow")
            .with("$readPreference", Document::new().with("mode", "primaryPreferred")),
    );
    let running = Document::new().with("currentOp", 1).with("command.comment", "slow");
    let opid = until("the count running", || {
        let r = c.run("admin", running.clone());
        let Some(Value::Array(ops)) = r.get("inprog") else { return None };
        match ops.first() {
            Some(Value::Document(op)) => match op.get("opid") {
                Some(Value::String(id)) => Some(id.clone()),
                _ => None,
            },
            _ => None,
        }
    });
    let killed = c.run("admin", Document::new().with("killOp", 1).with("op", opid.as_str()));
    assert!(ok(&killed), "{killed:?}");
    let r = slow.receive();
    assert_eq!(r.get("codeName"), Some(&Value::from("Interrupted")), "{r:?}");
    node.stop();
}
