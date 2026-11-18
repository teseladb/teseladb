//! The parts of an unordered insert over a collection's partitions (D-186) come to their cores
//! with their evaluation prepared on the connection's thread (005 FR-035), as a single write does:
//! the cores only check that the versions did not change and propose. Alone in its binary, so
//! that no other test's writes move the process's counters.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, ok};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_wire::commands::mutation::{EvaluatedBy, evaluated_counts};

const ROUNDS: i32 = 60;
const DOCS: i32 = 5;

#[test]
fn the_parts_of_an_unordered_insert_come_prepared_to_their_cores() {
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    // A first insert, so that every partition's range has published its view.
    let warm: Vec<Value> =
        (0..64).map(|i| Value::Document(Document::new().with("_id", -1 - i))).collect();
    assert!(ok(
        &c.run("app", Document::new().with("insert", "c").with("documents", Value::Array(warm)))
    ));
    let before = evaluated_counts();
    for round in 0..ROUNDS {
        let docs: Vec<Value> = (0..DOCS)
            .map(|k| Value::Document(Document::new().with("_id", round * DOCS + k).with("v", k)))
            .collect();
        let insert = Document::new()
            .with("insert", "c")
            .with("documents", Value::Array(docs))
            .with("ordered", false);
        let r = c.run("app", insert);
        assert!(ok(&r), "{r:?}");
    }
    let after = evaluated_counts();
    let by = |how: EvaluatedBy| after[how as usize] - before[how as usize];
    let writes = by(EvaluatedBy::Prepared) + by(EvaluatedBy::Again) + by(EvaluatedBy::Core);
    assert!(writes >= u64::try_from(ROUNDS).unwrap(), "{writes} writes evaluated");
    // Most parts come prepared; one may be evaluated in its core when a view lags.
    assert!(
        by(EvaluatedBy::Prepared) * 10 >= writes * 9,
        "prepared {} of {writes}, in the core {}",
        by(EvaluatedBy::Prepared),
        by(EvaluatedBy::Core)
    );
    node.stop();
}
