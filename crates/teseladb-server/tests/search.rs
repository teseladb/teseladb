//! Search on running nodes (011 FR-010, FR-014): a `$search` without a read mark waits until every
//! range of its collection reaches the cluster's mark, which a range without writes reaches with
//! the marks its leader closes as time goes by. On a cluster of three nodes whose data range has
//! been idle for a while, a query answers within its `maxTimeMS` on every node.

mod common;

use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

/// The most a query on an idle range may take: its leader closes a mark at most 3 s behind its
/// clock, every 200 ms, and 200 ms behind every 20 ms once something reads its changes.
const ANSWER_WITHIN: Duration = Duration::from_secs(8);

fn search(c: &mut Client, q: &str) -> Document {
    let spec = Document::new().with("text", Document::new().with("query", q).with("path", "t"));
    let cmd = Document::new()
        .with("aggregate", "c")
        .with(
            "pipeline",
            Value::Array(vec![Value::Document(Document::new().with("$search", spec))]),
        )
        .with("cursor", Document::new())
        .with("maxTimeMS", 20_000);
    c.run("db", cmd)
}

fn queryable(c: &mut Client) -> bool {
    let r = c.run("db", Document::new().with("listSearchIndexes", "c"));
    batch(&r, "firstBatch").iter().any(
        |i| matches!(i, Value::Document(d) if d.get("queryable") == Some(&Value::Boolean(true))),
    )
}

#[test]
fn a_search_on_an_idle_range_answers_on_every_node() {
    let first = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2)
        .map(|_| Node::start(config(seed.clone()), || Ok(MemDisk::new()), None).unwrap())
        .collect();
    let nodes: Vec<&Node<MemDisk>> = std::iter::once(&first).chain(others.iter()).collect();
    for n in &nodes {
        assert!(n.wait_ready(READY));
    }
    let mut c = Client::new(&first);
    let Value::Array(plain) = docs(0..3) else { panic!("documents") };
    let texts = ["hola mundo", "adios mundo cruel", "otra cosa"];
    let docs = plain
        .into_iter()
        .zip(texts)
        .map(|(d, t)| match d {
            Value::Document(d) => Value::Document(d.with("t", t)),
            other => other,
        })
        .collect::<Vec<_>>();
    let r = c.run("db", Document::new().with("insert", "c").with("documents", Value::Array(docs)));
    assert_eq!(r.get("n"), Some(&Value::Int32(3)), "{r:?}");
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    let spec = Document::new().with("name", "default").with("definition", def);
    let r = c.run(
        "db",
        Document::new()
            .with("createSearchIndexes", "c")
            .with("indexes", Value::Array(vec![Value::Document(spec)])),
    );
    assert!(ok(&r), "{r:?}");
    until("a queryable index", || queryable(&mut c).then_some(()));
    // The data range stays without writes for longer than a stream's read window.
    std::thread::sleep(Duration::from_secs(2));
    for n in &nodes {
        let mut c = Client::new(n);
        let started = Instant::now();
        let r = search(&mut c, "mundo");
        assert!(ok(&r), "{r:?}");
        assert_eq!(batch(&r, "firstBatch").len(), 2, "{r:?}");
        assert!(started.elapsed() < ANSWER_WITHIN, "took {:?}", started.elapsed());
    }
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}
