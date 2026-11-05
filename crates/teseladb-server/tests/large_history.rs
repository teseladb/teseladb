//! Large versions read from a node that does not lead their range (014 FR-003, 018 FR-009): the
//! oplog and a change stream ask the range's leader for its history, and the answer crosses the
//! peers' link. Before, an answer above a frame's 16 MiB was dropped and the read failed after
//! the forward's 5 s with 89 «the range did not answer in time» (battery e2e, `replset-oplog`
//! steps 8, 9 and 11; `cs-split-large` steps 6 to 9 came back empty).

mod common;

use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

const DB: &str = "big";
/// Bytes of the string each large document carries: two versions pass a frame's 16 MiB.
const LARGE: usize = 9 << 20;
/// Longest a read of the history may take: under the forward's 5 s with room for a busy machine.
const ANSWER_WITHIN: Duration = Duration::from_secs(4);
/// `maxTimeMS` of a stream's `getMore`, as in `cs-split-large`.
const GET_MORE_MS: i32 = 3_000;

/// Three nodes of two cores, the second and third joining the first.
fn cluster() -> Vec<Node<MemDisk>> {
    let start = |join| Node::start(config(join), || Ok(MemDisk::new()), None).expect("starts");
    let first = start(Vec::new());
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    nodes.extend((0..2).map(|_| start(seed.clone())));
    until("every node ready", || {
        nodes.iter().all(|n| n.wait_ready(Duration::from_millis(10))).then_some(())
    });
    nodes
}

fn large(id: i32, fill: char) -> Document {
    Document::new().with("_id", id).with("s", fill.to_string().repeat(LARGE))
}

fn insert(c: &mut Client, coll: &str, d: Document) {
    let documents = Value::Array(vec![Value::Document(d)]);
    let r = c.run(DB, Document::new().with("insert", coll).with("documents", documents));
    assert!(ok(&r), "{r:?}");
}

/// Creates `coll` and gives the `operationTime` of the creation: the streams start there, so
/// that a stream on any node has every write after it, whatever the node's clock (the clients of
/// different nodes share no `clusterTime`).
fn create(c: &mut Client, cmd: Document) -> Value {
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    r.get("operationTime").cloned().expect("an operationTime")
}

fn replace(c: &mut Client, coll: &str, d: Document) {
    let q = Document::new().with("_id", d.get("_id").cloned().expect("an _id"));
    let u = Document::new().with("q", q).with("u", d);
    let r = c.run(DB, Document::new().with("update", coll).with("updates", vec![u.into()]));
    assert!(ok(&r), "{r:?}");
}

/// The first oplog entry of `ns` with operation `op`, and how long the read took.
fn first_entry(c: &mut Client, ns: &str, op: &str) -> (Document, Duration) {
    let filter = Document::new().with("ns", ns).with("op", op);
    let find = Document::new().with("find", "oplog.rs").with("filter", filter).with("limit", 1);
    let t = Instant::now();
    let r = c.run("local", find);
    let took = t.elapsed();
    assert!(ok(&r), "{r:?}");
    match batch(&r, "firstBatch").first() {
        Some(Value::Document(d)) => (d.clone(), took),
        other => panic!("no {op} entry of {ns}: {other:?}"),
    }
}

/// A stream on `coll` from mark `at`, and its cursor.
fn watch(c: &mut Client, coll: &str, at: &Value) -> i64 {
    let spec = Document::new().with("startAtOperationTime", at.clone());
    let stage = Document::new().with("$changeStream", spec);
    let cmd = Document::new()
        .with("aggregate", coll)
        .with("pipeline", vec![Value::Document(stage)])
        .with("cursor", Document::new());
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    match r.get("cursor") {
        Some(Value::Document(cur)) => match cur.get("id") {
            Some(Value::Int64(id)) => *id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The events of `getMore`s on cursor `id` until `n` arrived; each `getMore` answers within its
/// `maxTimeMS`.
fn events(c: &mut Client, coll: &str, id: i64, n: usize) -> Vec<Document> {
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(30);
    while got.len() < n {
        assert!(Instant::now() < end, "{} of {n} events: {got:?}", got.len());
        let more = Document::new()
            .with("getMore", Value::Int64(id))
            .with("collection", coll)
            .with("maxTimeMS", GET_MORE_MS);
        let t = Instant::now();
        let r = c.run(DB, more);
        assert!(ok(&r), "{r:?}");
        let within = Duration::from_millis(GET_MORE_MS as u64) + Duration::from_secs(1);
        assert!(t.elapsed() < within, "a getMore took {:?}", t.elapsed());
        for e in batch(&r, "nextBatch") {
            let Value::Document(e) = e else { panic!("{e:?}") };
            got.push(e);
        }
    }
    got
}

fn kinds(events: &[Document]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e.get("operationType") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Small documents and two versions of 9 MiB: every node reads the first insert and the
/// replacement of the collection from the oplog in time.
#[test]
fn every_node_reads_large_versions_from_the_oplog() {
    let nodes = cluster();
    let mut writer = Client::new(&nodes[0]);
    // Small documents first: the history the oplog reads holds them and the two large versions.
    let r = writer.run(DB, Document::new().with("insert", "c").with("documents", docs(10..13)));
    assert!(ok(&r), "{r:?}");
    insert(&mut writer, "c", large(1, 'a'));
    replace(&mut writer, "c", large(1, 'b'));
    for (k, n) in nodes.iter().enumerate() {
        let mut c = Client::new(n);
        for op in ["i", "u"] {
            let (e, took) = first_entry(&mut c, "big.c", op);
            assert!(took < ANSWER_WITHIN, "node {k}: the {op} entry took {took:?}");
            assert_eq!(e.get("op"), Some(&Value::from(op)), "node {k}: {e:?}");
        }
    }
    nodes.into_iter().for_each(|n| drop(n.stop()));
}

/// A stream on each node gets the insert and the replacement of 9 MiB written within a second,
/// each `getMore` within its `maxTimeMS`.
#[test]
fn a_stream_on_every_node_gets_large_events() {
    let nodes = cluster();
    let mut writer = Client::new(&nodes[0]);
    let at = create(&mut writer, Document::new().with("create", "s"));
    let mut streams: Vec<(Client, i64)> = nodes
        .iter()
        .map(|n| {
            let mut c = Client::new(n);
            let id = watch(&mut c, "s", &at);
            (c, id)
        })
        .collect();
    insert(&mut writer, "s", large(1, 'a'));
    replace(&mut writer, "s", large(1, 'b'));
    for (k, (c, id)) in streams.iter_mut().enumerate() {
        let got = events(c, "s", *id, 2);
        assert_eq!(kinds(&got), ["insert", "replace"], "node {k}");
    }
    nodes.into_iter().for_each(|n| drop(n.stop()));
}

fn lsid() -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![9; 16] })
}

/// Two documents of 9 MiB written by one transaction share their range's commit mark, so no
/// answer cut by marks keeps them apart: a stream on every node still gets both.
#[test]
fn a_stream_on_every_node_gets_a_transaction_s_large_writes() {
    let nodes = cluster();
    let mut writer = Client::new(&nodes[0]);
    let at = create(&mut writer, Document::new().with("create", "t"));
    let mut streams: Vec<(Client, i64)> = nodes
        .iter()
        .map(|n| {
            let mut c = Client::new(n);
            let id = watch(&mut c, "t", &at);
            (c, id)
        })
        .collect();
    let in_txn = |cmd: Document, start: bool| {
        let cmd = cmd.with("lsid", lsid()).with("txnNumber", Value::Int64(1));
        let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
        cmd.with("autocommit", false)
    };
    for (i, start) in [(1, true), (2, false)] {
        let d = large(i, 'c');
        let cmd = Document::new().with("insert", "t").with("documents", vec![Value::Document(d)]);
        let r = writer.run(DB, in_txn(cmd, start));
        assert!(ok(&r), "{r:?}");
    }
    let r = writer.run("admin", in_txn(Document::new().with("commitTransaction", 1), false));
    assert!(ok(&r), "{r:?}");
    for (k, (c, id)) in streams.iter_mut().enumerate() {
        let got = events(c, "t", *id, 2);
        assert_eq!(kinds(&got), ["insert", "insert"], "node {k}");
    }
    nodes.into_iter().for_each(|n| drop(n.stop()));
}

/// The stream of `cs-split-large` on `coll`, with `$changeStreamSplitLargeEvent` if `split`.
fn watch_images(c: &mut Client, coll: &str, at: &Value, split: bool) -> i64 {
    let spec = Document::new()
        .with("fullDocument", "updateLookup")
        .with("fullDocumentBeforeChange", "required")
        .with("startAtOperationTime", at.clone());
    let mut pipeline = vec![Value::Document(Document::new().with("$changeStream", spec))];
    if split {
        pipeline.push(Value::Document(
            Document::new().with("$changeStreamSplitLargeEvent", Document::new()),
        ));
    }
    let cmd = Document::new()
        .with("aggregate", coll)
        .with("pipeline", pipeline)
        .with("cursor", Document::new());
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    match r.get("cursor") {
        Some(Value::Document(cur)) => match cur.get("id") {
            Some(Value::Int64(id)) => *id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// One `getMore` of batch size 1 with the `maxTimeMS` of `cs-split-large`: its reply, which
/// comes within that time.
fn one_more(c: &mut Client, coll: &str, id: i64) -> Document {
    let more = Document::new()
        .with("getMore", Value::Int64(id))
        .with("collection", coll)
        .with("batchSize", 1)
        .with("maxTimeMS", GET_MORE_MS);
    let t = Instant::now();
    let r = c.run(DB, more);
    let within = Duration::from_millis(GET_MORE_MS as u64) + Duration::from_secs(1);
    assert!(t.elapsed() < within, "a getMore took {:?}", t.elapsed());
    r
}

/// The single event of a `getMore` of batch size 1.
#[track_caller]
fn the_event(r: &Document) -> Document {
    assert!(ok(r), "{r:?}");
    match batch(r, "nextBatch").as_slice() {
        [Value::Document(e)] => e.clone(),
        other => panic!("one event expected: {other:?}"),
    }
}

/// `cs-split-large` (014) on every node: an insert, an update to 8 MiB and one to 14 MB whose
/// event, with its pre-image of 8 MiB, needs two fragments. Each `getMore` of the battery gives
/// its event within its `maxTimeMS` (before, the stream on a node that did not lead the range
/// came back empty); without the stage the first `getMore` fails with 10334, as on a `refrouter`.
#[test]
fn every_node_gives_the_events_of_cs_split_large() {
    let nodes = cluster();
    let mut writer = Client::new(&nodes[0]);
    let cmd = Document::new()
        .with("create", "split")
        .with("changeStreamPreAndPostImages", Document::new().with("enabled", true));
    let at = create(&mut writer, cmd);
    let mut streams: Vec<(Client, i64, i64)> = nodes
        .iter()
        .map(|n| {
            let mut c = Client::new(n);
            let split = watch_images(&mut c, "split", &at, true);
            let whole = watch_images(&mut c, "split", &at, false);
            (c, split, whole)
        })
        .collect();
    insert(&mut writer, "split", Document::new().with("_id", 1).with("s", "x".repeat(1_024)));
    let double =
        Value::Document(Document::new().with(
            "$set",
            Document::new().with(
                "s",
                Document::new().with("$concat", vec![Value::from("$s"), Value::from("$s")]),
            ),
        ));
    let update = |c: &mut Client, u: Vec<Value>| {
        let q = Document::new().with("_id", 1);
        let upd = Document::new().with("q", q).with("u", u);
        let r =
            c.run(DB, Document::new().with("update", "split").with("updates", vec![upd.into()]));
        assert!(ok(&r), "{r:?}");
    };
    update(&mut writer, vec![double; 13]);
    let six = Value::Document(Document::new().with(
        "$set",
        Document::new().with(
            "u",
            Document::new().with(
                "$substrBytes",
                vec![Value::from("$s"), Value::Int32(0), Value::Int32(6_000_000)],
            ),
        ),
    ));
    update(&mut writer, vec![six]);
    let kind = |e: &Document| e.get("operationType").cloned();
    // The update to 8 MiB: an update or a replacement, as the node logs it.
    let changed = |e: &Document| {
        [Value::from("update"), Value::from("replace")].iter().any(|w| kind(e).as_ref() == Some(w))
    };
    for (k, (c, split, whole)) in streams.iter_mut().enumerate() {
        let first = the_event(&one_more(c, "split", *split));
        assert_eq!(kind(&first), Some(Value::from("insert")), "node {k}");
        let second = the_event(&one_more(c, "split", *split));
        assert!(changed(&second), "node {k}: {:?}", kind(&second));
        for fragment in 1..=2 {
            let e = the_event(&one_more(c, "split", *split));
            let Some(Value::Document(s)) = e.get("splitEvent") else { panic!("node {k}: {e:?}") };
            assert_eq!(s.get("fragment"), Some(&Value::Int32(fragment)), "node {k}");
        }
        // Without the stage, the three events fall in one shard batch and the large one fails
        // it: the first `getMore` fails and the cursor is gone (014 FR-011, measured on a
        // `refrouter` 9.0.4 on 04-11-2026).
        let too_large = one_more(c, "split", *whole);
        assert_eq!(too_large.get("code"), Some(&Value::Int32(10334)), "node {k}: {too_large:?}");
        let gone = one_more(c, "split", *whole);
        assert_eq!(gone.get("code"), Some(&Value::Int32(43)), "node {k}: {gone:?}");
    }
    nodes.into_iter().for_each(|n| drop(n.stop()));
}
