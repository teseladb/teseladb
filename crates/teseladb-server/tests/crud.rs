//! The CRUD and catalog commands on running nodes (006 T025–T028, FR-006, FR-014, FR-027): a
//! client on one node writes and reads the data range through its core, the catalog through
//! Group 0, and on a node that does not lead the range or Group 0 the requests reach the leaders
//! as forwarded requests.

mod common;

use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_wire::commands::Timeouts;
use teseladb_bson::{Document, Value};
use teseladb_raft::Role;
use teseladb_raftlog::MemDisk;

#[test]
fn a_single_node_runs_the_crud_commands() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("app", Document::new().with("insert", "people").with("documents", docs(0..5)));
    assert_eq!(r.get("n"), Some(&Value::Int32(5)), "{r:?}");
    assert!(r.get("$clusterTime").is_some() && r.get("operationTime").is_some());
    let r = c.run("app", Document::new().with("insert", "people").with("documents", docs(4..6)));
    assert!(matches!(r.get("writeErrors"), Some(Value::Array(e)) if e.len() == 1), "{r:?}");
    let r = c.run("app", Document::new().with("find", "people").with("batchSize", 2));
    assert_eq!(batch(&r, "firstBatch").len(), 2);
    let Some(Value::Document(cur)) = r.get("cursor") else { panic!() };
    let Some(Value::Int64(id)) = cur.get("id") else { panic!() };
    let r = c.run(
        "app",
        Document::new().with("getMore", *id).with("collection", "people").with("batchSize", 10),
    );
    assert_eq!(batch(&r, "nextBatch").len(), 3);
    let set = Document::new().with("$set", Document::new().with("v", 99));
    let upd = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    let r = c.run(
        "app",
        Document::new().with("update", "people").with("updates", vec![Value::Document(upd)]),
    );
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    let r = c.run(
        "app",
        Document::new().with("find", "people").with("filter", Document::new().with("v", 99)),
    );
    assert_eq!(batch(&r, "firstBatch").len(), 1);
    let del = Document::new()
        .with("q", Document::new().with("v", Document::new().with("$gte", 30)))
        .with("limit", 0);
    let r = c.run(
        "app",
        Document::new().with("delete", "people").with("deletes", vec![Value::Document(del)]),
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(3)), "{r:?}");
    let r = c.run("app", Document::new().with("count", "people"));
    assert_eq!(r.get("n"), Some(&Value::Int32(2)), "{r:?}");
    let r = c.run("app", Document::new().with("listCollections", 1).with("nameOnly", true));
    assert_eq!(batch(&r, "firstBatch").len(), 1);
    assert!(ok(&c.run("app", Document::new().with("drop", "people"))));
    let r = c.run("app", Document::new().with("count", "people"));
    assert_eq!(r.get("n"), Some(&Value::Int32(0)));
    let r = c.run("app", Document::new().with("nosuch", 1));
    assert_eq!(r.get("code"), Some(&Value::Int32(59)));
    drop(node.stop());
}

fn cursor_of(r: &Document) -> i64 {
    let Some(Value::Document(cur)) = r.get("cursor") else { panic!("no cursor in {r:?}") };
    let Some(Value::Int64(id)) = cur.get("id") else { panic!("no id in {r:?}") };
    *id
}

#[test]
fn the_node_reaps_idle_cursors_and_expired_sessions() {
    let mut cfg = config(Vec::new());
    if let Some(c) = cfg.client.as_mut() {
        c.timeouts = Timeouts { cursor_ms: 300, session_ms: 600 };
        c.reap_every = Duration::from_millis(50);
    }
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("app", Document::new().with("insert", "c").with("documents", docs(0..5)));
    assert_eq!(r.get("n"), Some(&Value::Int32(5)), "{r:?}");
    let find = || Document::new().with("find", "c").with("batchSize", 1);
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
    let plain = cursor_of(&c.run("app", find()));
    let kept = cursor_of(&c.run("app", find().with("noCursorTimeout", true)));
    let in_session = cursor_of(&c.run("app", find().with("lsid", lsid.clone())));
    std::thread::sleep(Duration::from_millis(1_500));
    let more = |id: i64| Document::new().with("getMore", id).with("collection", "c");
    let r = c.run("app", more(plain));
    assert_eq!(r.get("code"), Some(&Value::Int32(43)), "{r:?}");
    let r = c.run("app", more(in_session).with("lsid", lsid));
    assert_eq!(r.get("code"), Some(&Value::Int32(43)), "{r:?}");
    let r = c.run("app", more(kept));
    assert_eq!(batch(&r, "nextBatch").len(), 4, "{r:?}");
    drop(node.stop());
}

#[test]
fn every_node_of_a_cluster_takes_the_commands() {
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
    // A node that does not lead the data range forwards to its leader.
    let follower = until("a replica that follows", || {
        nodes.iter().find(|n| {
            n.data_status(Duration::from_secs(2)).is_some_and(|s| s.role == Some(Role::Follower))
        })
    });
    let mut c = Client::new(follower);
    let r = c.run("db", Document::new().with("create", "c"));
    assert!(ok(&r), "{r:?}");
    let r = c.run("db", Document::new().with("insert", "c").with("documents", docs(0..20)));
    assert_eq!(r.get("n"), Some(&Value::Int32(20)), "{r:?}");
    // Every node sees the collection and its documents.
    for n in &nodes {
        let mut c = Client::new(n);
        let r = c.run("db", Document::new().with("count", "c"));
        assert_eq!(r.get("n"), Some(&Value::Int32(20)), "{r:?}");
    }
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

#[test]
fn an_undecodable_mutation_is_refused_by_the_range() {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::ack::{W, WriteConcern};
    use teseladb_range::host::RangeError;
    use teseladb_server::data::{DataError, DataReq};
    use teseladb_server::data_wire::RemoteReq;
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let req = RemoteReq::Write {
        keys: vec![vec![1, 2]],
        mutation: vec![0xff],
        dedup: None,
        wc: WriteConcern { w: W::Nodes(1), j: false },
        deadline: None,
    };
    let r = until("the range's answer", || {
        match node.data(DataReq::Remote(DATA_RANGE, req.clone()), READY) {
            Err(DataError::Range(RangeError::NotLeader { .. })) => None,
            r => Some(r),
        }
    });
    assert!(matches!(r, Err(DataError::Range(RangeError::Mutation(_)))), "{r:?}");
    drop(node.stop());
}

/// A write or a transaction's request that reaches the range's leader after its deadline is not
/// evaluated (009 FR-006): the operation that sent it may have ended and released its read of the
/// indexes, so an index change may already count on no write with the old list.
#[test]
fn a_request_past_its_deadline_is_refused_by_the_range() {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::ack::{W, WriteConcern};
    use teseladb_range::host::{RangeError, TxnOp};
    use teseladb_range::intents::{Push, TxnId};
    use teseladb_server::data::{DataError, DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let write = RemoteReq::Write {
        keys: vec![vec![1, 2]],
        mutation: vec![0xff],
        dedup: None,
        wc: WriteConcern { w: W::Nodes(1), j: false },
        deadline: Some(1),
    };
    let push = RemoteReq::Txn {
        op: TxnOp::PushTxn {
            txn: TxnId(5),
            push: Push::AbortIfAbandoned {
                intent_ts: teseladb_hlc::Hlc { physical: 0, logical: 0 },
            },
        },
        deadline: Some(1),
    };
    for req in [write, push] {
        let r = until("the range's answer", || {
            match node.data(DataReq::Remote(DATA_RANGE, req.clone()), READY) {
                Err(DataError::Range(RangeError::NotLeader { .. })) => None,
                r => Some(r),
            }
        });
        assert!(matches!(r, Err(DataError::Range(RangeError::Timeout))), "{r:?}");
    }
    // Without a deadline the same push is evaluated: the record it leaves comes back.
    let push = RemoteReq::Txn {
        op: TxnOp::PushTxn {
            txn: TxnId(5),
            push: Push::AbortIfAbandoned {
                intent_ts: teseladb_hlc::Hlc { physical: 0, logical: 0 },
            },
        },
        deadline: None,
    };
    let r = node.data(DataReq::Remote(DATA_RANGE, push), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    drop(node.stop());
}

/// Two clients that increment the same document at once (006 T023, FR-014): each update is
/// evaluated by the range's leader on the document it holds, so none is lost.
#[test]
fn concurrent_updates_of_one_document_are_both_applied() {
    const EACH: i32 = 100;
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let doc = Document::new().with("_id", 1).with("n", 0);
    assert!(ok(&c.run(
        "app",
        Document::new().with("insert", "k").with("documents", vec![Value::Document(doc)])
    )));
    let inc = || {
        let u = Document::new()
            .with("q", Document::new().with("_id", 1))
            .with("u", Document::new().with("$inc", Document::new().with("n", 1)));
        Document::new().with("update", "k").with("updates", vec![Value::Document(u)])
    };
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                let mut c = Client::new(&node);
                for _ in 0..EACH {
                    let r = c.run("app", inc());
                    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
                }
            });
        }
    });
    let r = c.run("app", Document::new().with("find", "k"));
    let Some(Value::Document(d)) = batch(&r, "firstBatch").first().cloned() else {
        panic!("{r:?}")
    };
    assert_eq!(d.get("n"), Some(&Value::Int32(2 * EACH)));
    drop(node.stop());
}

#[test]
fn writes_that_match_nothing_in_an_empty_collection_answer_at_once() {
    // A delete, update or `findAndModify` without candidates used to send a write without keys,
    // which no range holds: it waited for the 30 s request deadline and failed with 89.
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "empty"))));
    let stmt = |pairs: &[(&str, Value)]| {
        let mut d = Document::new();
        for (k, v) in pairs {
            d.push(*k, v.clone());
        }
        Value::Array(vec![Value::Document(d)])
    };
    let all = Value::Document(Document::new());
    let set = Value::Document(Document::new().with("$set", Document::new().with("a", 1)));
    let commands = [
        Document::new()
            .with("findAndModify", "empty")
            .with("query", all.clone())
            .with("remove", true),
        Document::new()
            .with("delete", "empty")
            .with("deletes", stmt(&[("q", all.clone()), ("limit", Value::Int32(1))])),
        Document::new()
            .with("update", "empty")
            .with("updates", stmt(&[("q", all.clone()), ("u", set)])),
    ];
    for cmd in commands {
        let started = std::time::Instant::now();
        let r = c.run("app", cmd.clone());
        assert!(ok(&r) && r.get("writeErrors").is_none(), "{cmd:?}: {r:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{cmd:?} took {:?}", started.elapsed());
    }
    drop(node.stop());
}

#[test]
fn a_command_that_waits_does_not_hold_up_the_other_connections() {
    // One core, so one client worker serves every connection of the node. A catalog change that
    // cannot reach a majority of Group 0 waits for its `maxTimeMS`; meanwhile another connection
    // of the same worker gets its `ping` and `hello` answered.
    let one_core =
        |join: Vec<String>| teseladb_server::node::NodeConfig { cores: Some(1), ..config(join) };
    let first = Node::start(one_core(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2)
        .map(|_| Node::start(one_core(seed.clone()), || Ok(MemDisk::new()), None).unwrap())
        .collect();
    for n in &others {
        assert!(n.wait_ready(READY));
    }
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    let mut slow = Client::new(&first);
    let create = Document::new().with("create", "d").with("maxTimeMS", Value::Int32(4000));
    let started = std::time::Instant::now();
    slow.send("app", create);
    std::thread::sleep(Duration::from_millis(200));
    let mut quick = Client::new(&first);
    for cmd in ["ping", "hello"] {
        let asked = std::time::Instant::now();
        let r = quick.run("admin", Document::new().with(cmd, 1));
        assert!(ok(&r), "{r:?}");
        assert!(asked.elapsed() < Duration::from_millis(500), "{cmd} took {:?}", asked.elapsed());
    }
    let r = slow.receive();
    assert!(!ok(&r), "{r:?}");
    assert!(started.elapsed() >= Duration::from_secs(3), "the change did not wait: {r:?}");
    drop(first.stop());
}

/// A write outside a transaction that finds a transaction's intent (008 T032, FR-002) waits for
/// its owner, as RefDb 9.0 does: with `maxTimeMS` it ends with `MaxTimeMSExpired` (50) in
/// `writeErrors`, and once the owner is resolved the same update goes through.
#[test]
fn a_plain_write_waits_for_the_transaction_that_holds_its_document() {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId, TxnStatus, record_key};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    // The document's key, and an intent of transaction 9 on it.
    let scan =
        RemoteReq::Scan { start: vec![1], end: vec![2], limit: 10, concern: ReadConcern::Local };
    let Ok(DataReply::Remote(RemoteReply::Rows(rows))) =
        node.data(DataReq::Remote(DATA_RANGE, scan), READY)
    else {
        panic!("no rows")
    };
    let key = rows[0].0.clone();
    let txn = TxnId(9);
    // A live transaction: its record exists and has a heartbeat.
    let begin = TxnOp::EndTxn {
        txn,
        op: RecordOp::Begin {
            lsid: 1,
            txn_number: 1,
            read_ts: teseladb_hlc::Hlc { physical: 1, logical: 0 },
        },
    };
    let record = record_key(txn);
    let g = DATA_RANGE;
    assert!(record.as_slice() >= [2u8].as_slice(), "the data range holds the record");
    let r = node.data(DataReq::Remote(g, RemoteReq::Txn { op: begin, deadline: None }), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let op = IntentOp::Put { value_type: 1, value: rows[0].1.clone() };
    let put = TxnOp::PutIntents {
        txn,
        read_ts: teseladb_hlc::Hlc { physical: u64::MAX >> 1, logical: 0 },
        writes: vec![(key.clone(), op)],
    };
    let r =
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op: put, deadline: None }), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let set = Document::new().with("$set", Document::new().with("v", 7));
    let upd = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    let update = Document::new()
        .with("update", "c")
        .with("updates", vec![Value::Document(upd)])
        .with("maxTimeMS", 300);
    let start = std::time::Instant::now();
    let r = c.run("held", update.clone());
    assert!(start.elapsed() >= Duration::from_millis(250), "it did not wait: {r:?}");
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!("{r:?}") };
    assert_eq!(e.get("code"), Some(&Value::Int32(50)), "{r:?}");
    // The owner aborts and its intent goes: the update goes through.
    let resolve = TxnOp::ResolveIntents {
        txn,
        status: TxnStatus::Aborted,
        commit_ts: teseladb_hlc::Hlc { physical: 0, logical: 0 },
        keys: vec![key],
    };
    let r = node
        .data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op: resolve, deadline: None }), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let r = c.run("held", update);
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    drop(node.stop());
}

/// A plain insert that runs into an intent on another key than its document's, the entry of a
/// unique index it reads (008 FR-002, T032): the wait resolves the intent where the range found
/// it. Here the intent belongs to an aborted transaction, so the insert goes on at once and meets
/// the entry of the document that holds the value: `DuplicateKey` (11000), never a wait until
/// `maxTimeMS`.
#[test]
fn a_plain_write_resolves_the_intent_on_the_index_entry_it_read() {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let rows = || {
        let scan = RemoteReq::Scan {
            start: vec![1],
            end: vec![2],
            limit: 100,
            concern: ReadConcern::Local,
        };
        let Ok(DataReply::Remote(RemoteReply::Rows(rows))) =
            node.data(DataReq::Remote(DATA_RANGE, scan), READY)
        else {
            panic!("no rows")
        };
        rows
    };
    let one = Document::new().with("_id", 1).with("u", 5);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", vec![one.into()]));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let before = rows();
    let spec = Document::new()
        .with("key", Document::new().with("u", 1))
        .with("name", "u_1")
        .with("unique", true);
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let r = c.run("held", create);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    // The entry of `u: 5`: the one row the index added.
    let entry = rows()
        .into_iter()
        .find(|row| !before.contains(row))
        .expect("the index entry of the document");
    // Transaction 9 began, deleted the entry and was aborted, its intent left in place.
    let txn = TxnId(9);
    let ask = |op: TxnOp| {
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op, deadline: None }), READY)
    };
    let hlc = |physical| teseladb_hlc::Hlc { physical, logical: 0 };
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: hlc(1) };
    let r = ask(TxnOp::EndTxn { txn, op: begin });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let put = TxnOp::PutIntents {
        txn,
        read_ts: hlc(u64::MAX >> 1),
        writes: vec![(entry.0.clone(), IntentOp::Delete)],
    };
    let r = ask(put);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let r = ask(TxnOp::EndTxn {
        txn,
        op: RecordOp::Abort { lsid: 1, txn_number: 1, spans: Vec::new() },
    });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let two = Document::new().with("_id", 2).with("u", 5);
    let insert = Document::new()
        .with("insert", "c")
        .with("documents", vec![two.into()])
        .with("maxTimeMS", 2_000);
    let r = c.run("held", insert);
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!("{r:?}") };
    assert_eq!(e.get("code"), Some(&Value::Int32(11000)), "{r:?}");
    drop(node.stop());
}

/// The reaper of abandoned transactions (008 FR-007, T011): a transaction whose coordinator died
/// (its record pending, with no heartbeat for more than five seconds) is aborted by the node that
/// leads its record's range, and its intents are resolved, so a plain write no longer waits.
#[test]
fn an_abandoned_transaction_is_aborted_and_its_intents_resolved() {
    use std::time::Instant;
    use teseladb_hlc::Hlc;
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId, TxnRecord, TxnStatus, record_key};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let try_scan = |start: Vec<u8>, end: Vec<u8>| {
        let req = RemoteReq::Scan { start, end, limit: 10, concern: ReadConcern::Local };
        match node.data(DataReq::Remote(DATA_RANGE, req), READY) {
            Ok(DataReply::Remote(RemoteReply::Rows(rows))) => Some(rows),
            _ => None,
        }
    };
    let scan = |start, end| try_scan(start, end).expect("rows");
    let (key, value) = scan(vec![1], vec![2]).remove(0);
    // Transaction 9 began and wrote an intent on the document; its coordinator died.
    let txn = TxnId(9);
    let ask = |op: TxnOp| {
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op, deadline: None }), READY)
    };
    let hlc = |physical| Hlc { physical, logical: 0 };
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: hlc(1) };
    let r = ask(TxnOp::EndTxn { txn, op: begin });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let op = IntentOp::Put { value_type: 1, value };
    let writes = vec![(key.clone(), op)];
    let r = ask(TxnOp::PutIntents { txn, read_ts: hlc(u64::MAX >> 1), writes });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    // Its one heartbeat told the record which keys have intents.
    let spans = vec![(key.clone(), [key.as_slice(), &[0]].concat())];
    let r = ask(TxnOp::EndTxn { txn, op: RecordOp::Heartbeat { spans } });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    // A read meets the intent of a transaction it knows nothing about.
    assert_eq!(try_scan(vec![1], vec![2]), None);
    let started = Instant::now();
    let record = until("the reaper aborts the record", || {
        let rows = scan(record_key(txn), vec![3]);
        let (_, stored) = rows.first()?;
        let rec = TxnRecord::decode_row(stored).expect("a record");
        (rec.status == TxnStatus::Aborted).then_some(rec)
    });
    assert!(started.elapsed() >= Duration::from_secs(4), "aborted before it was abandoned");
    assert_eq!(record.txn, txn);
    // The reaper resolved its intent: the read finds the document.
    until("the reaper resolves the intent", || try_scan(vec![1], vec![2]));
    // A plain update goes through without waiting.
    let set = Document::new().with("$set", Document::new().with("v", 7));
    let upd = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    let update = Document::new()
        .with("update", "c")
        .with("updates", vec![Value::Document(upd)])
        .with("maxTimeMS", 200);
    let r = c.run("held", update);
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    drop(node.stop());
}

/// A read that meets the intent of a live transaction (008 FR-003, T013) does not wait: it pushes
/// the transaction above its mark and reads the version below the intent, as a read outside a
/// transaction in RefDb 9.0 sees the last committed document.
#[test]
fn a_read_pushes_a_live_transaction_and_reads_below_its_intent() {
    use std::time::Instant;
    use teseladb_hlc::Hlc;
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId, TxnRecord, TxnStatus, record_key};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let scan = |start: Vec<u8>, end: Vec<u8>| {
        let req = RemoteReq::Scan { start, end, limit: 10, concern: ReadConcern::Local };
        match node.data(DataReq::Remote(DATA_RANGE, req), READY) {
            Ok(DataReply::Remote(RemoteReply::Rows(rows))) => rows,
            other => panic!("no rows: {other:?}"),
        }
    };
    let (key, _) = scan(vec![1], vec![2]).remove(0);
    // Transaction 9, alive, deletes the document.
    let txn = TxnId(9);
    let ask = |op: TxnOp| {
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op, deadline: None }), READY)
    };
    let hlc = |physical| Hlc { physical, logical: 0 };
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: hlc(1) };
    let r = ask(TxnOp::EndTxn { txn, op: begin });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let writes = vec![(key, IntentOp::Delete)];
    let r = ask(TxnOp::PutIntents { txn, read_ts: hlc(u64::MAX >> 1), writes });
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let start = Instant::now();
    let r = c.run("held", Document::new().with("find", "c").with("maxTimeMS", 2_000));
    assert!(start.elapsed() < Duration::from_millis(1_500), "it waited: {r:?}");
    assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    // The read pushed the transaction by reading its record (008 T043): the record is as it was,
    // and a commit with the coordinator's mark far below the read goes above it.
    let rows = scan(record_key(txn), vec![3]);
    let rec = TxnRecord::decode_row(&rows[0].1).expect("a record");
    assert_eq!((rec.status, rec.min_commit_ts), (TxnStatus::Pending, hlc(0)), "{rec:?}");
    let read_at = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis(),
    )
    .expect("a time")
        - 1_500;
    let commit = RecordOp::Commit { ts: hlc(2), spans: Vec::new() };
    let Ok(DataReply::Remote(RemoteReply::Record(done))) = ask(TxnOp::EndTxn { txn, op: commit })
    else {
        panic!("no record")
    };
    assert_eq!(done.status, TxnStatus::Committed);
    assert!(done.commit_ts > hlc(read_at), "the commit went below the read: {done:?}");
    drop(node.stop());
}

/// What a plain insert answers when a transaction holds an intent that adds the entry of a unique
/// index for its value (009 FR-004, T032): it waits for the transaction and decides with its end,
/// `DuplicateKey` (11000) if it committed and the insert if it aborted, never a false
/// `DuplicateKey` for a value nobody keeps.
fn insert_after_pending_unique_entry(commit: bool) -> Document {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    use teseladb_wire::commands::index_maintenance::INDEX_ENTRY;
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let rows = || {
        let scan = RemoteReq::Scan {
            start: vec![1],
            end: vec![2],
            limit: 100,
            concern: ReadConcern::Local,
        };
        let Ok(DataReply::Remote(RemoteReply::Rows(rows))) =
            node.data(DataReq::Remote(DATA_RANGE, scan), READY)
        else {
            panic!("no rows")
        };
        rows
    };
    // The entry of `u: 7` as the index writes it (the one row the index adds), then gone with
    // its document.
    let one = Document::new().with("_id", 1).with("u", 7);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", vec![one.into()]));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let before = rows();
    let spec = Document::new()
        .with("key", Document::new().with("u", 1))
        .with("name", "u_1")
        .with("unique", true);
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let r = c.run("held", create);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let entry = rows().into_iter().find(|row| !before.contains(row)).expect("the index entry");
    let deletes = vec![Value::Document(
        Document::new().with("q", Document::new().with("_id", 1)).with("limit", 1),
    )];
    let r = c.run("held", Document::new().with("delete", "c").with("deletes", deletes));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    // The cluster time after the delete, at or above its mark: the node's clock may run ahead of
    // the wall clock (a new leader's starts 250 ms ahead), so the transaction reads there.
    let Some(Value::Timestamp { time, increment }) = r.get("operationTime").cloned() else {
        panic!("{r:?}")
    };
    let now = teseladb_hlc::from_timestamp((time, increment));
    assert!(!rows().iter().any(|(k, _)| k == &entry.0), "the entry went with its document");
    // Transaction 9 adds that entry again and ends after the insert has started waiting.
    let txn = TxnId(9);
    let ask = |op: TxnOp| {
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op, deadline: None }), READY)
    };
    let hlc = |physical| teseladb_hlc::Hlc { physical, logical: 0 };
    let begin = RecordOp::Begin { lsid: 1, txn_number: 1, read_ts: now };
    assert!(matches!(ask(TxnOp::EndTxn { txn, op: begin }), Ok(DataReply::Remote(_))));
    let put = TxnOp::PutIntents {
        txn,
        read_ts: now,
        writes: vec![(entry.0.clone(), IntentOp::Put { value_type: INDEX_ENTRY, value: entry.1 })],
    };
    let r = ask(put);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let spans = vec![(entry.0.clone(), [entry.0.as_slice(), &[0]].concat())];
    let end = if commit {
        RecordOp::Commit { ts: hlc(now.physical + 1), spans }
    } else {
        RecordOp::Abort { lsid: 1, txn_number: 1, spans }
    };
    let r = std::thread::scope(|s| {
        let ender = s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ask(TxnOp::EndTxn { txn, op: end })
        });
        let two = Document::new().with("_id", 2).with("u", 7);
        let insert = Document::new()
            .with("insert", "c")
            .with("documents", vec![two.into()])
            .with("maxTimeMS", 5_000);
        let r = c.run("held", insert);
        assert!(matches!(ender.join().expect("the ender"), Ok(DataReply::Remote(_))));
        r
    });
    drop(node.stop());
    r
}

#[test]
fn a_unique_insert_waits_for_a_transaction_that_aborts_and_goes_on() {
    let r = insert_after_pending_unique_entry(false);
    assert_eq!((r.get("n"), r.get("writeErrors")), (Some(&Value::Int32(1)), None), "{r:?}");
}

#[test]
fn a_unique_insert_waits_for_a_transaction_that_commits_and_is_a_duplicate() {
    let r = insert_after_pending_unique_entry(true);
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!("{r:?}") };
    assert_eq!(e.get("code"), Some(&Value::Int32(11000)), "{r:?}");
}

/// Inserts of the same values of a unique index from every node of a cluster at once (009 FR-004,
/// T009): each value ends in one document, and `null` and a missing field are one value.
#[test]
fn concurrent_unique_inserts_from_every_node_keep_one_document_per_value() {
    const VALUES: i32 = 15;
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
    let spec = Document::new()
        .with("key", Document::new().with("u", 1))
        .with("name", "u_1")
        .with("unique", true);
    let create = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let r = c.run("uniq", create);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let inserted: Vec<i32> = std::thread::scope(|s| {
        let workers: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(k, n)| {
                s.spawn(move || {
                    let mut c = Client::new(n);
                    let mut won = 0;
                    for i in 0..=VALUES {
                        let id = i32::try_from(k).expect("three nodes") * 1000 + i;
                        // The last document of each node has no `u`, or `u: null`.
                        let doc = match i {
                            VALUES if k == 0 => Document::new().with("_id", id),
                            VALUES => Document::new().with("_id", id).with("u", Value::Null),
                            _ => Document::new().with("_id", id).with("u", i),
                        };
                        let r = c.run(
                            "uniq",
                            Document::new().with("insert", "c").with("documents", vec![doc.into()]),
                        );
                        match (r.get("n"), r.get("writeErrors")) {
                            (Some(Value::Int32(1)), None) => won += 1,
                            (Some(Value::Int32(0)), Some(Value::Array(e))) => {
                                let Value::Document(e) = &e[0] else { panic!("{r:?}") };
                                assert_eq!(e.get("code"), Some(&Value::Int32(11000)), "{r:?}");
                            }
                            _ => panic!("{r:?}"),
                        }
                    }
                    won
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().expect("a worker")).collect()
    });
    assert_eq!(inserted.iter().sum::<i32>(), VALUES + 1, "{inserted:?}");
    let r = c.run("uniq", Document::new().with("count", "c"));
    assert_eq!(r.get("n"), Some(&Value::Int32(VALUES + 1)), "{r:?}");
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

/// An aggregation that works in memory stops at its `maxTimeMS` (006 FR-012): a `$lookup` of
/// every document with every other reads its collection once, and then it and the `$unwind` after
/// it spread a million pairs without reading again, which ran past the limit before the stages
/// asked the operation whether to stop.
#[test]
fn an_aggregation_in_memory_stops_at_its_max_time_ms() {
    const DOCUMENTS: i32 = 1_000;
    const MAX_TIME_MS: i32 = 50;
    const MAX_TIME_MS_EXPIRED: i32 = 50;
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let rows: Vec<Value> =
        (0..DOCUMENTS).map(|n| Value::Document(Document::new().with("_id", n))).collect();
    assert!(ok(&c.run("joins", Document::new().with("insert", "c").with("documents", rows))));
    let lookup = Document::new().with(
        "$lookup",
        Document::new().with("from", "c").with("pipeline", Vec::<Value>::new()).with("as", "all"),
    );
    let pipeline = vec![
        Value::Document(lookup),
        Value::Document(Document::new().with("$unwind", "$all")),
        Value::Document(Document::new().with("$count", "pairs")),
    ];
    let aggregate = Document::new()
        .with("aggregate", "c")
        .with("pipeline", pipeline)
        .with("cursor", Document::new())
        .with("maxTimeMS", MAX_TIME_MS);
    let started = std::time::Instant::now();
    let r = c.run("joins", aggregate);
    assert_eq!(r.get("code"), Some(&Value::Int32(MAX_TIME_MS_EXPIRED)), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "it ran {:?}", started.elapsed());
    drop(node.stop());
}

/// The reads of users and roles of a command see what any node wrote before them (013 T058, FR-009), as
/// a `refrouter` reads them from the config servers.
/// Rounds of a write on one node and reads on the others: the copies of Group 0 apply each entry within
/// milliseconds, so one round rarely shows a stale read and a few dozen do.
const PRINCIPAL_ROUNDS: usize = 40;

fn roles_of(r: &Document) -> usize {
    match r.get("roles") {
        Some(Value::Array(a)) => a.len(),
        _ => panic!("no roles in {r:?}"),
    }
}

fn users_of(r: &Document) -> usize {
    match r.get("users") {
        Some(Value::Array(a)) => a.len(),
        _ => panic!("no users in {r:?}"),
    }
}

#[test]
fn a_role_or_a_user_written_on_one_node_is_read_at_once_on_the_others() {
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
    let mut clients: Vec<Client> = nodes.iter().map(|n| Client::new(n)).collect();
    for round in 0..PRINCIPAL_ROUNDS {
        let writer = round % clients.len();
        let role = format!("r{round}");
        let user = format!("u{round}");
        let created = clients[writer].run(
            "db",
            Document::new()
                .with("createRole", role.as_str())
                .with("privileges", Value::Array(Vec::new()))
                .with("roles", Value::Array(Vec::new())),
        );
        assert!(ok(&created), "{created:?}");
        let created = clients[writer].run(
            "db",
            Document::new()
                .with("createUser", user.as_str())
                .with("pwd", "a-password-of-the-test")
                .with("roles", Value::Array(Vec::new())),
        );
        assert!(ok(&created), "{created:?}");
        for (i, c) in clients.iter_mut().enumerate() {
            let r = c.run("db", Document::new().with("rolesInfo", role.as_str()));
            assert_eq!(roles_of(&r), 1, "round {round}, node {i}: {r:?}");
            let r = c.run("db", Document::new().with("usersInfo", user.as_str()));
            assert_eq!(users_of(&r), 1, "round {round}, node {i}: {r:?}");
        }
    }
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

/// Readers that meet a live transaction's intent push it by reading its record (008 T042, T043):
/// they all read the version below the intent and the range's log does not grow. Before, each
/// push was a Raft proposal that raised the record's `min_commit_ts`.
#[test]
fn readers_that_meet_a_pending_intent_write_nothing_to_the_range() {
    use teseladb_meta::bootstrap::DATA_RANGE;
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{IntentOp, RecordOp, TxnId};
    use teseladb_range::read::ReadConcern;
    use teseladb_server::data::{DataReply, DataReq};
    use teseladb_server::data_wire::{RemoteReply, RemoteReq};
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("held", Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let scan =
        RemoteReq::Scan { start: vec![1], end: vec![2], limit: 10, concern: ReadConcern::Local };
    let Ok(DataReply::Remote(RemoteReply::Rows(rows))) =
        node.data(DataReq::Remote(DATA_RANGE, scan), READY)
    else {
        panic!("no rows")
    };
    let txn = TxnId(11);
    let begin = TxnOp::EndTxn {
        txn,
        op: RecordOp::Begin {
            lsid: 1,
            txn_number: 1,
            read_ts: teseladb_hlc::Hlc { physical: 1, logical: 0 },
        },
    };
    let r =
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op: begin, deadline: None }), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Record(_)))), "{r:?}");
    let op = IntentOp::Put { value_type: 3, value: rows[0].1.clone() };
    let put = TxnOp::PutIntents {
        txn,
        read_ts: teseladb_hlc::Hlc { physical: u64::MAX >> 1, logical: 0 },
        writes: vec![(rows[0].0.clone(), op)],
    };
    let r =
        node.data(DataReq::Remote(DATA_RANGE, RemoteReq::Txn { op: put, deadline: None }), READY);
    assert!(matches!(r, Ok(DataReply::Remote(RemoteReply::Intents { .. }))), "{r:?}");
    let applied = || node.data_status(READY).map(|s| s.applied).unwrap_or_default();
    let before = applied();
    for _ in 0..16 {
        let r = c.run("held", Document::new().with("find", "c").with("filter", Document::new()));
        assert_eq!(batch(&r, "firstBatch").len(), 1, "the version below the intent: {r:?}");
    }
    assert_eq!(applied(), before, "the pushes wrote to the range");
    drop(node.stop());
}
