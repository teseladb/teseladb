//! Retryable writes and reads on a running node (006 T034, FR-018): a client whose connection
//! drops after sending a write sends it again on a new connection with the same `lsid` and
//! `txnNumber`, and the range answers the first result without writing twice; a read sent again
//! reads the same documents.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

fn lsid(n: u8) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })
}

fn find_one(c: &mut Client, id: i32) -> Option<Document> {
    let r = c.run(
        "app",
        Document::new().with("find", "c").with("filter", Document::new().with("_id", id)),
    );
    match batch(&r, "firstBatch").first() {
        Some(Value::Document(d)) => Some(d.clone()),
        _ => None,
    }
}

fn inc(n: i64) -> Document {
    let u = Document::new()
        .with("q", Document::new().with("_id", 1))
        .with("u", Document::new().with("$inc", Document::new().with("v", 1)));
    Document::new()
        .with("update", "c")
        .with("updates", vec![Value::Document(u)])
        .with("lsid", lsid(3))
        .with("txnNumber", n)
}

#[test]
fn a_write_whose_connection_drops_applies_once_when_retried() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut reader = Client::new(&node);
    assert!(ok(
        &reader.run("app", Document::new().with("insert", "c").with("documents", docs(0..3)))
    ));
    // The write is applied but its reply never reaches the client.
    let mut lost = Client::new(&node);
    lost.send("app", inc(1));
    until("the update", || {
        find_one(&mut reader, 1).filter(|d| d.get("v") == Some(&Value::Int32(11)))
    });
    drop(lost);
    let mut again = Client::new(&node);
    let r = again.run("app", inc(1));
    assert_eq!(
        (r.get("n"), r.get("nModified")),
        (Some(&Value::Int32(1)), Some(&Value::Int32(1))),
        "{r:?}"
    );
    assert_eq!(find_one(&mut reader, 1).and_then(|d| d.get("v").cloned()), Some(Value::Int32(11)));
    // The connection drops right after the write leaves: applied once whether or not the node
    // read it before the drop.
    let mut cut = Client::new(&node);
    cut.send("app", inc(2));
    drop(cut);
    let r = again.run("app", inc(2));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(find_one(&mut reader, 1).and_then(|d| d.get("v").cloned()), Some(Value::Int32(12)));
    // An insert sent again does not meet its own duplicate key.
    let insert = || {
        Document::new()
            .with("insert", "c")
            .with("documents", docs(10..12))
            .with("lsid", lsid(3))
            .with("txnNumber", 3i64)
    };
    let first = again.run("app", insert());
    let second = Client::new(&node).run("app", insert());
    assert_eq!(first.get("n"), second.get("n"));
    assert!(second.get("writeErrors").is_none(), "{second:?}");
    drop(node.stop());
}

#[test]
fn a_read_sent_again_reads_the_same_documents() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("insert", "c").with("documents", docs(0..4)))));
    let find = || Document::new().with("find", "c").with("lsid", lsid(4));
    let mut lost = Client::new(&node);
    lost.send("app", find());
    drop(lost);
    let a = batch(&Client::new(&node).run("app", find()), "firstBatch");
    let b = batch(&Client::new(&node).run("app", find()), "firstBatch");
    assert_eq!(a.len(), 4);
    assert_eq!(a, b);
    drop(node.stop());
}
