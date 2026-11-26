//! Reads of a collection spread over partitions (003 FR-012, D-186) see every write their client
//! was answered before: an unordered insert over several partitions and a multi update, each
//! followed at once by the aggregation that counts the client's documents, as the control
//! aggregation of the one-node load does, on a node of four cores whose partitions get their own
//! ranges while the client writes.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, ok};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};

const ROUNDS: i32 = 300;

fn count_and_sum(c: &mut Client) -> (i64, i64) {
    let matched = Document::new().with("$match", Document::new().with("c", 1));
    let sum = |f: Value| Value::Document(Document::new().with("$sum", f));
    let spec = Document::new()
        .with("_id", Value::Null)
        .with("n", sum(1.into()))
        .with("v", sum("$v".into()));
    let group = Document::new().with("$group", spec);
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(vec![matched.into(), group.into()]))
        .with("cursor", Document::new());
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    let first = common::batch(&r, "firstBatch");
    let Some(Value::Document(d)) = first.first() else { return (0, 0) };
    let number = |k: &str| match d.get(k) {
        Some(Value::Int32(i)) => i64::from(*i),
        Some(Value::Int64(i)) => *i,
        other => panic!("{k}: {other:?}"),
    };
    (number("n"), number("v"))
}

#[test]
fn a_client_s_aggregation_sees_each_write_it_was_answered() {
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    let (mut n, mut v) = (0i64, 0i64);
    for round in 0..ROUNDS {
        // Five new documents over the partitions, unordered.
        let docs: Vec<Value> = (0..5)
            .map(|k| {
                let id = round * 5 + k;
                Value::Document(Document::new().with("_id", id).with("c", 1).with("v", 0))
            })
            .collect();
        let insert = Document::new()
            .with("insert", "c")
            .with("documents", Value::Array(docs))
            .with("ordered", false);
        assert!(ok(&c.run("app", insert)));
        n += 5;
        // One in three rounds, every document of the client one more.
        if round % 3 == 0 {
            let u = Document::new()
                .with("q", Document::new().with("c", 1))
                .with("u", Document::new().with("$inc", Document::new().with("v", 1)))
                .with("multi", true);
            let updates = Value::Array(vec![u.into()]);
            assert!(
                ok(&c.run("app", Document::new().with("update", "c").with("updates", updates)))
            );
            v += n;
        }
        assert_eq!(count_and_sum(&mut c), (n, v), "round {round}");
    }
}

#[test]
fn a_client_s_aggregation_sees_each_transaction_over_partitions_it_committed() {
    // A transaction's writes in several partitions go across ranges: the range of its record
    // answers the commit, and the next read still sees every document (control_agg of p7, p8).
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
    let mut n = 0i64;
    for round in 0..ROUNDS {
        let txn = |cmd: Document, start: bool| {
            let cmd = cmd.with("lsid", lsid.clone()).with("txnNumber", i64::from(round) + 1);
            let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
            cmd.with("autocommit", false)
        };
        let docs: Vec<Value> = (0..4)
            .map(|k| {
                let id = round * 4 + k;
                Value::Document(Document::new().with("_id", id).with("c", 1).with("v", 0))
            })
            .collect();
        let insert = Document::new().with("insert", "c").with("documents", Value::Array(docs));
        let r = c.run("app", txn(insert, true));
        assert!(ok(&r), "{r:?}");
        let r = c.run("admin", txn(Document::new().with("commitTransaction", 1), false));
        assert!(ok(&r), "{r:?}");
        n += 4;
        assert_eq!(count_and_sum(&mut c).0, n, "round {round}");
    }
}

/// The `_id` and `v` of the documents a `find` of `filter` sorted by `sort` gives, by batches of
/// `size`: its first batch, then each `getMore` until the cursor ends.
fn read_by_batches(c: &mut Client, (filter, sort): (&Document, &Document), size: i32) -> Vec<i32> {
    let find = Document::new()
        .with("find", "c")
        .with("filter", filter.clone())
        .with("sort", sort.clone())
        .with("batchSize", size);
    let r = c.run("app", find);
    assert!(ok(&r), "{r:?}");
    let mut got = common::batch(&r, "firstBatch");
    let id_of = |r: &Document| match r.get("cursor") {
        Some(Value::Document(d)) => d.get("id").cloned().unwrap_or(Value::Int64(0)),
        _ => panic!("no cursor in {r:?}"),
    };
    let mut id = id_of(&r);
    while id != Value::Int64(0) {
        let more =
            Document::new().with("getMore", id).with("collection", "c").with("batchSize", size);
        let r = c.run("app", more);
        assert!(ok(&r), "a getMore after {} documents: {r:?}", got.len());
        got.extend(common::batch(&r, "nextBatch"));
        id = id_of(&r);
    }
    got.iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(i)) => *i,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

/// A collection spread over partitions read in key order, forward and backward, by `_id` and by
/// an index, in one batch and by batches that `getMore` goes on with, gives each document once
/// and in its order (25-11-2026, 0.2.1: a read backward took each partition's rows forward and
/// gave repeated documents out of order, and a cursor's `getMore` read in the partition's
/// numbers ended after its first batch).
#[test]
fn a_collection_over_partitions_reads_in_order_forward_and_backward_by_batches() {
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    let index = Document::new().with("key", Document::new().with("v", 1)).with("name", "v_1");
    let r = c.run(
        "app",
        Document::new()
            .with("createIndexes", "c")
            .with("indexes", Value::Array(vec![index.into()])),
    );
    assert!(ok(&r), "{r:?}");
    const N: i32 = 3000;
    for chunk in (0..N).collect::<Vec<_>>().chunks(500) {
        let docs: Vec<Value> = chunk
            .iter()
            .map(|&i| Value::Document(Document::new().with("_id", i).with("v", i % 17)))
            .collect();
        let r =
            c.run("app", Document::new().with("insert", "c").with("documents", Value::Array(docs)));
        assert!(ok(&r), "{r:?}");
    }
    let by_id: Vec<i32> = (0..N).collect();
    let by_v: Vec<i32> = {
        let mut ids: Vec<i32> = (0..N).filter(|i| i % 17 >= 3).collect();
        ids.sort_by_key(|i| (i % 17, *i));
        ids
    };
    let none = Document::new();
    let from_3 = Document::new().with("v", Document::new().with("$gte", 3));
    let reads: [(&Document, Document, Vec<i32>); 4] = [
        (&none, Document::new().with("_id", 1), by_id.clone()),
        (&none, Document::new().with("_id", -1), by_id.iter().rev().copied().collect()),
        (&from_3, Document::new().with("v", 1).with("_id", 1), by_v.clone()),
        (
            &from_3,
            Document::new().with("v", -1).with("_id", -1),
            by_v.iter().rev().copied().collect(),
        ),
    ];
    for (filter, sort, want) in &reads {
        for size in [100_000, 10] {
            let got = read_by_batches(&mut c, (filter, sort), size);
            assert_eq!(got.len(), want.len(), "{filter:?} {sort:?} by {size}");
            assert_eq!(&got, want, "{filter:?} {sort:?} by {size}");
        }
    }
}
