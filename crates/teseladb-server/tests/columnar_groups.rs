//! An aggregation over a collection whose files hold the columns of their blocks (010 FR-033):
//! once a compaction wrote them, the part of a `$group` that reads only top-level fields takes
//! those blocks as their columns on the core, also in a collection of one range, and the answer
//! is the one of the same pipeline read and run in one place (a command with `let` variables
//! never splits).

mod common;

use common::{Client, READY, batch, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::pushed::column_batches;

const DOCS: i32 = 4000;

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

fn aggregate(c: &mut Client, pipeline: &[Document], with_let: bool) -> Vec<Value> {
    let stages: Vec<Value> = pipeline.iter().cloned().map(Value::Document).collect();
    let mut cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(stages))
        .with("cursor", Document::new().with("batchSize", 10_000));
    if with_let {
        cmd.insert("let", Document::new().with("unused", 1));
    }
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

#[test]
fn an_aggregation_takes_the_compacted_blocks_as_columns_and_answers_as_in_one_place() {
    let config = NodeConfig { cores: Some(1), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    for at in (0..DOCS).step_by(500) {
        let insert = Document::new().with("insert", "c").with("documents", docs(at..at + 500));
        assert!(ok(&c.run("app", insert)));
    }
    // A fraction and a key of seven values on every document.
    let set = Document::new()
        .with("x", op("$divide", Value::Array(vec!["$_id".into(), 3.into()])))
        .with("k", op("$mod", Value::Array(vec!["$_id".into(), 7.into()])));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", Value::Array(vec![Value::Document(Document::new().with("$set", set))]))
        .with("multi", true);
    let update =
        Document::new().with("update", "c").with("updates", Value::Array(vec![all.into()]));
    assert!(ok(&c.run("app", update)));
    let compact = Document::new().with("compact", "c").with("force", true);
    assert!(ok(&c.run("app", compact)));
    let on_columns = Document::new().with(
        "$group",
        Document::new()
            .with("_id", "$k")
            .with("s", op("$sum", "$x"))
            .with("n", op("$sum", "$v"))
            .with("a", op("$avg", "$x"))
            .with("mn", op("$min", "$x"))
            .with("mx", op("$max", "$v"))
            .with("c", op("$count", Document::new())),
    );
    let as_documents = Document::new().with(
        "$group",
        Document::new()
            .with("_id", Value::Null)
            .with("f", op("$first", "$x"))
            .with("l", op("$last", "$_id"))
            .with("s", op("$sum", "$x")),
    );
    let matches = Document::new().with("$match", Document::new().with("v", op("$gte", 9_000)));
    let pipelines = vec![vec![on_columns.clone()], vec![as_documents], vec![matches, on_columns]];
    for pipeline in &pipelines {
        let one_place = aggregate(&mut c, pipeline, true);
        assert!(!one_place.is_empty());
        let on_columns = until("a run on columns", || {
            let before = column_batches();
            let out = aggregate(&mut c, pipeline, false);
            assert_eq!(out, one_place, "{pipeline:?}");
            (column_batches() > before).then_some(out)
        });
        assert_eq!(on_columns, one_place, "{pipeline:?}");
    }
    node.stop();
}
