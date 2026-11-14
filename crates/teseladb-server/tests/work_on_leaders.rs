//! Three nodes in one process, the client on a node that does not lead the collection's ranges.
//! The `$match`es and the partial `$group` of an aggregation travel in bytes to the leader of each
//! range on another node, which runs them on its core and answers its partial groups (010
//! FR-034); the answer is the one of the same pipeline read and run in one place (a command with
//! `let` variables never splits), and a pipeline whose sums do not merge exactly still answers
//! the same through the usual path. A scan's filter goes to the leaders the same way, which
//! answer only the documents it accepts, the next parts asked for ahead (FR-027); a `find`, an
//! `update` with `multi` and an aggregation's `$match` give what the same filter evaluated on
//! this node gives.
//!
//! The counters are the process's: the tests of this binary run one at a time ([`serial`]), so
//! that what one measures between two reads is its own work.

mod common;

use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::admin_http::{Method, request};
use teseladb_server::backend::{
    filtered_parts_sent, lookup_batches_sent, lookup_keys_filtered, lookup_keys_sent,
    parts_taken_ahead, scattered_groups, scattered_parts_sent_away,
};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::pushed::ran_for_other_nodes;
use teseladb_bson::{Document, Value};
use teseladb_raft::Role;
use teseladb_raftlog::MemDisk;

const DOCS: i32 = 3000;

/// Held by each test while it runs: the counters it reads are the whole process's.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

/// Three nodes, the first one the seed.
fn cluster() -> Vec<Node<MemDisk>> {
    cluster_with(config)
}

/// Three nodes with the configuration `cfg` gives for their seeds, the first one the seed.
fn cluster_with(cfg: impl Fn(Vec<String>) -> NodeConfig) -> Vec<Node<MemDisk>> {
    let first = Node::start(cfg(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    for _ in 0..2 {
        nodes.push(Node::start(cfg(seed.clone()), || Ok(MemDisk::new()), None).unwrap());
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
    let _serial = serial();
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

fn find(c: &mut Client, filter: Document, collation: bool) -> Vec<Value> {
    let mut cmd =
        Document::new().with("find", "c").with("filter", filter).with("batchSize", 10_000);
    if collation {
        cmd.insert("collation", Document::new().with("locale", "en"));
    }
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

/// `{field: value}` as `$expr`, which the leaders never evaluate: the usual path.
fn as_expr(field: &str, value: i32) -> Document {
    let eq = op("$eq", Value::Array(vec![format!("${field}").into(), value.into()]));
    Document::new().with("$expr", eq)
}

/// The ranges of collection `app.c` in key order, from the administration port of `at`.
fn ranges_of_c(at: &str) -> Vec<teseladb_types::GroupId> {
    let (status, body) = request(at, Method::Get, "/admin/ranges?collection=app.c", b"").unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let d = teseladb_rest::media::decode(&body, teseladb_rest::media::Format::Relaxed).unwrap();
    let Some(Value::Array(ranges)) = d.get("ranges") else { panic!("{d:?}") };
    let mut out: Vec<(String, u64)> = ranges
        .iter()
        .map(|r| {
            let Value::Document(r) = r else { panic!("{r:?}") };
            let id = match r.get("_id") {
                Some(Value::Int64(n)) => u64::try_from(*n).unwrap(),
                Some(Value::Int32(n)) => u64::try_from(*n).unwrap(),
                other => panic!("{other:?}"),
            };
            let Some(Value::String(start)) = r.get("start") else { panic!("{r:?}") };
            (start.clone(), id)
        })
        .collect();
    // The starts are hexadecimal, two digits a byte: their order is the keys'.
    out.sort();
    out.into_iter().map(|(_, g)| teseladb_types::GroupId(g)).collect()
}

/// The leader of range `g`, as every node that has a replica of it says, when they agree.
fn leader_of(nodes: &[Node<MemDisk>], g: teseladb_types::GroupId) -> Option<teseladb_types::NodeId> {
    let said: Vec<teseladb_types::NodeId> = nodes
        .iter()
        .filter_map(|n| n.range_status(g, Duration::from_secs(2)))
        .filter(|s| s.role.is_some())
        .map(|s| s.leader)
        .collect();
    let first = *said.first()?;
    (first.0 != 0 && said.iter().all(|l| *l == first)).then_some(first)
}

/// Documents added past the five ranges' last cut, all in the last range: a scan reads that
/// range in several parts of at most `SCAN_CHUNK` documents.
const TAIL: std::ops::Range<i32> = DOCS..DOCS + 4000;

#[test]
fn a_filtered_scan_runs_its_filter_on_the_leaders_of_other_nodes() {
    let _serial = serial();
    let nodes = cluster_with(|seeds| NodeConfig {
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        ..config(seeds)
    });
    let mut c = Client::new(&nodes[0]);
    load(&mut c);
    for at in TAIL.step_by(500) {
        let insert = Document::new().with("insert", "c").with("documents", docs(at..at + 500));
        assert!(ok(&c.run("app", insert)));
    }
    // No leader moves by the balancer while the test counts.
    assert!(ok(&c.run("admin", Document::new().with("balancerStop", 1))));
    let at = nodes[0].admin_addr().unwrap().to_string();
    // The last range holds every document from 2300 on: more than two parts of a scan.
    let tail_docs = usize::try_from(DOCS + 4000 - 2300).unwrap();
    assert!(tail_docs > 2 * teseladb_wire::commands::query::SCAN_CHUNK);
    let last = || {
        let ranges = ranges_of_c(&at);
        assert_eq!(ranges.len(), 5, "the four cuts and no other: {ranges:?}");
        *ranges.last().unwrap()
    };
    // The client goes to a node that does not lead the last range.
    let (g, leader) = until("a leader of the last range", || {
        let g = last();
        leader_of(&nodes, g).map(|l| (g, l))
    });
    let client = nodes.iter().find(|n| n.node_id() != leader).unwrap();
    let mut c = Client::new(client);
    let by_k = || Document::new().with("k", 3);
    let want: Vec<i32> = (0..DOCS).filter(|i| i % 7 == 3).collect();
    let ids = |docs: &[Value]| -> Vec<i32> {
        docs.iter()
            .map(|d| match d {
                Value::Document(d) => match d.get("_id") {
                    Some(Value::Int32(i)) => *i,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect()
    };
    // A find gives the same documents as the filter evaluated here, also with a collation.
    let here = find(&mut c, as_expr("k", 3), false);
    assert_eq!(ids(&here), want);
    assert_eq!(find(&mut c, by_k(), false), here);
    assert_eq!(find(&mut c, by_k(), true), here);
    // An aggregation whose `$match` the scan takes, not split (no `$group`), counted only while
    // the last range kept its leader on another node: its filter went with each part to the
    // leaders, and the parts of the last range after its first were asked for ahead and taken.
    let pipeline = |m: Document| {
        vec![
            Document::new().with("$match", m),
            Document::new().with("$project", Document::new().with("v", 1)),
        ]
    };
    let agg_here = aggregate(&mut c, &pipeline(as_expr("k", 3)), false);
    assert_eq!(agg_here.len(), want.len());
    let (sent, ran, ahead) = until("an aggregation while the leaders stay", || {
        let still = |g| leader_of(&nodes, g) == Some(leader) && last() == g;
        if !still(g) {
            return None;
        }
        let before = (filtered_parts_sent(), ran_for_other_nodes(), parts_taken_ahead());
        assert_eq!(aggregate(&mut c, &pipeline(by_k()), false), agg_here);
        let after = (filtered_parts_sent(), ran_for_other_nodes(), parts_taken_ahead());
        still(g).then_some((after.0 - before.0, after.1 - before.1, after.2 - before.2))
    });
    let tail_parts =
        u64::try_from(tail_docs.div_ceil(teseladb_wire::commands::query::SCAN_CHUNK)).unwrap();
    assert!(sent >= tail_parts, "{sent} parts sent, {tail_parts} in the last range alone");
    assert!(ran >= tail_parts, "{ran} parts ran for this node");
    assert!(ahead > 0, "no part asked for ahead was taken: {sent} sent");
    // An update of every document the filter keeps.
    let set = Document::new().with("$set", Document::new().with("u", 1));
    let many = Document::new().with("q", by_k()).with("u", set).with("multi", true);
    let update =
        Document::new().with("update", "c").with("updates", Value::Array(vec![many.into()]));
    let r = c.run("app", update);
    assert_eq!(
        r.get("nModified"),
        Some(&Value::Int32(i32::try_from(want.len()).unwrap())),
        "{r:?}"
    );
    assert_eq!(ids(&find(&mut c, as_expr("u", 1), false)), want);
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}

/// The value of the sample `line` (name and labels) in `text`.
fn sample(text: &str, line: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix(line)?.strip_prefix(' ')?.trim().parse().ok())
}

#[test]
fn the_work_sent_to_the_leaders_shows_in_the_metrics() {
    let _serial = serial();
    let nodes = cluster_with(|seeds| NodeConfig {
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        ..config(seeds)
    });
    let client = follower(&nodes);
    let mut c = Client::new(client);
    load(&mut c);
    // A split aggregation and a filtered scan, both through leaders on other nodes.
    until("a split run", || {
        let before = scattered_groups();
        aggregate(&mut c, &[group("$k".into())], false);
        (scattered_groups() > before).then_some(())
    });
    assert_eq!(find(&mut c, Document::new().with("k", 3), false).len(), 429);
    let at = client.admin_addr().unwrap().to_string();
    let (status, body) = request(&at, Method::Get, "/metrics", b"").unwrap();
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    for name in [
        "scattered_groups",
        "scattered_parts_sent_away",
        "filtered_parts_sent",
        "parts_taken_ahead",
        "ran_for_other_nodes",
    ] {
        let name = format!("teseladb_pushed_{name}_total");
        assert!(text.contains(&format!("# TYPE {name} counter")), "{name} in {text}");
        let n = sample(&text, &name).unwrap_or_else(|| panic!("{name} in {text}"));
        assert!(n > 0, "{name} is {n}");
    }
    for work in ["fold", "filter"] {
        let line = format!("teseladb_pushed_fallbacks_total{{work=\"{work}\"}}");
        assert!(sample(&text, &line).is_some(), "{line} in {text}");
    }
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}

/// The `_id`s of `docs`, sorted.
fn sorted_ids(docs: &[Value]) -> Vec<i32> {
    let mut ids: Vec<i32> = docs
        .iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(i)) => *i,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// A find of `filter` that reads the collection (`hint: {$natural: 1}`) or plans, with a
/// collation when asked.
fn find_by(c: &mut Client, filter: &Document, (scan, collation): (bool, bool)) -> Vec<i32> {
    let mut cmd =
        Document::new().with("find", "c").with("filter", filter.clone()).with("batchSize", 10_000);
    if scan {
        cmd.insert("hint", Document::new().with("$natural", 1));
    }
    if collation {
        cmd.insert("collation", Document::new().with("locale", "en"));
    }
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    sorted_ids(&batch(&r, "firstBatch"))
}

/// The lookup counters: batches, keys and keys sent with the filter.
fn lookups() -> (u64, u64, u64) {
    (lookup_batches_sent(), lookup_keys_sent(), lookup_keys_filtered())
}

#[test]
fn the_documents_of_an_index_scan_go_by_batches_to_the_leaders_of_other_nodes() {
    let _serial = serial();
    let nodes = cluster();
    let mut c = Client::new(follower(&nodes));
    load(&mut c);
    let index = Document::new().with("key", Document::new().with("k", 1)).with("name", "k_1");
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![index.into()]));
    assert!(ok(&c.run("app", create)));
    let filter = Document::new().with("k", 3).with("v", op("$gte", 9_000));
    let want: Vec<i32> = (900..DOCS).filter(|i| i % 7 == 3).collect();
    assert_eq!(find_by(&mut c, &filter, (true, false)), want);
    // A find through the index: the same documents, each batch to its leader with the filter.
    let (before, ran) = (lookups(), ran_for_other_nodes());
    assert_eq!(find_by(&mut c, &filter, (false, false)), want);
    let after = lookups();
    let batches = after.0 - before.0;
    assert!(batches > 0, "no batch went");
    assert!(after.1 - before.1 > batches, "batches of one key: {before:?} {after:?}");
    assert!(after.2 > before.2, "the filter did not go: {before:?} {after:?}");
    assert!(ran_for_other_nodes() > ran);
    // What explain counts is what the reads one by one counted: every entry's document examined.
    let explain = Document::new()
        .with("explain", Document::new().with("find", "c").with("filter", filter.clone()))
        .with("verbosity", "executionStats");
    let r = c.run("app", explain);
    let Some(Value::Document(stats)) = r.get("executionStats") else { panic!("{r:?}") };
    let examined = (0..DOCS).filter(|i| i % 7 == 3).count();
    let as_i64 = |v: Option<&Value>| match v {
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        other => panic!("{other:?} in {r:?}"),
    };
    assert_eq!(as_i64(stats.get("totalDocsExamined")), i64::try_from(examined).unwrap());
    assert_eq!(as_i64(stats.get("nReturned")), i64::try_from(want.len()).unwrap());
    // With a collation the plan's filter never travels, and the answer is the same.
    let before = lookups();
    assert_eq!(find_by(&mut c, &filter, (false, true)), want);
    assert_eq!(lookups().2, before.2, "a filter went with a collation");
    // A residual filter that reads more than the document stays on this node.
    let by_expr = Document::new()
        .with("k", 3)
        .with("$expr", op("$gte", Value::Array(vec!["$v".into(), 9_000.into()])));
    let before = lookups();
    assert_eq!(find_by(&mut c, &by_expr, (false, false)), want);
    let after = lookups();
    assert!(after.0 > before.0, "no batch went: {before:?} {after:?}");
    assert_eq!(after.2, before.2, "an `$expr` went to the leaders");
    // An aggregation's `$match` and an update with `multi` through the index.
    let pipeline = vec![
        Document::new().with("$match", filter.clone()),
        Document::new().with("$project", Document::new().with("v", 1)),
    ];
    assert_eq!(sorted_ids(&aggregate(&mut c, &pipeline, false)), want);
    let set = Document::new().with("$set", Document::new().with("w", 1));
    let many = Document::new().with("q", filter.clone()).with("u", set).with("multi", true);
    let update =
        Document::new().with("update", "c").with("updates", Value::Array(vec![many.into()]));
    let before = lookups();
    let r = c.run("app", update);
    assert_eq!(
        r.get("nModified"),
        Some(&Value::Int32(i32::try_from(want.len()).unwrap())),
        "{r:?}"
    );
    assert!(lookups().0 > before.0, "the update read key by key");
    assert_eq!(find_by(&mut c, &Document::new().with("w", 1), (true, false)), want);
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}
