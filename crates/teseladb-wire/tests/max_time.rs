//! `maxTimeMS`, `defaultMaxTimeMS`, `killOp` and `currentOp` (FR-020, FR-023), with the errors and
//! messages a `refrouter` 9.0.4 gives (measured on 30-10-2026, the default with a user without
//! `bypassDefaultMaxTimeMS` on a 9.0.4 replica set with `--auth`).

mod common;

use std::sync::{Arc, Mutex};

use common::{Mem, doc, field};
use teseladb_bson::{Decimal128, Document, Value};
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::{NodeSessions, Shared, Timeouts, lock};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;
use teseladb_wire::ops::{
    DEFAULT_MAX_TIME_MS, Guarded, MAX_TIME_MS, Op, OpTable, READ_OPERATIONS, default_read_ms,
    effective_limit, max_time_ms, op_ns, op_type,
};

const DB: &str = "t";
const COLL: &str = "c";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

fn request(cmd: Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(9);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

/// Runs `cmd` on `db` through the dispatch, with the node's tables in `shared`.
fn run_on(
    m: &mut Mem,
    shared: &Shared,
    conn: &mut Connection,
    db: &str,
    cmd: Document,
) -> Document {
    match dispatch_with(conn, &request(cmd.with("$db", db)), &info(), Some((m, shared))) {
        Action::Reply(mut d) => {
            d.remove("$clusterTime");
            d.remove("operationTime");
            d
        }
        other => panic!("{other:?}"),
    }
}

fn run(m: &mut Mem, conn: &mut Connection, db: &str, cmd: Document) -> Document {
    let shared = std::mem::take(&mut m.shared);
    let r = run_on(m, &shared, conn, db, cmd);
    m.shared = shared;
    r
}

/// A node with `n` documents in `t.c`.
fn node(n: i32) -> Mem {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    let docs: Vec<Value> =
        (0..n).map(|i| Value::Document(doc(&[("_id", Value::Int32(i))]))).collect();
    let r = match docs.is_empty() {
        true => m.run(DB, Document::new().with("create", COLL)),
        false => {
            m.run(DB, Document::new().with("insert", COLL).with("documents", Value::Array(docs)))
        }
    };
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m
}

fn error(d: &Document) -> (i32, String) {
    let code = match d.get("code") {
        Some(Value::Int32(c)) => *c,
        other => panic!("no code: {other:?} in {d:?}"),
    };
    let msg = match d.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    };
    (code, msg)
}

fn ok(d: &Document) -> bool {
    d.get("ok") == Some(&Value::Double(1.0))
}

fn find(limit: Value) -> Document {
    Document::new().with("find", COLL).with(MAX_TIME_MS, limit)
}

#[test]
fn max_time_ms_reads_numbers_as_refdb_does() {
    let cmd = |v: Value| Document::new().with("find", COLL).with(MAX_TIME_MS, v);
    assert_eq!(max_time_ms(&Document::new(), "find").unwrap(), None);
    assert_eq!(max_time_ms(&cmd(Value::Null), "find").unwrap(), None);
    assert_eq!(max_time_ms(&cmd(Value::Int32(0)), "find").unwrap(), Some(0));
    assert_eq!(max_time_ms(&cmd(Value::Int64(5)), "find").unwrap(), Some(5));
    assert_eq!(max_time_ms(&cmd(Value::Double(-0.0)), "find").unwrap(), Some(0));
    assert_eq!(max_time_ms(&cmd(Value::Double(7.0)), "find").unwrap(), Some(7));
    let three = Value::Decimal128(Decimal128::from_parts(false, 3, 0).unwrap());
    assert_eq!(max_time_ms(&cmd(three), "find").unwrap(), Some(3));
    assert_eq!(max_time_ms(&cmd(Value::Int32(i32::MAX)), "find").unwrap(), Some(2_147_483_647));
    let cases: [(Value, &str, i32, &str); 11] = [
        (
            Value::Int32(-1),
            "find",
            2,
            "BSON field 'maxTimeMS' value must be >= 0, actual value '-1'",
        ),
        (
            Value::Int64(-2),
            "insert",
            2,
            "BSON field 'maxTimeMS' value must be >= 0, actual value '-2'",
        ),
        (
            Value::Int64(2_147_483_648),
            "find",
            2,
            "BSON field 'maxTimeMS' value must be <= 2147483647, actual value '2147483648'",
        ),
        (Value::Double(1.5), "find", 9, "Expected an integer: maxTimeMS: 1.5"),
        (
            Value::Double(f64::NAN),
            "find",
            9,
            "Expected an integer, but found NaN in: maxTimeMS: nan",
        ),
        (Value::Double(1e20), "find", 9, "Cannot represent as a 64-bit integer: maxTimeMS: 1e+20"),
        (
            Value::Double(-1e20),
            "find",
            9,
            "Cannot represent as a 64-bit integer: maxTimeMS: -1e+20",
        ),
        (
            Value::Decimal128(Decimal128::from_parts(false, 25, -1).unwrap()),
            "find",
            9,
            "Cannot represent as a 64-bit integer: maxTimeMS: 2.5",
        ),
        (
            Value::from("x"),
            "find",
            14,
            "BSON field 'find.maxTimeMS' is the wrong type 'string', expected types '[long, decimal, double, int]'",
        ),
        (
            Value::Boolean(true),
            "ping",
            14,
            "BSON field 'ping.maxTimeMS' is the wrong type 'bool', expected types '[int, double, long, decimal]'",
        ),
        (
            Value::from("x"),
            "serverStatus",
            14,
            "BSON field 'serverStatus.maxTimeMS' is the wrong type 'string', expected types '[decimal, double, int, long]'",
        ),
    ];
    for (v, name, code, msg) in cases {
        let e = max_time_ms(&cmd(v), name).unwrap_err();
        assert_eq!((e.code.code, e.errmsg.as_str()), (code, msg));
    }
}

#[test]
fn every_command_has_its_measured_type_order() {
    let orders = [
        ("insert", "[long, double, int, decimal]"),
        ("count", "[decimal, long, double, int]"),
        ("bulkWrite", "[int, double, long, decimal]"),
        ("endSessions", "[double, long, decimal, int]"),
        ("hostInfo", "[long, decimal, int, double]"),
        ("getMore", "[int, decimal, long, double]"),
        ("killCursors", "[decimal, int, double, long]"),
        ("create", "[double, int, long, decimal]"),
        ("drop", "[int, double, decimal, long]"),
        ("renameCollection", "[double, decimal, int, long]"),
        ("listDatabases", "[double, decimal, long, int]"),
        ("hello", "[decimal, double, int, long]"),
    ];
    for (name, order) in orders {
        let c = Document::new().with(name, 1).with(MAX_TIME_MS, "x");
        let e = max_time_ms(&c, name).unwrap_err();
        assert!(e.errmsg.ends_with(&format!("expected types '{order}'")), "{name}: {}", e.errmsg);
    }
}

#[test]
fn every_command_checks_its_max_time_ms() {
    let mut m = node(1);
    let mut conn = Connection::new(1);
    for cmd in [
        Document::new().with("ping", 1),
        Document::new().with("hello", 1),
        Document::new().with("insert", COLL).with("documents", Value::Array(vec![])),
    ] {
        let r = run(&mut m, &mut conn, DB, cmd.with(MAX_TIME_MS, -5));
        assert_eq!(error(&r).0, 2, "{r:?}");
    }
}

#[test]
fn a_find_past_its_time_limit_fails_and_no_limit_is_zero() {
    let mut m = node(3000);
    m.tick = 20;
    let mut conn = Connection::new(1);
    let r = run(&mut m, &mut conn, DB, find(Value::Int32(15)));
    assert_eq!(error(&r), (50, "operation exceeded time limit".to_owned()));
    assert_eq!(r.get("codeName"), Some(&Value::from("MaxTimeMSExpired")));
    assert!(ok(&run(&mut m, &mut conn, DB, find(Value::Int32(0)))));
    assert!(ok(&run(&mut m, &mut conn, DB, find(Value::Int32(100_000)))));
    assert_eq!(m.deadline, None, "the deadline ends with the command");
    assert!(lock(&m.shared).ops.iter().next().is_none(), "the operation left the table");
}

#[test]
fn writes_past_their_time_limit_fail_statement_by_statement() {
    let mut m = node(3);
    m.tick = 10;
    let mut conn = Connection::new(1);
    let filter = Document::new();
    let del = |i: i32| {
        Value::Document(doc(&[("q", Value::Document(filter.clone())), ("limit", Value::Int32(i))]))
    };
    let cmd = Document::new()
        .with("delete", COLL)
        .with("deletes", Value::Array(vec![del(0), del(0)]))
        .with("ordered", false)
        .with(MAX_TIME_MS, 1);
    let r = run(&mut m, &mut conn, DB, cmd);
    assert!(ok(&r), "{r:?}");
    let errors = match field(&r, "writeErrors") {
        Value::Array(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(errors.len(), 2, "{r:?}");
    for e in &errors {
        let Value::Document(e) = e else { panic!() };
        assert_eq!(e.get("code"), Some(&Value::Int32(50)));
        assert_eq!(e.get("errmsg"), Some(&Value::from("operation exceeded time limit")));
    }
}

#[test]
fn the_default_limit_applies_to_reads_of_clients_without_bypass() {
    let mut m = node(3000);
    m.tick = 20;
    let value = Document::new().with(READ_OPERATIONS, 15).to_bytes().unwrap();
    m.params.insert(DEFAULT_MAX_TIME_MS.to_owned(), value);
    let mut conn = Connection::new(1);
    let plain = |name: &str| Document::new().with(name, COLL);
    // Every client holds `bypassDefaultMaxTimeMS` without authentication.
    assert!(ok(&run(&mut m, &mut conn, DB, plain("find"))));
    conn.bypass_default_max_time = false;
    for name in ["find", "count"] {
        let r = run(&mut m, &mut conn, DB, plain(name));
        assert_eq!(error(&r).0, 50, "{name}: {r:?}");
    }
    let distinct = plain("distinct").with("key", "x");
    assert_eq!(error(&run(&mut m, &mut conn, DB, distinct)).0, 50);
    // An explicit limit, also 0, wins over the default.
    assert!(ok(&run(&mut m, &mut conn, DB, find(Value::Int32(0)))));
    assert!(ok(&run(&mut m, &mut conn, DB, find(Value::Int32(1_000_000)))));
    // Writes are not reads.
    let upd = Document::new().with("update", COLL).with(
        "updates",
        Value::Array(vec![Value::Document(doc(&[
            ("q", Value::Document(Document::new())),
            (
                "u",
                Value::Document(doc(&[("$set", Value::Document(doc(&[("y", Value::Int32(1))])))])),
            ),
            ("multi", Value::Boolean(true)),
        ]))]),
    );
    let r = run(&mut m, &mut conn, DB, upd);
    assert_eq!(r.get("nModified"), Some(&Value::Int32(3000)), "{r:?}");
}

#[test]
fn the_effective_limit() {
    assert_eq!(effective_limit("find", Some(0), 5, false), None);
    assert_eq!(effective_limit("find", Some(9), 5, false), Some(9));
    assert_eq!(effective_limit("find", None, 5, false), Some(5));
    assert_eq!(effective_limit("find", None, 5, true), None);
    assert_eq!(effective_limit("find", None, 0, false), None);
    assert_eq!(effective_limit("insert", None, 5, false), None);
    let mut p = std::collections::BTreeMap::new();
    assert_eq!(default_read_ms(&p), 0);
    p.insert(
        DEFAULT_MAX_TIME_MS.to_owned(),
        Document::new().with(READ_OPERATIONS, 7).to_bytes().unwrap(),
    );
    assert_eq!(default_read_ms(&p), 7);
    p.insert(DEFAULT_MAX_TIME_MS.to_owned(), vec![1, 2]);
    assert_eq!(default_read_ms(&p), 0);
}

#[test]
fn kill_op_interrupts_a_running_find() {
    let mut m = node(3000);
    let shared: Arc<Shared> = Arc::new(Mutex::new(NodeSessions::new(Timeouts::default())));
    let killer = Arc::clone(&shared);
    m.on_scan = Some(Box::new(move || {
        let ids: Vec<u32> = lock(&killer).ops.iter().map(|(id, _)| id).collect();
        for id in ids {
            lock(&killer).ops.kill(id);
        }
    }));
    let mut conn = Connection::new(1);
    let r = run_on(&mut m, &shared, &mut conn, DB, Document::new().with("find", COLL));
    assert_eq!(
        error(&r),
        (
            11601,
            "Executor error during find command: t.c :: caused by :: operation was interrupted"
                .to_owned()
        )
    );
    assert_eq!(r.get("codeName"), Some(&Value::from("Interrupted")));
    let r = run_on(&mut m, &shared, &mut conn, DB, Document::new().with("count", COLL));
    assert_eq!(error(&r), (11601, "operation was interrupted".to_owned()));
}

fn op(conn: i32) -> Op {
    Op {
        conn,
        client: Some("127.0.0.1:5000".to_owned()),
        metadata: Some(doc(&[("driver", Value::Document(doc(&[("name", Value::from("x"))])))])),
        app_name: Some("app".to_owned()),
        op: op_type("find"),
        ns: "t.c".to_owned(),
        command: Document::new().with("find", COLL).with("lsid", Value::Document(Document::new())),
        started_ms: 1_700_000_000_000 - 2_500,
        deadline_ms: None,
        killed: false,
        user: None,
        tenant: 0,
        privileges: None,
        roles: None,
        desc: None,
        progress: None,
    }
}

#[test]
fn current_op_lists_and_filters_the_node_operations() {
    let mut m = node(0);
    let id = lock(&m.shared).ops.begin(op(7));
    let mut conn = Connection::new(1);
    let r =
        run(&mut m, &mut conn, "admin", Document::new().with("currentOp", 1).with("$all", true));
    let inprog = match field(&r, "inprog") {
        Value::Array(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    // The `currentOp` itself is running too.
    assert_eq!(inprog.len(), 2, "{r:?}");
    let Value::Document(first) = &inprog[0] else { panic!() };
    let keys: Vec<&str> = first.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            "shard",
            "type",
            "host",
            "desc",
            "connectionId",
            "client_s",
            "appName",
            "clientMetadata",
            "active",
            "currentOpTime",
            "opid",
            "lsid",
            "secs_running",
            "microsecs_running",
            "op",
            "ns",
            "command",
            "numYields",
            "locks",
            "waitingForLock"
        ]
    );
    // As a `refrouter` names a shard's operation (measured, DV-031, D-102).
    assert_eq!(first.get("shard"), Some(&Value::from("node1")));
    assert_eq!(first.get("opid"), Some(&Value::from(format!("node1:{id}"))));
    assert_eq!(first.get("secs_running"), Some(&Value::Int64(2)));
    assert_eq!(first.get("currentOpTime"), Some(&Value::from("2023-11-14T22:13:20.000+00:00")));
    assert_eq!(first.get("op"), Some(&Value::from("query")));
    let filtered = Document::new().with("currentOp", 1).with("op", "query");
    let r = run(&mut m, &mut conn, "admin", filtered);
    assert!(matches!(field(&r, "inprog"), Value::Array(a) if a.len() == 1), "{r:?}");
    let bad = Document::new()
        .with("currentOp", 1)
        .with("op", Value::Document(doc(&[("$bad", Value::Int32(1))])));
    assert_eq!(error(&run(&mut m, &mut conn, "admin", bad)).0, 2);
    let r = run(&mut m, &mut conn, DB, Document::new().with("currentOp", 1));
    assert_eq!(error(&r), (13, "currentOp may only be run against the admin database.".to_owned()));
    // A killed operation shows it.
    lock(&m.shared).ops.kill(id);
    let r =
        run(&mut m, &mut conn, "admin", Document::new().with("currentOp", 1).with("op", "query"));
    let Value::Array(a) = field(&r, "inprog") else { panic!() };
    let Value::Document(d) = &a[0] else { panic!() };
    assert_eq!(d.get("killPending"), Some(&Value::Boolean(true)));
}

/// `currentOp` and `$currentOp` list the operations of every node, this one's first, and the
/// filter applies to all of them (010 T033, FR-016).
#[test]
fn current_op_lists_the_operations_of_every_node() {
    let mut m = node(0);
    let mut conn = Connection::new(1);
    let other = |node: &str, op: &str| {
        Document::new()
            .with("shard", node)
            .with("type", "op")
            .with("opid", format!("{node}:3"))
            .with("op", op)
    };
    m.other_ops = vec![other("node2", "query"), other("node3", "insert")];
    let r =
        run(&mut m, &mut conn, "admin", Document::new().with("currentOp", 1).with("$all", true));
    let Value::Array(all) = field(&r, "inprog") else { panic!("{r:?}") };
    let shards: Vec<Option<&Value>> = all
        .iter()
        .map(|d| match d {
            Value::Document(d) => d.get("shard"),
            _ => None,
        })
        .collect();
    let (n1, n2, n3) = (Value::from("node1"), Value::from("node2"), Value::from("node3"));
    assert_eq!(shards, [Some(&n1), Some(&n2), Some(&n3)]);
    let r =
        run(&mut m, &mut conn, "admin", Document::new().with("currentOp", 1).with("op", "insert"));
    let Value::Array(some) = field(&r, "inprog") else { panic!("{r:?}") };
    assert_eq!(some, &vec![Value::Document(other("node3", "insert"))]);
    let agg = Document::new()
        .with("aggregate", 1)
        .with(
            "pipeline",
            vec![
                Value::Document(Document::new().with("$currentOp", Document::new())),
                Value::Document(
                    Document::new().with("$match", Document::new().with("shard", "node2")),
                ),
            ],
        )
        .with("cursor", Document::new());
    let r = run(&mut m, &mut conn, "admin", agg);
    let Value::Document(cursor) = field(&r, "cursor") else { panic!("{r:?}") };
    assert_eq!(
        cursor.get("firstBatch"),
        Some(&Value::Array(vec![Value::Document(other("node2", "query"))]))
    );
}

#[test]
fn kill_op_reads_its_op_as_a_refrouter_does() {
    let mut m = node(0);
    let id = lock(&m.shared).ops.begin(op(7));
    let mut conn = Connection::new(1);
    let kill = |v: Value| Document::new().with("killOp", 1).with("op", v);
    let cases: [(Value, i32, &str); 11] = [
        (
            Value::from("x"),
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \"x\"",
        ),
        (
            Value::from(":5"),
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \":5\"",
        ),
        (
            Value::from("a:"),
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \"a:\"",
        ),
        (
            Value::Double(-1e20),
            2,
            "Expected field \"op\" to have a value exactly representable as a 64-bit integer, but found op: -1e+20",
        ),
        (Value::from("abc:def"), 70, "Shard abc not found"),
        (Value::from("1:2:3"), 70, "Shard 1 not found"),
        (
            Value::Double(1.5),
            2,
            "Expected field \"op\" to have a value exactly representable as a 64-bit integer, but found op: 1.5",
        ),
        (Value::Document(Document::new()), 50760, "\"op\" field was of unsupported type object"),
        (Value::Null, 50760, "\"op\" field was of unsupported type null"),
        (
            Value::Int64(2_147_483_648),
            26823,
            "invalid op : 2147483648. Op ID cannot be represented with 32 bits",
        ),
        (
            Value::Int64(-2_147_483_649),
            26823,
            "invalid op : -2147483649. Op ID cannot be represented with 32 bits",
        ),
    ];
    for (v, code, msg) in cases {
        assert_eq!(error(&run(&mut m, &mut conn, "admin", kill(v))), (code, msg.to_owned()));
    }
    let r = run(&mut m, &mut conn, "admin", Document::new().with("killOp", 1));
    assert_eq!(error(&r), (50759, "Did not provide \"op\" field".to_owned()));
    let r = run(&mut m, &mut conn, DB, kill(Value::Int32(1)));
    assert_eq!(error(&r).0, 13);
    for v in [Value::Int32(-1), Value::Int32(424_242), Value::Double(3.0)] {
        assert!(ok(&run(&mut m, &mut conn, "admin", kill(v))));
    }
    assert!(!lock(&m.shared).ops.get(id).unwrap().killed);
    let as_decimal = Value::Decimal128(Decimal128::from_parts(false, u128::from(id), 0).unwrap());
    assert!(ok(&run(&mut m, &mut conn, "admin", kill(as_decimal))));
    assert!(lock(&m.shared).ops.get(id).unwrap().killed);
}

/// `killOp` with a `shardid:opid` as a `refrouter` 9.0.4 reads it (measured, DV-031, D-102): the
/// shard first (70 when no node has that name; `config` is one), then the number (9 when it is
/// not one, 15 when it does not fit 32 bits); the reply names the shard and the number, and the
/// operation stops on its node.
#[test]
fn kill_op_takes_a_node_and_its_operation() {
    let mut m = node(0);
    m.shards = vec!["node1".to_owned(), "node2".to_owned()];
    let id = lock(&m.shared).ops.begin(op(7));
    let mut conn = Connection::new(1);
    let kill = |v: &str| Document::new().with("killOp", 1).with("op", v);
    let cases: [(&str, i32, &str); 12] = [
        (
            "node1",
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \"node1\"",
        ),
        (
            "node1:",
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \"node1:\"",
        ),
        (
            ":",
            28625,
            "The op argument to killOp must be of the format shardid:opid but found \":\"",
        ),
        ("nope:1", 70, "Shard nope not found"),
        ("nope:x", 70, "Shard nope not found"),
        ("node1:x", 9, "Did not consume whole string."),
        ("node1:1.5", 9, "Did not consume whole string."),
        ("node1: 5", 9, "Did not consume whole string."),
        ("node1:5:6", 9, "Did not consume whole string."),
        ("node1:0x10", 9, "Did not consume whole string."),
        ("node1:2147483648", 15, "Overflow"),
        ("node2:-2147483649", 15, "Overflow"),
    ];
    for (v, code, msg) in cases {
        assert_eq!(error(&run(&mut m, &mut conn, "admin", kill(v))), (code, msg.to_owned()), "{v}");
    }
    let answer = |shard: &str, n: i32| {
        Document::new().with("shard", shard).with("shardid", Value::Int32(n)).with("ok", 1.0)
    };
    for (v, shard, n) in
        [("node1:-5", "node1", -5), ("node1:007", "node1", 7), ("config:1", "config", 1)]
    {
        assert_eq!(run(&mut m, &mut conn, "admin", kill(v)), answer(shard, n), "{v}");
    }
    assert!(!lock(&m.shared).ops.get(id).unwrap().killed);
    assert!(m.killed.is_empty());
    // An operation of another node: that node stops it.
    assert_eq!(run(&mut m, &mut conn, "admin", kill("node2:+12")), answer("node2", 12));
    assert_eq!(m.killed, vec![KillScope::Op { node: 2, op: 12 }]);
    // One of this node.
    let mine = format!("node1:{id}");
    let n = i32::try_from(id).unwrap();
    assert_eq!(run(&mut m, &mut conn, "admin", kill(&mine)), answer("node1", n));
    assert!(lock(&m.shared).ops.get(id).unwrap().killed);
    assert_eq!(m.killed.len(), 1);
}

#[test]
fn get_more_takes_no_max_time_on_a_cursor_that_is_not_tailable() {
    let mut m = node(5);
    let mut conn = Connection::new(1);
    let r = run(&mut m, &mut conn, DB, Document::new().with("find", COLL).with("batchSize", 1));
    let id = match field(&r, "cursor.id") {
        Value::Int64(id) => *id,
        other => panic!("{other:?}"),
    };
    let more = |ms: i32| {
        Document::new().with("getMore", id).with("collection", COLL).with(MAX_TIME_MS, ms)
    };
    let r = run(&mut m, &mut conn, DB, more(5));
    assert_eq!(
        error(&r),
        (2, "maxTimeMS can only be used with getMore for tailable, awaitData cursors".to_owned())
    );
    // The error killed the cursor, as in a `refrouter` 9.0.4 (measured); a `maxTimeMS` of 0 on a
    // live cursor is no limit.
    assert_eq!(error(&run(&mut m, &mut conn, DB, more(0))).0, 43);
    let r = run(&mut m, &mut conn, DB, Document::new().with("find", COLL).with("batchSize", 1));
    let Value::Int64(id) = field(&r, "cursor.id").clone() else { panic!() };
    let r = run(
        &mut m,
        &mut conn,
        DB,
        Document::new().with("getMore", id).with("collection", COLL).with(MAX_TIME_MS, 0),
    );
    assert!(ok(&r));
}

#[test]
fn the_table_gives_fresh_ids_and_forgets_ended_operations() {
    let mut t = OpTable::default();
    let a = t.begin(op(1));
    let b = t.begin(op(2));
    assert_eq!((a, b), (1, 2));
    assert!(t.kill(a) && !t.kill(99));
    t.end(a);
    assert!(t.get(a).is_none() && t.get(b).is_some());
    assert_eq!(op_ns("db", &Document::new().with("find", "c")), "db.c");
    assert_eq!(op_ns("db", &Document::new().with("ping", 1)), "db.$cmd");
    assert_eq!(op_ns("db", &Document::new().with("drop", "")), "db.$cmd");
    assert_eq!(
        ["insert", "update", "delete", "getMore", "count"].map(op_type),
        ["insert", "update", "remove", "getmore", "command"]
    );
}

#[test]
fn a_guarded_backend_stops_at_its_deadline() {
    use teseladb_wire::commands::Backend;
    let mut m = node(0);
    let shared: Shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    let mut o = op(1);
    o.deadline_ms = Some(m.now + 5);
    let id = lock(&shared).ops.begin(o);
    {
        let mut g = Guarded::new(&mut m, &shared, id);
        assert!(g.check().is_ok());
        assert_eq!(g.node(), 1);
        let _ = g.random();
        assert!(g.collections().is_ok());
        assert!(g.cluster_parameters().is_ok());
        assert!(g.kill_everywhere(&teseladb_wire::commands::session::KillScope::All).is_ok());
    }
    assert_eq!(m.deadline, None);
    m.fail_next =
        Some(teseladb_wire::error::CommandError::new(teseladb_wire::error::BAD_VALUE, "x"));
    {
        let mut g = Guarded::new(&mut m, &shared, id);
        let op = teseladb_wire::commands::WriteOp {
            keys: vec![],
            mutation: vec![],
            dedup: None,
            wc: teseladb_range::ack::WriteConcern { w: teseladb_range::ack::W::Nodes(1), j: false },
        };
        assert_eq!(g.write(op).unwrap_err().code.code, 2);
    }
    m.now += 5;
    let mut g = Guarded::new(&mut m, &shared, id);
    assert_eq!(g.collections().unwrap_err().code.code, 50);
}

fn set_param(v: Value) -> Document {
    Document::new().with("setClusterParameter", v)
}

fn max_time_param(fields: Document) -> Value {
    Value::Document(Document::new().with(DEFAULT_MAX_TIME_MS, fields))
}

#[test]
fn cluster_parameters_are_set_and_read_as_a_refrouter_does() {
    let mut m = node(0);
    let mut conn = Connection::new(1);
    let get = |v: Value| Document::new().with("getClusterParameter", v);
    let r = run(&mut m, &mut conn, "admin", get(Value::from(DEFAULT_MAX_TIME_MS)));
    let unset = Document::new()
        .with("_id", DEFAULT_MAX_TIME_MS)
        .with("clusterParameterTime", Value::Timestamp { time: 0, increment: 0 })
        .with(READ_OPERATIONS, 0i64);
    assert_eq!(field(&r, "clusterParameters"), &Value::Array(vec![Value::Document(unset)]));
    let set = set_param(max_time_param(Document::new().with(READ_OPERATIONS, 1.5)));
    assert!(ok(&run(&mut m, &mut conn, "admin", set)));
    assert_eq!(default_read_ms(&m.params), 1);
    for v in [Value::from("*"), Value::Array(vec![Value::from(DEFAULT_MAX_TIME_MS)])] {
        let r = run(&mut m, &mut conn, "admin", get(v));
        let Value::Array(list) = field(&r, "clusterParameters") else { panic!() };
        // `"*"` also lists `auditConfig` (013 FR-031), before it in name order.
        let mine = |v: &&Value| matches!(v, Value::Document(d) if d.get("_id") == Some(&Value::from(DEFAULT_MAX_TIME_MS)));
        let Some(Value::Document(p)) = list.iter().find(mine) else { panic!() };
        assert_eq!(p.get(READ_OPERATIONS), Some(&Value::Int64(1)));
        assert_eq!(
            p.get("clusterParameterTime"),
            Some(&Value::Timestamp { time: 1_700_000_000, increment: 1 })
        );
    }
    // An empty value goes back to the default.
    assert!(ok(&run(&mut m, &mut conn, "admin", set_param(max_time_param(Document::new())))));
    assert_eq!(default_read_ms(&m.params), 0);
    let prefix = "Failed parsing ClusterServerParameter 'defaultMaxTimeMS' :: caused by :: ";
    let cases: [(Document, &str, i32, String); 13] = [
        (set_param(max_time_param(Document::new().with(READ_OPERATIONS, -1))), "admin", 2,
            format!("{prefix}BSON field 'readOperations' value must be >= 0, actual value '-1'")),
        (set_param(max_time_param(Document::new().with(READ_OPERATIONS, "x"))), "admin", 14,
            format!("{prefix}BSON field 'ClusterServerParameter.readOperations' is the wrong type 'string', expected types '[long, int, double, decimal]'")),
        (set_param(max_time_param(Document::new().with("bogus", 1))), "admin", 40415,
            format!("{prefix}BSON field 'ClusterServerParameter.bogus' is an unknown field.")),
        (set_param(max_time_param(Document::new().with("clusterParameterTime", 5))), "admin", 40413,
            format!("{prefix}BSON field 'ClusterServerParameter.clusterParameterTime' is a duplicate field")),
        (set_param(max_time_param(Document::new().with("_id", "x"))), "admin", 2,
            "Can't have multiple _id fields in one document".to_owned()),
        (set_param(Value::Document(Document::new().with(DEFAULT_MAX_TIME_MS, 5))), "admin", 2,
            "Cluster parameter value must be an object".to_owned()),
        (set_param(Value::Document(Document::new())), "admin", 4, "No cluster parameter provided".to_owned()),
        (set_param(Value::Document(Document::new().with("nope", Value::Document(Document::new())))), "admin", 4,
            "Unknown server parameter: nope".to_owned()),
        (set_param(Value::Document(Document::new().with("a", 1).with("b", 1))), "admin", 72,
            "setClusterParameter only supports setting exactly one parameter".to_owned()),
        (set_param(Value::Int32(5)), "admin", 14,
            "BSON field 'setClusterParameter.setClusterParameter' is the wrong type 'int', expected type 'object'".to_owned()),
        (set_param(max_time_param(Document::new())), DB, 13,
            "setClusterParameter may only be run against the admin database.".to_owned()),
        (get(Value::Array(vec![])), "admin", 2,
            "Must supply at least one cluster server parameter name to getClusterParameter".to_owned()),
        (get(Value::Array(vec![Value::from(DEFAULT_MAX_TIME_MS), Value::Int32(5)])), "admin", 14,
            "BSON field 'getClusterParameter.getClusterParameter.1' is the wrong type 'int', expected type 'string'".to_owned()),
    ];
    for (cmd, db, code, msg) in cases {
        assert_eq!(error(&run(&mut m, &mut conn, db, cmd)), (code, msg));
    }
    let r = run(&mut m, &mut conn, "admin", get(Value::Int32(5)));
    assert_eq!(
        error(&r),
        (14, "BSON field 'getClusterParameter.getClusterParameter' is the wrong type 'int', expected types '[string]'".to_owned())
    );
    let r = run(&mut m, &mut conn, "admin", get(Value::Array(vec![Value::from("nope")])));
    assert_eq!(error(&r), (4, "Unknown server parameter: nope".to_owned()));
}

#[test]
fn server_parameters_are_read_and_set_as_a_refrouter_does() {
    let mut m = node(0);
    let mut conn = Connection::new(1);
    let get = |name: &str| Document::new().with("getParameter", 1).with(name, 1);
    let set = |name: &str, v: Value| Document::new().with("setParameter", 1).with(name, v);
    let r = run(&mut m, &mut conn, "admin", get("cursorTimeoutMillis"));
    assert_eq!(r.get("cursorTimeoutMillis"), Some(&Value::Int64(600_000)), "{r:?}");
    let r = run(&mut m, &mut conn, "admin", set("cursorTimeoutMillis", Value::Double(1.5)));
    assert_eq!(r.get("was"), Some(&Value::Int64(600_000)), "{r:?}");
    assert_eq!(lock(&m.shared).cursors.timeout_ms(), 1);
    let r = run(&mut m, &mut conn, "admin", set("cursorTimeoutMillis", Value::Int32(-5)));
    assert_eq!(r.get("was"), Some(&Value::Int64(1)));
    assert_eq!(lock(&m.shared).cursors.timeout_ms(), 0);
    let r = run(&mut m, &mut conn, "admin", set("cursorTimeoutMillis", Value::Boolean(true)));
    assert_eq!(r.get("was"), Some(&Value::Int64(-5)));
    let r =
        run(&mut m, &mut conn, "admin", set("clientCursorMonitorFrequencySecs", Value::Int64(2)));
    assert_eq!(r.get("was"), Some(&Value::Int32(4)));
    assert_eq!(lock(&m.shared).params.monitor_ms, 2000);
    let three = Value::Decimal128(Decimal128::from_parts(false, 3, 0).unwrap());
    assert!(ok(&run(&mut m, &mut conn, "admin", set("clientCursorMonitorFrequencySecs", three))));
    // Generic fields are not parameters.
    let r = run(
        &mut m,
        &mut conn,
        "admin",
        set("cursorTimeoutMillis", Value::Int32(7)).with("comment", "x"),
    );
    assert!(ok(&r), "{r:?}");
    let details = Document::new()
        .with("getParameter", Document::new().with("showDetails", true))
        .with("localLogicalSessionTimeoutMinutes", 1)
        .with("enableTestCommands", 1);
    let r = run(&mut m, &mut conn, "admin", details);
    let expect = |v: Value, runtime: bool| {
        Value::Document(
            Document::new()
                .with("value", v)
                .with("settableAtRuntime", runtime)
                .with("settableAtStartup", true),
        )
    };
    assert_eq!(r.get("localLogicalSessionTimeoutMinutes"), Some(&expect(Value::Int32(30), false)));
    assert_eq!(r.get("enableTestCommands"), Some(&expect(Value::Boolean(false), false)));
    // `internalQueryPlannerEnableHashIntersection` takes any value as true or false, as a
    // `refrouter` 9.0.4 (measured: a string sets it, `0` clears it).
    let hash = "internalQueryPlannerEnableHashIntersection";
    let r = run(&mut m, &mut conn, "admin", get(hash));
    assert_eq!(r.get(hash), Some(&Value::Boolean(false)), "{r:?}");
    let r = run(&mut m, &mut conn, "admin", set(hash, Value::from("x")));
    assert_eq!(r.get("was"), Some(&Value::Boolean(false)), "{r:?}");
    let r = run(&mut m, &mut conn, "admin", set(hash, Value::Int32(0)));
    assert_eq!(r.get("was"), Some(&Value::Boolean(true)), "{r:?}");
    let r = run(&mut m, &mut conn, "admin", get(hash));
    assert_eq!(r.get(hash), Some(&Value::Boolean(false)), "{r:?}");
    let r = run(&mut m, &mut conn, "admin", Document::new().with("getParameter", "*"));
    let names: Vec<&str> = r.iter().map(|(k, _)| k).collect();
    // `auditAuthorizationSuccess` is RefDb Enterprise's, the OCSP ones, `authSchemaVersion` and
    // `maxConcurrentMultiDocumentTransactions` a refnode's (measured on 9.0.4; 013 FR-016, FR-030,
    // FR-031; 008 FR-020).
    assert_eq!(
        names,
        [
            "auditAuthorizationSuccess",
            "authSchemaVersion",
            "clientCursorMonitorFrequencySecs",
            "clusterAuthMode",
            "cursorTimeoutMillis",
            "enableTestCommands",
            "internalQueryMaxMemoryUsageBytesPerOperation",
            "internalQueryPlannerEnableHashIntersection",
            "localLogicalSessionTimeoutMinutes",
            "maxConcurrentMultiDocumentTransactions",
            "ocspEnabled",
            "ocspStaplingRefreshPeriodSecs",
            "ocspValidationRefreshPeriodSecs",
            "redactClientLogData",
            "tlsOCSPStaplingTimeoutSecs",
            "tlsOCSPVerifyTimeoutSecs",
            "transactionLifetimeLimitSeconds",
            "useInternalAuthzForX509",
            "ok"
        ]
    );
    let cases: [(Document, &str, i32, &str); 10] = [
        (get("nope"), "admin", 72, "no option found to get"),
        (Document::new().with("getParameter", 1), "admin", 72, "no option found to get"),
        (
            Document::new().with("setParameter", 1),
            "admin",
            72,
            "no option found to set, use help:true to see options ",
        ),
        (
            set("nope", Value::Int32(1)),
            "admin",
            72,
            "attempted to set unrecognized parameter [nope], use help:true to see options ",
        ),
        (
            set("enableTestCommands", Value::Int32(1)),
            "admin",
            20,
            "not allowed to change [enableTestCommands] at runtime",
        ),
        (
            set("cursorTimeoutMillis", Value::from("x")),
            "admin",
            2,
            "Failed validating cursorTimeoutMillis: Unable to coerce value to integral type",
        ),
        (
            set("cursorTimeoutMillis", Value::Null),
            "admin",
            2,
            "Failed validating cursorTimeoutMillis: Unable to coerce value to integral type",
        ),
        (
            set("cursorTimeoutMillis", Value::Double(1e20)),
            "admin",
            2,
            "Failed validating cursorTimeoutMillis: Out of bounds coercing to integral value",
        ),
        (
            set("cursorTimeoutMillis", Value::Double(f64::NAN)),
            "admin",
            2,
            "Failed validating cursorTimeoutMillis: Unable to coerce value to integral type",
        ),
        (
            get("cursorTimeoutMillis"),
            DB,
            13,
            "getParameter may only be run against the admin database.",
        ),
    ];
    for (cmd, db, code, msg) in cases {
        assert_eq!(error(&run(&mut m, &mut conn, db, cmd)), (code, msg.to_owned()));
    }
    // A failed change changes nothing, also the parameters before the bad one.
    let both = set("cursorTimeoutMillis", Value::Int32(9)).with("nope", 1);
    assert_eq!(error(&run(&mut m, &mut conn, "admin", both)).0, 72);
    assert_eq!(lock(&m.shared).params.cursor_timeout_ms, 7);
    let r = run(&mut m, &mut conn, DB, set("cursorTimeoutMillis", Value::Int32(1)));
    assert_eq!(error(&r).0, 13);
}

/// `maxTimeMS` is a length of time: it is counted on the monotonic clock, never on the cluster
/// time, which jumps ahead with a mark from a node whose clock is ahead and stops while the
/// node's clock is out of the cluster's margin (D-003).
#[test]
fn max_time_counts_on_the_monotonic_clock_and_not_on_the_cluster_time() {
    let mut m = node(1);
    m.mono = Some(10_000);
    let mut conn = Connection::new(1);
    let find = Document::new().with("find", COLL).with("maxTimeMS", 100);
    let r = run(&mut m, &mut conn, DB, find);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(m.last_deadline, Some(10_100), "the deadline is on the monotonic clock");
    let shared: Shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    let mut o = op(1);
    o.deadline_ms = Some(10_005);
    let id = lock(&shared).ops.begin(o);
    // The cluster time jumps a second ahead: the operation goes on.
    m.now += 1_000;
    assert!(Guarded::new(&mut m, &shared, id).check().is_ok());
    // The cluster time stops and the monotonic clock passes the deadline: it ends.
    m.mono = Some(10_005);
    let e = Guarded::new(&mut m, &shared, id).check().unwrap_err();
    assert_eq!(e.code.code, 50);
}

/// `$currentOp` (010 T033): the node's operations as `currentOp` describes them, the running
/// `aggregate` among them (measured on a `refrouter` 9.0.4: `type: "op"`, `op: "command"`,
/// `ns: "admin.$cmd.aggregate"`, `active: true`).
#[test]
fn current_op_stage_lists_the_running_aggregate() {
    let mut m = node(0);
    let mut conn = Connection::new(1);
    let cmd = teseladb_bson::extjson::parse(
        r#"{"aggregate":1,"pipeline":[{"$currentOp":{}},{"$match":{"command.pipeline.0.$currentOp":{"$exists":true}}},{"$project":{"_id":0,"type":1,"op":1,"ns":1,"active":1}}],"cursor":{}}"#,
    )
    .expect("command");
    let r = run(&mut m, &mut conn, "admin", cmd);
    let Value::Array(batch) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    assert_eq!(batch.len(), 1, "{r:?}");
    let Value::Document(d) = &batch[0] else { panic!("{r:?}") };
    assert_eq!(d.get("type"), Some(&Value::from("op")));
    assert_eq!(d.get("op"), Some(&Value::from("command")));
    assert_eq!(d.get("ns"), Some(&Value::from("admin.$cmd.aggregate")));
    assert_eq!(d.get("active"), Some(&Value::Boolean(true)));
}
