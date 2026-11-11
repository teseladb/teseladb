//! Every key keeps answering through splits by load (007 FR-040, 002 FR-032): eight clients
//! insert, increment and read their own documents in two collections, each with a secondary index,
//! while the load splits their range and moves its new halves to other cores; afterwards each
//! document they wrote reads back at once with its last confirmed value. The sustained load of 002
//! found keys that no read or write could reach any more after such a split (11-11-2026). The cut
//! falls between the two collections: it does not cut through the writes of a document and of its
//! index entry (007 FR-006).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{Client, READY, batch, docs, ok, until};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

const WRITERS: i32 = 8;
const LOAD: Duration = Duration::from_secs(20);
/// What an operation of the load may take: far below the requests' deadline of 30 s.
const SLOW: Duration = Duration::from_secs(5);
/// Operations per second of a range that split it (the default is 2500).
const SPLIT_OPS: i32 = 200;

fn config() -> NodeConfig {
    NodeConfig { cores: Some(4), ..common::config(Vec::new()) }
}

fn id(writer: i32, n: usize) -> String {
    format!("w{writer}-{n:06}")
}

/// The collection of writer `w`: half the writers write to each.
fn coll(w: i32) -> &'static str {
    if w % 2 == 0 { "c" } else { "d" }
}

fn update(coll: &str, _id: &str) -> Document {
    let inc = Document::new().with("$inc", Document::new().with("v", 1));
    let one = Document::new().with("q", Document::new().with("_id", _id)).with("u", inc);
    Document::new().with("update", coll).with("updates", Value::Array(vec![one.into()]))
}

fn find(coll: &str, _id: &str) -> Document {
    Document::new()
        .with("find", coll)
        .with("filter", Document::new().with("_id", _id))
        .with("maxTimeMS", i32::try_from(SLOW.as_millis()).unwrap())
}

/// The value of a document as a find answered it.
fn value_of(r: &Document) -> Option<Value> {
    let got = if ok(r) { batch(r, "firstBatch") } else { Vec::new() };
    got.first().and_then(|d| match d {
        Value::Document(d) => d.get("v").cloned(),
        _ => None,
    })
}

/// One writer: inserts a document, increments it and reads it back until `stop`; how many it
/// wrote, and the operations that failed or took longer than `SLOW`.
fn writer(
    n: &Node<MemDisk>,
    w: i32,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<(usize, Vec<String>)> {
    let mut c = Client::new(n);
    thread::spawn(move || {
        let (mut written, mut bad) = (0, Vec::new());
        while !stop.load(Ordering::Acquire) {
            let _id = id(w, written);
            let n = i32::try_from(written).unwrap();
            let doc =
                Document::new().with("_id", _id.as_str()).with("c", w).with("n", n).with("v", 0);
            let insert = Document::new()
                .with("insert", coll(w))
                .with("documents", Value::Array(vec![doc.into()]));
            let (update, find) = (update(coll(w), &_id), find(coll(w), &_id));
            for (what, cmd) in [("insert", insert), ("update", update), ("find", find)] {
                let started = Instant::now();
                let r = c.run("db", cmd);
                if !ok(&r) || started.elapsed() > SLOW {
                    bad.push(format!("{what} {_id} in {:?}: {r:?}", started.elapsed()));
                }
            }
            written += 1;
        }
        (written, bad)
    })
}

#[test]
fn every_key_answers_after_splits_by_load() {
    let n = Node::start(config(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let mut c = Client::new(&n);
    let ops = Document::new().with("opsPerSecond", SPLIT_OPS);
    let param = Document::new().with("teselaLoadSplitOpsPerSecond", ops);
    until("the split parameter", || {
        let r = c.run("admin", Document::new().with("setClusterParameter", param.clone()));
        ok(&r).then_some(())
    });
    for name in ["c", "d"] {
        // The collection exists before its index, with a first document no writer touches.
        let first = Document::new().with("insert", name).with("documents", docs(-1..0));
        assert!(ok(&c.run("db", first)));
        // A secondary index: each write also writes its entry.
        let key = Document::new().with("c", 1).with("n", 1);
        let index = Document::new().with("key", key).with("name", "c_n");
        let create = Document::new()
            .with("createIndexes", name)
            .with("indexes", Value::Array(vec![index.into()]));
        assert!(ok(&c.run("db", create)));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..WRITERS).map(|w| writer(&n, w, stop.clone())).collect();
    thread::sleep(LOAD);
    stop.store(true, Ordering::Release);
    let mut bad = Vec::new();
    let mut written = Vec::new();
    for (w, h) in (0..WRITERS).zip(writers) {
        let (count, b) = h.join().unwrap();
        bad.extend(b);
        written.push((w, count));
    }
    let ranges = n.ranges().len();
    assert!(ranges > 1, "the load split the range: {ranges} ranges");
    for (w, count) in written {
        for k in 0..count {
            let _id = id(w, k);
            let r = c.run("db", find(coll(w), &_id));
            if value_of(&r) != Some(Value::Int32(1)) {
                bad.push(format!("read {_id} after the load: {r:?}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} operations failed or stalled, the first: {:?}",
        bad.len(),
        bad.first()
    );
    n.stop();
}
