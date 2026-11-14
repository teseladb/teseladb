//! A scan the core serves goes a bounded part at a time (H-051 of the native tests): a scan of a
//! whole collection held the core's writes behind it. Every document comes once, in order, across
//! the parts, for a collection of several parts; a linearizable read never goes on a connection's
//! thread, so the core serves all of it. A scan on connections' threads that writes keep sending
//! back goes in smaller parts, and each count while writers update the documents is the whole
//! collection's.

mod common;

use common::{Client, READY, batch, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

/// Documents in the collection: several of the core's parts.
const DOCS: i32 = 2600;

/// An integer value as `i64`.
fn int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int32(n)) => Some(i64::from(*n)),
        Some(Value::Int64(n)) => Some(*n),
        _ => None,
    }
}

#[test]
fn a_scan_the_core_serves_in_parts_gives_every_document_once_in_order() {
    let n = Node::start(common::config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let mut c = Client::new(&n);
    for at in (0..DOCS).step_by(500) {
        let insert =
            Document::new().with("insert", "c").with("documents", docs(at..(at + 500).min(DOCS)));
        assert!(ok(&c.run("db", insert)));
    }
    let concern = Document::new().with("level", "linearizable");
    let group = Document::new()
        .with("_id", Value::Null)
        .with("n", Document::new().with("$sum", 1))
        .with("v", Document::new().with("$sum", "$v"));
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(vec![Document::new().with("$group", group).into()]))
        .with("cursor", Document::new())
        .with("readConcern", concern.clone());
    // The collection's documents are all applied once the first answer comes.
    let r = until("the aggregation", || Some(c.run("db", cmd.clone())).filter(ok));
    let first = batch(&r, "firstBatch");
    let Some(Value::Document(d)) = first.first() else { panic!("{r:?}") };
    let want: i64 = (0..i64::from(DOCS)).map(|i| i * 10).sum();
    assert_eq!(d.get("n"), Some(&Value::Int32(DOCS)), "{d:?}");
    assert_eq!(int(d.get("v")), Some(want), "{d:?}");
    // In order: the `_id`s of a find read back as they went in.
    let find =
        Document::new().with("find", "c").with("batchSize", DOCS).with("readConcern", concern);
    let r = c.run("db", find);
    let ids: Vec<Option<i64>> = batch(&r, "firstBatch")
        .iter()
        .map(|d| match d {
            Value::Document(d) => int(d.get("_id")),
            _ => None,
        })
        .collect();
    let want: Vec<Option<i64>> = (0..i64::from(DOCS)).map(Some).collect();
    assert_eq!(ids, want);
    // Writers update documents while counts read the collection on connections' threads.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (mut c, stop) = (Client::new(&n), stop.clone());
            std::thread::spawn(move || {
                let mut i = w;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let inc = Document::new().with("$inc", Document::new().with("u", 1));
                    let q = Document::new().with("_id", i % DOCS);
                    let one = Document::new().with("q", q).with("u", inc);
                    let update = Document::new()
                        .with("update", "c")
                        .with("updates", Value::Array(vec![one.into()]));
                    assert!(ok(&c.run("db", update)));
                    i += 97;
                }
            })
        })
        .collect();
    // A sum of a field reads every document, where a plain count takes the ranges' counts.
    let group = Document::new()
        .with("_id", Value::Null)
        .with("n", Document::new().with("$sum", 1))
        .with("v", Document::new().with("$sum", "$v"));
    let count = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(vec![Document::new().with("$group", group).into()]))
        .with("cursor", Document::new());
    for _ in 0..40 {
        let r = c.run("db", count.clone());
        let first = batch(&r, "firstBatch");
        let Some(Value::Document(d)) = first.first() else { panic!("{r:?}") };
        assert_eq!(int(d.get("n")), Some(i64::from(DOCS)), "a count while writers update");
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    for w in writers {
        w.join().unwrap();
    }
    n.stop();
}
