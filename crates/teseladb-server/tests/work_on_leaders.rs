//! Three nodes in one process, the client on a node that does not lead the collection's ranges
//! (010 FR-034): the `$match`es and the partial `$group` of an aggregation travel in bytes to the
//! leader of each range on another node, which runs them on its core and answers its partial
//! groups. The answer is the one of the same pipeline read and run in one place (a command with
//! `let` variables never splits), and a pipeline whose sums do not merge exactly still answers
//! the same through the usual path.

mod common;

use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::backend::{scattered_groups, scattered_parts_sent_away};
use teseladb_server::node::Node;
use teseladb_server::pushed::ran_for_other_nodes;
use teseladb_bson::{Document, Value};
use teseladb_raft::Role;
use teseladb_raftlog::MemDisk;

const DOCS: i32 = 3000;

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

/// Three nodes, the first one the seed.
fn cluster() -> Vec<Node<MemDisk>> {
    let first = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    for _ in 0..2 {
        nodes.push(Node::start(config(seed.clone()), || Ok(MemDisk::new()), None).unwrap());
    }
    for n in &nodes {
        assert!(n.wait_ready(READY));
    }
    nodes
}

/// A node that follows the data range, so that the ranges split from it are led elsewhere.
fn follower(nodes: &[Node<MemDisk>]) -> &Node<MemDisk> {
    until("a replica that follows", || {
        nodes.iter().find(|n| {
            n.data_status(Duration::from_secs(2)).is_some_and(|s| s.role == Some(Role::Follower))
        })
    })
}

/// `DOCS` documents with `v`, a fraction `x` and a key `k` of seven values, in a collection split
/// in five ranges.
fn load(c: &mut Client) {
    for at in (0..DOCS).step_by(500) {
        let insert = Document::new().with("insert", "c").with("documents", docs(at..at + 500));
        let r = c.run("app", insert);
        assert!(ok(&r), "{r:?}");
    }
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
    until("five ranges", || (ranges(c) >= 5).then_some(()));
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

fn group(id: Value) -> Document {
    let spec = Document::new()
        .with("_id", id)
        .with("s", op("$sum", "$v"))
        .with("a", op("$avg", "$v"))
        .with("mn", op("$min", "$v"))
        .with("mx", op("$max", "$v"))
        .with("f", op("$first", "$_id"))
        .with("l", op("$last", "$x"))
        .with("c", op("$count", Document::new()));
    Document::new().with("$group", spec)
}

#[test]
fn an_aggregation_runs_its_partial_group_on_the_leaders_of_other_nodes() {
    let nodes = cluster();
    let mut c = Client::new(follower(&nodes));
    load(&mut c);
    let matches = Document::new().with("$match", Document::new().with("v", op("$gte", 9_000)));
    let sort = Document::new().with("$sort", Document::new().with("s", -1));
    let limit = Document::new().with("$limit", 3);
    let pipelines = vec![
        vec![group("$k".into())],
        vec![matches.clone(), group("$k".into()), sort, limit],
        vec![matches, group(Value::Null)],
    ];
    let (sent, ran) = (scattered_parts_sent_away(), ran_for_other_nodes());
    for pipeline in &pipelines {
        let one_place = aggregate(&mut c, pipeline, true);
        let split = until("a split run", || {
            let before = scattered_groups();
            let out = aggregate(&mut c, pipeline, false);
            assert_eq!(out, one_place, "{pipeline:?}");
            (scattered_groups() > before).then_some(out)
        });
        assert_eq!(split, one_place, "{pipeline:?}");
    }
    // The program went to leaders on other nodes, and they ran it for this one.
    assert!(scattered_parts_sent_away() > sent);
    assert!(ran_for_other_nodes() > ran);
    // Sums of fractions do not merge: the leaders' groups come back, the merge refuses them and
    // the usual path answers.
    let fractions =
        vec![
            Document::new().with(
                "$group",
                Document::new()
                    .with("_id", "$k")
                    .with("s", op("$sum", "$x"))
                    .with("a", op("$avg", "$x")),
            ),
        ];
    let ran = ran_for_other_nodes();
    assert_eq!(aggregate(&mut c, &fractions, false), aggregate(&mut c, &fractions, true));
    assert!(ran_for_other_nodes() > ran);
    let whole = aggregate(&mut c, &pipelines[2], false);
    let Some(Value::Document(d)) = whole.first() else { panic!("{whole:?}") };
    let want: i32 = (900..DOCS).map(|i| i * 10).sum();
    assert_eq!(d.get("s"), Some(&Value::Int32(want)));
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}
