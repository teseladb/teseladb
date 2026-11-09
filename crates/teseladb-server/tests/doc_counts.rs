//! `count` without a filter from the counts the ranges keep (007 FR-039, H-004): exact after
//! inserts and deletes, and without reading the documents once the leader counted them.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

fn count(c: &mut Client) -> i64 {
    let r = c.run("app", Document::new().with("count", "c"));
    assert!(ok(&r), "{r:?}");
    match r.get("n") {
        Some(Value::Int32(n)) => i64::from(*n),
        other => panic!("{other:?}"),
    }
}

/// The collection scans `serverStatus` has counted.
fn scans(c: &mut Client) -> i64 {
    let r = c.run("admin", Document::new().with("serverStatus", 1));
    let Some(Value::Document(m)) = r.get("metrics") else { panic!("{r:?}") };
    let Some(Value::Document(q)) = m.get("queryExecutor") else { panic!("{m:?}") };
    let Some(Value::Document(s)) = q.get("collectionScans") else { panic!("{q:?}") };
    match s.get("total") {
        Some(Value::Int64(n)) => *n,
        Some(Value::Int32(n)) => i64::from(*n),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_count_without_a_filter_adds_up_the_ranges_counts() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let insert = Document::new().with("insert", "c").with("documents", docs(0..300));
    assert!(ok(&c.run("app", insert)));
    let gone = Document::new()
        .with("q", Document::new().with("_id", Document::new().with("$lt", 30)))
        .with("limit", 0);
    let delete =
        Document::new().with("delete", "c").with("deletes", Value::Array(vec![gone.into()]));
    assert!(ok(&c.run("app", delete)));
    assert_eq!(count(&mut c), 270);
    // Once the counts are exact, a count reads no document.
    until("a count without a scan", || {
        let before = scans(&mut c);
        let n = count(&mut c);
        (scans(&mut c) == before).then_some(n)
    });
    let more = Document::new().with("insert", "c").with("documents", docs(1_000..1_050));
    assert!(ok(&c.run("app", more)));
    let before = scans(&mut c);
    assert_eq!(count(&mut c), 320);
    assert_eq!(scans(&mut c), before, "counted without reading the documents");
    // The documents the count gives are there.
    let find = Document::new().with("find", "c").with("batchSize", 1_000);
    assert_eq!(batch(&c.run("app", find), "firstBatch").len(), 320);
    node.stop();
}
