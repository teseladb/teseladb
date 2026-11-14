//! An aggregation over a collection split in several ranges on several cores (010 FR-034): its
//! `$match`es and partial `$group` run on the cores that own the ranges, and the coordinator
//! merges their partial groups. The answer is the one of the same pipeline read and run in one
//! place (a command with `let` variables never splits), the order of the groups, `$first` and
//! `$last` included, and a pipeline whose sums do not merge exactly still answers the same.

mod common;

use common::{Client, READY, batch, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::backend::scattered_groups;
use teseladb_server::node::{Node, NodeConfig};

const DOCS: i32 = 3000;

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

fn group(id: Value) -> Document {
    let spec = Document::new()
        .with("_id", id)
        .with("s", op("$sum", "$v"))
        .with("one", op("$sum", 1))
        .with("a", op("$avg", "$v"))
        .with("mn", op("$min", "$v"))
        .with("mx", op("$max", "$v"))
        .with("f", op("$first", "$_id"))
        .with("l", op("$last", "$x"))
        .with("c", op("$count", Document::new()));
    Document::new().with("$group", spec)
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
fn an_aggregation_over_ranges_on_several_cores_merges_into_the_answer_in_one_place() {
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    for at in (0..DOCS).step_by(500) {
        let insert = Document::new().with("insert", "c").with("documents", docs(at..at + 500));
        assert!(ok(&c.run("app", insert)));
    }
    // A fraction and a key of seven values on every document.
    let set = Document::new()
        .with("x", op("$divide", Value::Array(vec!["$_id".into(), 4.into()])))
        .with("k", op("$mod", Value::Array(vec!["$_id".into(), 7.into()])));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", Value::Array(vec![Value::Document(Document::new().with("$set", set))]))
        .with("multi", true);
    let update =
        Document::new().with("update", "c").with("updates", Value::Array(vec![all.into()]));
    assert!(ok(&c.run("app", update)));
    let shard = Document::new()
        .with("shardCollection", "app.c")
        .with("key", Document::new().with("_id", 1));
    assert!(ok(&c.run("admin", shard)));
    for at in [400, 1100, 1700, 2300] {
        let split =
            Document::new().with("split", "app.c").with("middle", Document::new().with("_id", at));
        until("the split", || ok(&c.run("admin", split.clone())).then_some(()));
    }
    let ranges = |c: &mut Client| {
        let r = c.run("config", Document::new().with("find", "chunks").with("batchSize", 100));
        batch(&r, "firstBatch").len()
    };
    until("five ranges", || (ranges(&mut c) >= 5).then_some(()));
    let by_mod = op("$mod", Value::Array(vec!["$_id".into(), 7.into()]));
    let matches = Document::new().with("$match", Document::new().with("v", op("$gte", 9_000)));
    let sort = Document::new().with("$sort", Document::new().with("s", -1));
    let fractions = Document::new().with(
        "$group",
        Document::new().with("_id", "$k").with("s", op("$sum", "$x")).with("a", op("$avg", "$x")),
    );
    let pipelines = vec![
        vec![group(by_mod)],
        vec![matches.clone(), group("$k".into()), sort],
        vec![matches, group(Value::Null)],
        vec![fractions],
    ];
    for pipeline in &pipelines {
        let one_place = aggregate(&mut c, pipeline, true);
        // Ranges that just split may still be moving: the answer is the same in any case, and
        // the split run happens once they settle.
        let split = until("a split run", || {
            let before = scattered_groups();
            let out = aggregate(&mut c, pipeline, false);
            assert_eq!(out, one_place, "{pipeline:?}");
            (scattered_groups() > before).then_some(out)
        });
        assert_eq!(split, one_place, "{pipeline:?}");
    }
    // The sum is the one of the values the `$match` keeps.
    let whole = aggregate(&mut c, &pipelines[2], false);
    let Some(Value::Document(d)) = whole.first() else { panic!("{whole:?}") };
    let want: i32 = (900..DOCS).map(|i| i * 10).sum();
    assert_eq!(d.get("s"), Some(&Value::Int32(want)));
    node.stop();
}
