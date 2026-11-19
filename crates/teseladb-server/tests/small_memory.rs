//! A node whose shard has little memory keeps serving when what it is asked does not fit in it
//! (023 FR-030). A shard's memory is one fixed pool: a request the pool could not serve failed,
//! and the failed allocation ended the process, however much memory the machine had. The pool
//! of a small node filled, or was left without a run of free pages as long as one buffer, within
//! seconds of a light load. The node's engine now takes from the system's allocator what the
//! pool cannot serve, and counts it.
#![cfg(target_os = "linux")]

mod common;

use std::net::SocketAddr;

use common::{Client, READY, config, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::admin_http::{Method, request};
use teseladb_server::node::{Node, NodeConfig};

/// The memory of the node's only shard.
const MEMORY: u64 = 64 << 20;
/// Documents of each insert, and their text: about 200 KiB a batch.
const BATCH: i32 = 100;
const TEXT: usize = 2_000;
/// Batches: about 80 MiB of documents, more than the shard's memory, kept by a disk in memory.
const BATCHES: i32 = 400;

fn sample(text: &str, name: &str) -> Option<f64> {
    text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

#[test]
fn a_node_takes_from_the_system_what_its_shard_s_memory_cannot_serve() {
    let cfg = NodeConfig {
        cores: Some(1),
        memory_limit: Some(MEMORY),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        ..config(Vec::new())
    };
    let node = Node::start(cfg, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let text = "x".repeat(TEXT);
    for b in 0..BATCHES {
        let docs = (0..BATCH)
            .map(|i| {
                Value::Document(Document::new().with("_id", b * BATCH + i).with("t", text.as_str()))
            })
            .collect();
        let insert = Document::new().with("insert", "c").with("documents", Value::Array(docs));
        let r = c.run("db", insert);
        assert!(ok(&r), "batch {b}: {r:?}");
    }
    // Every document is there, and a read of the whole collection goes through the same memory.
    let n = c.run("db", Document::new().with("count", "c"));
    assert_eq!(n.get("n"), Some(&Value::Int32(BATCHES * BATCH)), "{n:?}");
    // The shard's memory could not hold all of it: the system's allocator served the rest.
    let at = node.admin_addr().unwrap().to_string();
    let served = until("the allocations served by the system", || {
        let (status, body) = request(&at, Method::Get, "/metrics", b"").unwrap();
        let text = String::from_utf8(body).unwrap();
        let limit = sample(&text, "teseladb_memory_limit_bytes")?;
        assert_eq!(limit, MEMORY as f64, "the shard's memory");
        (status == 200)
            .then(|| sample(&text, "teseladb_engine_memory_system_allocations{shard=\"0\"}"))
            .flatten()
    });
    assert!(served > 0.0, "the shard's own memory served everything");
    node.stop();
}
