//! Sessions across the nodes of a cluster (006 T051, FR-016): each node writes the sessions it
//! used to `config.system.sessions`, which every node reads, and `killSessions` or
//! `killAllSessions` on one node kills the sessions' cursors on all of them.

mod common;

use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

fn fast(join: Vec<String>) -> NodeConfig {
    let mut cfg = config(join);
    if let Some(c) = cfg.client.as_mut() {
        c.refresh_every = Duration::from_millis(100);
    }
    cfg
}

fn lsid(n: u8) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })
}

fn cursor(r: &Document) -> i64 {
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("no cursor in {r:?}") };
    let Some(Value::Int64(id)) = c.get("id") else { panic!("no id in {r:?}") };
    *id
}

/// Opens a cursor of one document per batch in session `n` on `c`.
fn open(c: &mut Client, n: u8) -> i64 {
    let find = Document::new().with("find", "c").with("batchSize", 1).with("lsid", lsid(n));
    cursor(&c.run("app", find))
}

fn get_more(c: &mut Client, id: i64, n: u8) -> Document {
    let cmd = Document::new().with("getMore", id).with("collection", "c").with("lsid", lsid(n));
    c.run("app", cmd)
}

#[test]
fn sessions_are_recorded_for_the_cluster_and_killed_on_every_node() {
    let first = Node::start(fast(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2)
        .map(|_| Node::start(fast(seed.clone()), || Ok(MemDisk::new()), None).unwrap())
        .collect();
    for n in std::iter::once(&first).chain(others.iter()) {
        assert!(n.wait_ready(READY));
    }
    let mut a = Client::new(&first);
    let mut b = Client::new(&others[0]);
    let mut c = Client::new(&others[1]);
    assert!(ok(&a.run("app", Document::new().with("insert", "c").with("documents", docs(0..5)))));
    // Session 1 is used on node b; node c reads its record.
    let on_b = open(&mut b, 1);
    let filter = Document::new().with("_id.id", Value::Binary { subtype: 4, bytes: vec![1; 16] });
    until("the record of session 1", || {
        let r = c.run(
            "config",
            Document::new().with("find", "system.sessions").with("filter", filter.clone()),
        );
        (r.get("cursor").is_some() && batch(&r, "firstBatch").len() == 1).then_some(())
    });
    // Killed from node a, its cursor on node b dies.
    let kill = Document::new().with("killSessions", vec![Value::Document(lsid(1))]);
    assert!(ok(&a.run("admin", kill)));
    let r = get_more(&mut b, on_b, 1);
    assert_eq!(r.get("code"), Some(&Value::Int32(43)), "{r:?}");
    // `killAllSessions` from node b reaches node c's cursors.
    let on_c = open(&mut c, 2);
    let kill_all = Document::new().with("killAllSessions", Vec::<Value>::new());
    assert!(ok(&b.run("admin", kill_all)));
    let r = get_more(&mut c, on_c, 2);
    assert_eq!(r.get("code"), Some(&Value::Int32(43)), "{r:?}");
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}
