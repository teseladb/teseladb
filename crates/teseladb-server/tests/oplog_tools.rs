//! What `refdump --oplog` and `refrestore --oplogReplay` need of a running node (018 FR-009,
//! FR-012, T013): the development cluster presents itself as a replica set over the clients'
//! port it was given, a `find` on `local.oplog.rs` gives every entry of the history window
//! however long ago it was written, and the newest entry is the last write already acknowledged.

mod common;

use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::cli::DevOpts;
use teseladb_server::dev;
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

/// Writes spaced further apart than the one second a stream's read covers at a time.
const SPACING: Duration = Duration::from_millis(1_200);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn oplog(c: &mut Client, filter: Document) -> Vec<Document> {
    let find =
        Document::new().with("find", "oplog.rs").with("filter", filter).with("batchSize", 100_000);
    let r = c.run("local", find);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
        .into_iter()
        .map(|v| match v {
            Value::Document(d) => d,
            other => panic!("{other:?}"),
        })
        .collect()
}

/// The inserts into `ns` among `entries`, those an insert of several documents groups in an
/// `applyOps` entry included.
fn inserts_of(entries: &[Document], ns: &str) -> usize {
    let insert = |e: &Document| {
        e.get("op") == Some(&Value::from("i")) && e.get("ns") == Some(&Value::from(ns))
    };
    entries
        .iter()
        .map(|e| match e.get("o") {
            Some(Value::Document(o)) => match o.get("applyOps") {
                Some(Value::Array(ops)) => {
                    ops.iter().filter(|op| matches!(op, Value::Document(op) if insert(op))).count()
                }
                _ => usize::from(insert(e)),
            },
            _ => usize::from(insert(e)),
        })
        .sum()
}

/// With a clients' port given, each node announces it as its host (`config.shards`), so the
/// development cluster can present itself as a replica set (018 FR-007).
#[test]
fn nodes_on_a_given_port_announce_their_clients_address() {
    let port = free_port();
    let o = DevOpts { nodes: 1, cores: 1, port, test_commands: false };
    let nodes = dev::start(&o, Some(5)).expect("dev cluster");
    let mut c = Client::new(&nodes[0]);
    let host = format!("127.0.0.1:{port}");
    let shards = until("the node in config.shards", || {
        let r = c.run("config", Document::new().with("find", "shards"));
        let b = batch(&r, "firstBatch");
        (!b.is_empty()).then_some(b)
    });
    match &shards[0] {
        Value::Document(d) => assert_eq!(d.get("host"), Some(&Value::from(host.as_str()))),
        other => panic!("{other:?}"),
    }
    let value = Document::new().with("mode", "replicaset").with("setName", "rs0");
    let cmd = Document::new()
        .with("setClusterParameter", Document::new().with("topologyPresentation", value));
    let r = c.run("admin", cmd);
    assert!(ok(&r), "{r:?}");
    let hello = until("the replica set presentation", || {
        let h = c.run("admin", Document::new().with("hello", 1));
        (h.get("setName") == Some(&Value::from("rs0"))).then_some(h)
    });
    assert_eq!(hello.get("hosts"), Some(&Value::Array(vec![Value::from(host.as_str())])));
    nodes.into_iter().for_each(|n| drop(n.stop()));
}

/// Entries written seconds apart all come back from one `find`, with or without a lower bound
/// on `ts` below them, and the newest entry is the last acknowledged write.
#[test]
fn a_find_reads_the_whole_window_and_ends_at_the_last_write() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let first = c.run("dump", Document::new().with("insert", "a").with("documents", docs(0..3)));
    assert!(ok(&first), "{first:?}");
    let Some(Value::Timestamp { time, .. }) = first.get("operationTime").cloned() else {
        panic!("{first:?}")
    };
    std::thread::sleep(SPACING);
    let r = c.run("dump", Document::new().with("insert", "a").with("documents", docs(3..5)));
    assert!(ok(&r), "{r:?}");
    std::thread::sleep(SPACING);
    let last = c.run("dump", Document::new().with("insert", "a").with("documents", docs(5..6)));
    let last_ts = last.get("operationTime").cloned().expect("an operation time");
    let all = oplog(&mut c, Document::new());
    assert_eq!(inserts_of(&all, "dump.a"), 6, "{all:?}");
    let since = Document::new().with(
        "ts",
        Document::new().with("$gte", Value::Timestamp { time: time - 1, increment: 0 }),
    );
    assert_eq!(inserts_of(&oplog(&mut c, since), "dump.a"), 6);
    let newest = Document::new()
        .with("find", "oplog.rs")
        .with("sort", Document::new().with("$natural", -1))
        .with("limit", 1);
    let r = c.run("local", newest);
    let b = batch(&r, "firstBatch");
    let Some(Value::Document(e)) = b.first() else { panic!("{r:?}") };
    // The last write's entry; the reply's `operationTime` is at or after its mark.
    let five = Document::new().with("_id", 5).with("v", 50);
    assert_eq!(e.get("o"), Some(&Value::Document(five)), "{e:?}");
    match (e.get("ts"), &last_ts) {
        (
            Some(Value::Timestamp { time, increment }),
            Value::Timestamp { time: t, increment: i },
        ) => {
            assert!((time, increment) <= (t, i), "{e:?} after {last_ts:?}");
        }
        other => panic!("{other:?}"),
    }
    drop(node.stop());
}

/// `$natural: -1` with a limit reads back from the newest entries: the newest two of writes
/// seconds apart, with a skip the ones before them, and with a filter the newest it matches.
#[test]
fn the_newest_entries_come_first() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    for i in 0..3 {
        let r =
            c.run("dump", Document::new().with("insert", "n").with("documents", docs(i..i + 1)));
        assert!(ok(&r), "{r:?}");
        std::thread::sleep(SPACING);
    }
    let newest = |c: &mut Client, filter: Document, limit: i32, skip: i32| {
        let cmd = Document::new()
            .with("find", "oplog.rs")
            .with("filter", filter)
            .with("sort", Document::new().with("$natural", -1))
            .with("skip", skip)
            .with("limit", limit);
        let r = c.run("local", cmd);
        batch(&r, "firstBatch")
            .into_iter()
            .map(|v| match v {
                Value::Document(d) => d.get("o").cloned(),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let o = |i: i32| Some(Value::Document(Document::new().with("_id", i).with("v", i * 10)));
    assert_eq!(newest(&mut c, Document::new(), 2, 0), [o(2), o(1)]);
    assert_eq!(newest(&mut c, Document::new(), 2, 1), [o(1), o(0)]);
    let inserts = Document::new().with("op", "i");
    assert_eq!(newest(&mut c, inserts, 5, 2), [o(0)]);
    drop(node.stop());
}

/// How long the writer of [`a_copy_with_the_oplog_replays_into_the_same_data`] writes, and the
/// pause between its rounds.
const WRITING: Duration = Duration::from_millis(2_500);
const PACE: Duration = Duration::from_millis(5);
/// Documents the copy starts from.
const COPIED: i32 = 300;

/// The writes a client makes while the copy runs: updates, inserts, deletes, replacements and a
/// transaction that creates a collection, as in the measurement with the real tools.
fn write_while_copying(node: &Node<MemDisk>) {
    let mut c = Client::new(node);
    let end = std::time::Instant::now() + WRITING;
    let mut i = 0;
    while std::time::Instant::now() < end {
        let q = Document::new().with("_id", i);
        let inc = Document::new()
            .with("$inc", Document::new().with("v", 1000))
            .with("$set", Document::new().with("w", i));
        let upd = Document::new().with("q", q.clone()).with("u", inc);
        let r = c.run("mig", Document::new().with("update", "a").with("updates", vec![upd.into()]));
        assert!(ok(&r), "{r:?}");
        let r = c.run("mig", Document::new().with("insert", "c").with("documents", docs(i..i + 1)));
        assert!(ok(&r), "{r:?}");
        if i % 3 == 0 {
            let gone = Document::new().with("_id", COPIED - 1 - i % COPIED);
            let del = Document::new().with("q", gone).with("limit", 1);
            let del = Document::new().with("delete", "a").with("deletes", vec![del.into()]);
            assert!(ok(&c.run("mig", del)));
        }
        if i % 10 == 0 {
            let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
            let txn = |cmd: Document, start: bool| {
                let cmd = cmd.with("lsid", lsid.clone()).with("txnNumber", Value::Int64(i.into()));
                let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
                cmd.with("autocommit", false)
            };
            let ins = Document::new().with("insert", "t").with("documents", docs(i..i + 1));
            assert!(ok(&c.run("mig", txn(ins, true))));
            let set = Document::new().with("$set", Document::new().with("tx", i));
            let upd = Document::new().with("q", Document::new().with("_id", i + 1)).with("u", set);
            let upd = Document::new().with("update", "a").with("updates", vec![upd.into()]);
            assert!(ok(&c.run("mig", txn(upd, false))));
            let commit = txn(Document::new().with("commitTransaction", 1), false);
            assert!(ok(&c.run("admin", commit)), "commit");
        }
        i += 1;
        std::thread::sleep(PACE);
    }
}

/// The newest entry's `ts`, as `refdump --oplog` reads it.
fn newest_ts(c: &mut Client) -> Value {
    let cmd = Document::new()
        .with("find", "oplog.rs")
        .with("sort", Document::new().with("$natural", -1))
        .with("limit", 1);
    let r = c.run("local", cmd);
    match batch(&r, "firstBatch").first() {
        Some(Value::Document(e)) => e.get("ts").cloned().expect("a ts"),
        _ => panic!("{r:?}"),
    }
}

/// Every document of `db.coll` in `_id` order, at `at` when given.
fn all_of(c: &mut Client, coll: &str, at: Option<&Value>) -> Vec<Value> {
    let mut find = Document::new()
        .with("find", coll)
        .with("sort", Document::new().with("_id", 1))
        .with("batchSize", 100_000);
    if let Some(at) = at {
        let rc = Document::new().with("level", "snapshot").with("atClusterTime", at.clone());
        find.push("readConcern", rc);
    }
    let r = c.run("mig", find);
    assert!(ok(&r), "{coll} at {at:?}: {r:?}");
    batch(&r, "firstBatch")
}

/// One entry as `refrestore --oplogReplay` gives it to `applyOps` (captured on 02-11-2026
/// from refrestore 100.20.0): without `ui`, and the operations of a transaction one by one
/// with `ts: Timestamp(0, 0)`, `t: null` and `v: 0`.
fn as_refrestore_gives_it(entry: &Document) -> Vec<Document> {
    let without_ui = |d: &Document| {
        d.iter().filter(|(k, _)| *k != "ui").fold(Document::new(), |o, (k, v)| o.with(k, v.clone()))
    };
    let nested = match entry.get("o") {
        Some(Value::Document(o)) => match o.get("applyOps") {
            Some(Value::Array(ops)) => ops.clone(),
            _ => return vec![without_ui(entry)],
        },
        _ => return vec![without_ui(entry)],
    };
    nested
        .iter()
        .map(|op| match op {
            Value::Document(op) => {
                let head = Document::new()
                    .with("ts", Value::Timestamp { time: 0, increment: 0 })
                    .with("t", Value::Null)
                    .with("v", 0);
                without_ui(op).iter().fold(head, |o, (k, v)| o.with(k, v.clone()))
            }
            other => panic!("{other:?}"),
        })
        .collect()
}

/// What `refdump --oplog` and `refrestore --oplogReplay` do, over the wire, between two
/// clusters while a client writes to the first (FR-012, SC-004): the newest entry, a copy of
/// every collection, the newest entry again, the entries between both, and each of them given
/// to `applyOps` on the second cluster. The second cluster ends with the first one's data at
/// the last entry. The real tools in both directions with `refnode` 9.0.4 run in
/// `tests/compat/tools/run.sh`.
#[test]
fn a_copy_with_the_oplog_replays_into_the_same_data() {
    let source = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    let target = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(source.wait_ready(READY) && target.wait_ready(READY));
    let mut s = Client::new(&source);
    let mut t = Client::new(&target);
    let r = s.run("mig", Document::new().with("insert", "a").with("documents", docs(0..COPIED)));
    assert!(ok(&r), "{r:?}");
    let r = s.run("mig", Document::new().with("insert", "c").with("documents", docs(-2..0)));
    assert!(ok(&r), "{r:?}");
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| write_while_copying(&source));
        std::thread::sleep(WRITING / 4);
        let start = newest_ts(&mut s);
        let list = Document::new().with("listCollections", 1).with("nameOnly", true);
        let names: Vec<String> = batch(&s.run("mig", list), "firstBatch")
            .into_iter()
            .map(|c| match c {
                Value::Document(c) => match c.get("name") {
                    Some(Value::String(n)) => n.clone(),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect();
        for coll in &names {
            assert!(ok(&t.run("mig", Document::new().with("create", coll.as_str()))));
            let rows = all_of(&mut s, coll, None);
            if !rows.is_empty() {
                let insert = Document::new().with("insert", coll.as_str()).with("documents", rows);
                let r = t.run("mig", insert);
                assert!(ok(&r), "{r:?}");
            }
            std::thread::sleep(WRITING / 8);
        }
        let end = newest_ts(&mut s);
        let range = Document::new().with("$gte", start).with("$lte", end.clone());
        let entries = oplog(&mut s, Document::new().with("ts", range));
        assert!(entries.len() > 10, "{entries:?}");
        for entry in &entries {
            for op in as_refrestore_gives_it(entry) {
                let cmd = Document::new().with("applyOps", vec![Value::Document(op)]);
                let r = t.run("admin", cmd);
                assert!(ok(&r), "{entry:?}: {r:?}");
            }
        }
        writer.join().expect("the writer");
        for coll in ["a", "c", "t"] {
            let (got, want) = (all_of(&mut t, coll, None), all_of(&mut s, coll, Some(&end)));
            let first = got.iter().zip(&want).position(|(g, w)| g != w);
            let at = first.map(|i| (&got[i], &want[i]));
            assert!(
                first.is_none() && got.len() == want.len(),
                "{coll} at {end:?}: {at:?} of {} and {}",
                got.len(),
                want.len()
            );
        }
    });
    drop(source.stop());
    drop(target.stop());
}
