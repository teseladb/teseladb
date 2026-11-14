//! The memory of a client request comes from its connection thread's arena (012 FR-027): a
//! `find` with a filter and an `aggregate` with `$group` over a thousand documents take most of
//! their allocations from the arena, and connections that come and go, each on its own thread,
//! leave no chunk of the arena behind them.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_sys::arena::{ArenaAlloc, stats};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

#[global_allocator]
static ALLOC: ArenaAlloc = ArenaAlloc;

const DOCS: i32 = 1000;

fn load(c: &mut Client) {
    let docs: Vec<Value> = (0..DOCS)
        .map(|i| {
            Value::Document(
                Document::new()
                    .with("_id", i)
                    .with("city", format!("city {}", i % 7))
                    .with("age", 20 + i % 50)
                    .with("name", format!("customer {i}")),
            )
        })
        .collect();
    let r = c.run("t", Document::new().with("insert", "c").with("documents", docs));
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(DOCS)), "{r:?}");
}

fn find() -> Document {
    let filter = Document::new().with("age", Document::new().with("$gte", 60));
    Document::new().with("find", "c").with("filter", filter).with("batchSize", 10_000)
}

fn group() -> Document {
    let by = Document::new()
        .with("_id", "$city")
        .with("n", Document::new().with("$sum", 1))
        .with("age", Document::new().with("$avg", "$age"));
    let pipeline = vec![Value::Document(Document::new().with("$group", by))];
    let cursor = Document::new().with("batchSize", 1000);
    Document::new().with("aggregate", "c").with("pipeline", pipeline).with("cursor", cursor)
}

#[test]
fn requests_allocate_from_their_arena_and_ended_connections_keep_no_chunk() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    // The node takes writes once its data range has a leader.
    until("a first write", || {
        let r = c.run("t", Document::new().with("insert", "warm").with("documents", docs(0..5)));
        ok(&r).then_some(())
    });
    load(&mut c);
    for (name, cmd, rows) in [("find", find as fn() -> Document, 200), ("$group", group, 7)] {
        let before = stats();
        let r = c.run("t", cmd());
        let after = stats();
        assert_eq!(batch(&r, "firstBatch").len(), rows, "{name}: {r:?}");
        let (carved, general) = (after.arena - before.arena, after.general - before.general);
        // The node's other threads (the cores, the clock) allocate from the system meanwhile.
        assert!(carved > general, "{name}: {carved} blocks carved, {general} from the system");
    }
    drop(c);
    // A thread per connection (D-152): a thread that ends leaves its chunks to the others. The
    // first connections fill what the node keeps for good (caches and tables, whose blocks keep
    // their chunks); after them, the chunks out stay where they were.
    let warm = connections(&node, 50);
    let after = connections(&node, 200);
    assert!(after <= warm + 2, "{warm} chunks out after 50 connections, {after} after 200 more");
    node.stop();
}

/// Opens `n` connections one after the other, each with a `find`, and returns the chunks of the
/// arena out once their threads ended.
fn connections(node: &Node<MemDisk>, n: usize) -> u64 {
    for _ in 0..n {
        let mut c = Client::new(node);
        let r = c.run("t", find());
        assert_eq!(batch(&r, "firstBatch").len(), 200);
    }
    // The threads of the connections end on their own once their client is gone.
    let mut last = stats().chunks_out;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = stats().chunks_out;
        if now == last {
            break;
        }
        last = now;
    }
    last
}
