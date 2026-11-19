//! A scan of documents does not pay for the positions its documents had in the commands that
//! wrote them (014 FR-020). Each write of a retryable command leaves a meta next to its document
//! for the change streams and the oplog; keyed by the write's position in its command, a
//! document written by commands of several statements gathered a meta key for each position it
//! had, and a scan handed each of them over before it left it out (one-node load p61: 2.8 to 5
//! metas and 7 to 12 entries for each document the cores' scans gave).
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, batch, docs, ok};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};

const DOCS: i32 = 60;
/// Statements of each command.
const STATEMENTS: i32 = 6;
/// Commands: each document is written by several, at another position in each.
const COMMANDS: i32 = 40;

fn count(c: &mut Client) -> i64 {
    let matched = Document::new().with("$match", Document::new().with("c", 1));
    let sum = Value::Document(Document::new().with("$sum", 1));
    let spec = Document::new().with("_id", Value::Null).with("n", sum);
    let group = Document::new().with("$group", spec);
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(vec![matched.into(), group.into()]))
        .with("cursor", Document::new());
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    match batch(&r, "firstBatch").first() {
        Some(Value::Document(d)) => match d.get("n") {
            Some(Value::Int32(n)) => i64::from(*n),
            Some(Value::Int64(n)) => *n,
            other => panic!("{other:?}"),
        },
        None => 0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_scan_gives_at_most_two_items_for_each_document_whatever_wrote_it() {
    let config = NodeConfig { cores: Some(1), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    let insert = Document::new().with("insert", "c").with("documents", docs(0..DOCS));
    assert!(ok(&c.run("app", insert)));
    // A driver's retryable writes: ordered commands of several statements, as a `bulkWrite`
    // sends them, each with its session and its number.
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![9; 16] });
    for command in 0..COMMANDS {
        let updates: Vec<Value> = (0..STATEMENTS)
            .map(|at| {
                // Seven and sixty share no factor: a document comes back at another position.
                let id = (command * 7 + at * 11) % DOCS;
                let set = Document::new().with("$set", Document::new().with("c", 1));
                Document::new().with("q", Document::new().with("_id", id)).with("u", set).into()
            })
            .collect();
        let cmd = Document::new()
            .with("update", "c")
            .with("updates", Value::Array(updates))
            .with("ordered", true)
            .with("lsid", lsid.clone())
            .with("txnNumber", i64::from(command) + 1);
        let r = c.run("app", cmd);
        assert!(ok(&r), "{r:?}");
    }
    let written = count(&mut c);
    assert!(written > i64::from(DOCS) / 2, "{written} documents written");
    // The least of three counts: a pass of the node's own (statistics) may fall in one.
    let items = (0..3)
        .map(|_| {
            let before = teseladb_lsm::scan_totals().items;
            assert_eq!(count(&mut c), written);
            teseladb_lsm::scan_totals().items - before
        })
        .min()
        .expect("three counts");
    let most = 2 * u64::try_from(DOCS).expect("positive");
    assert!(items <= most, "{items} items for {DOCS} documents");
}
