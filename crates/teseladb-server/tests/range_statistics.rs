//! Statistics from the compactions (010 FR-029): a compaction of a core's LSM marks the ranges
//! whose keys it rewrote, and the node's statistics thread reads their indexes and publishes
//! their statistics; the planner then takes the plan the estimates find far cheaper without a
//! trial.
#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;
use teseladb_io::engine::disk::DiskPlace;

use common::{Client, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_server::client_listener::Hooks;
use teseladb_server::node::{Node, NodeConfig};

const DB: &str = "stats";
const DOCS: i32 = 2000;

fn node() -> Node {
    let cfg = NodeConfig {
        client_hooks: Hooks { stats_every: Duration::from_millis(50), ..Hooks::default() },
        ..config(Vec::new())
    };
    let n = Node::start(cfg, DiskPlace::Memory, None).expect("starts");
    assert!(n.wait_ready(common::READY));
    n
}

fn run(c: &mut Client, cmd: Document) -> Document {
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    r
}

/// The filter of the find: one document by `v`, half of them by `b`.
fn filter() -> Document {
    Document::new().with("v", 70).with("b", 1)
}

/// The number of rejected plans of the find of [`filter`], and its winning plan.
fn planned(c: &mut Client) -> (usize, String) {
    let filter = filter();
    let find = Document::new().with("find", "c").with("filter", filter);
    let r = run(c, Document::new().with("explain", find).with("verbosity", "queryPlanner"));
    let shard = match r.get("queryPlanner").and_then(|q| match q {
        Value::Document(q) => q.get("winningPlan").cloned(),
        _ => None,
    }) {
        Some(Value::Document(w)) => match w.get("shards") {
            Some(Value::Array(s)) => match s.first() {
                Some(Value::Document(d)) => d.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let rejected = match shard.get("rejectedPlans") {
        Some(Value::Array(a)) => a.len(),
        other => panic!("{other:?}"),
    };
    (rejected, format!("{:?}", shard.get("winningPlan")))
}

#[test]
fn a_compaction_gives_the_planner_statistics_and_it_picks_without_a_trial() {
    let n = node();
    let mut c = Client::new(&n);
    for (name, field) in [("v_1", "v"), ("b_1", "b")] {
        let index = Document::new().with("key", Document::new().with(field, 1)).with("name", name);
        let cmd = Document::new().with("createIndexes", "c").with("indexes", vec![index.into()]);
        run(&mut c, cmd);
    }
    // `v` is ten times `_id`, and `b` is `_id` modulo 2.
    run(&mut c, Document::new().with("insert", "c").with("documents", docs(0..DOCS)));
    let parity = Document::new().with("$mod", vec![Value::from("$_id"), Value::from(2)]);
    let set = Document::new().with("$set", Document::new().with("b", parity));
    let u = Document::new()
        .with("q", Document::new())
        .with("u", vec![Value::Document(set)])
        .with("multi", true);
    run(&mut c, Document::new().with("update", "c").with("updates", vec![u.into()]));
    // Without statistics the two indexes run a trial.
    assert_eq!(planned(&mut c).0, 1);
    // The compaction rewrites the range's files; its statistics follow.
    run(&mut c, Document::new().with("compact", "c").with("force", true));
    let plan = until("the plan by its estimate", || {
        let (rejected, plan) = planned(&mut c);
        (rejected == 0).then_some(plan)
    });
    assert!(plan.contains("\"v_1\""), "{plan}");
    let r = run(&mut c, Document::new().with("find", "c").with("filter", filter()));
    let found = batch(&r, "firstBatch");
    assert_eq!(found.len(), 1, "{found:?}");
    drop(n.stop());
}
