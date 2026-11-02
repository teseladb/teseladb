//! `$queryStats` through the dispatcher (010 FR-023), as a `refrouter` 9.0.4 counts queries
//! (measured on 02-11-2026 on database `qsk`): a query counts when its cursor ends, with every
//! batch; its key has the read concern, the write concern of an `aggregate` and the client's
//! metadata; the parameters `internalQueryStatsRateLimit` and `internalQueryStatsSampleRate`
//! decide which queries count.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::{NodeSessions, Shared, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const DB: &str = "qsk";
const RATE_LIMIT: &str = "internalQueryStatsRateLimit";
const SAMPLE_RATE: &str = "internalQueryStatsSampleRate";
const CLIENT: &str = r#"{"application":{"name":"refshell 2.14.0"},"driver":{"name":"nodejs","version":"7.5.0"},"os":{"type":"Linux"}}"#;

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

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

/// A node and one client connection, which said hello with [`CLIENT`].
struct Client {
    m: Mem,
    conn: Connection,
}

impl Client {
    fn new() -> Client {
        let mut m = Mem::new();
        m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
        let mut c = Client { m, conn: Connection::new(1) };
        c.run("admin", &format!(r#"{{"hello":1,"client":{CLIENT}}}"#));
        let docs = r#"[{"_id":1,"a":1,"b":1},{"_id":2,"a":2,"b":2},{"_id":3,"a":3,"b":3}]"#;
        let r = c.m.run(DB, cmd(&format!(r#"{{"insert":"c","documents":{docs}}}"#)));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
        c
    }

    fn run(&mut self, db: &str, text: &str) -> Document {
        let shared: Shared = std::mem::take(&mut self.m.shared);
        let req = request(cmd(text).with("$db", db));
        let r = match dispatch_with(&mut self.conn, &req, &info(), Some((&mut self.m, &shared))) {
            Action::Reply(d) => d,
            other => panic!("{other:?}"),
        };
        self.m.shared = shared;
        r
    }

    fn ok(&mut self, db: &str, text: &str) -> Document {
        let r = self.run(db, text);
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{text}: {r:?}");
        r
    }

    fn set(&mut self, limit: i64, rate: f64) {
        self.ok("admin", &format!(r#"{{"setParameter":1,"{RATE_LIMIT}":{limit}}}"#));
        self.ok("admin", &format!(r#"{{"setParameter":1,"{SAMPLE_RATE}":{rate:?}}}"#));
    }

    /// The entries of collection `c`.
    fn entries(&mut self) -> Vec<Document> {
        let r = self.ok("admin", r#"{"aggregate":1,"pipeline":[{"$queryStats":{}}],"cursor":{}}"#);
        let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
        b.iter()
            .filter_map(|e| match e {
                Value::Document(d) if field(d, "key.queryShape.cmdNs.db") == &Value::from(DB) => {
                    Some(d.clone())
                }
                _ => None,
            })
            .collect()
    }
}

fn keys(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_owned()).collect()
}

#[test]
fn a_query_counts_when_its_cursor_ends_with_every_batch() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    let r = c.ok(DB, r#"{"find":"c","filter":{"b":{"$gt":0}},"batchSize":1}"#);
    let Value::Int64(id) = *field(&r, "cursor.id") else { panic!("{r:?}") };
    assert!(c.entries().is_empty());
    c.ok(DB, &format!(r#"{{"getMore":{{"$numberLong":"{id}"}},"collection":"c","batchSize":1}}"#));
    assert!(c.entries().is_empty());
    c.ok(DB, &format!(r#"{{"getMore":{{"$numberLong":"{id}"}},"collection":"c"}}"#));
    let e = c.entries();
    assert_eq!(e.len(), 1, "{e:?}");
    let m = &e[0];
    assert_eq!(field(m, "metrics.execCount"), &Value::Int64(1));
    assert_eq!(field(m, "metrics.queryExec.docsReturned.sum"), &Value::Int64(3));
    assert_eq!(field(m, "metrics.queryExec.docsExamined.sum"), &Value::Int64(3));
    assert_eq!(field(m, "metrics.queryExec.keysExamined.sum"), &Value::Int64(0));
    assert!(m.get("metrics").is_some_and(|v| matches!(v, Value::Document(d)
        if d.get("workingTimeMillis").is_some())));
    let key = match field(m, "key") {
        Value::Document(k) => k.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(keys(&key), ["queryShape", "readConcern", "client", "batchSize"]);
    let rc =
        doc(&[("level", Value::from("local")), ("provenance", Value::from("implicitDefault"))]);
    assert_eq!(field(&key, "readConcern"), &Value::Document(rc));
    assert_eq!(field(&key, "client"), &Value::Document(cmd(CLIENT)));
}

#[test]
fn a_killed_cursor_counts_what_it_returned() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    let r = c.ok(DB, r#"{"find":"c","filter":{"b":{"$gt":0}},"batchSize":1}"#);
    let Value::Int64(id) = *field(&r, "cursor.id") else { panic!("{r:?}") };
    c.ok(DB, &format!(r#"{{"killCursors":"c","cursors":[{{"$numberLong":"{id}"}}]}}"#));
    let e = c.entries();
    assert_eq!(field(&e[0], "metrics.queryExec.docsReturned.sum"), &Value::Int64(1));
}

#[test]
fn an_aggregate_key_has_options_in_the_measured_order() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    c.ok(
        DB,
        r#"{"aggregate":"c","pipeline":[{"$match":{"o":1}}],"cursor":{"batchSize":5},"maxTimeMS":1000,"comment":"hi","writeConcern":{"w":1}}"#,
    );
    c.ok(
        DB,
        r#"{"find":"c","filter":{"o":1},"batchSize":5,"maxTimeMS":1000,"comment":"hi","readConcern":{"level":"local"}}"#,
    );
    let e = c.entries();
    let agg = e
        .iter()
        .find(|d| field(d, "key.queryShape.command") == &Value::from("aggregate"))
        .expect("the aggregate");
    let Value::Document(k) = field(agg, "key") else { panic!() };
    let order =
        ["queryShape", "comment", "readConcern", "writeConcern", "client", "maxTimeMS", "cursor"];
    assert_eq!(keys(k), order);
    let wc = cmd(r#"{"w":1,"wtimeout":0,"provenance":"clientSupplied"}"#);
    assert_eq!(field(k, "writeConcern"), &Value::Document(wc));
    assert_eq!(field(k, "comment"), &Value::from("?string"));
    let find = e
        .iter()
        .find(|d| field(d, "key.queryShape.command") == &Value::from("find"))
        .expect("the find");
    let Value::Document(k) = field(find, "key") else { panic!() };
    assert_eq!(
        keys(k),
        ["queryShape", "comment", "readConcern", "client", "maxTimeMS", "batchSize"]
    );
    let rc = cmd(r#"{"level":"local","provenance":"clientSupplied"}"#);
    assert_eq!(field(k, "readConcern"), &Value::Document(rc));
}

#[test]
fn the_parameters_decide_which_queries_count() {
    let mut c = Client::new();
    let r = c.ok("admin", &format!(r#"{{"getParameter":1,"{RATE_LIMIT}":1,"{SAMPLE_RATE}":1}}"#));
    assert_eq!(field(&r, RATE_LIMIT), &Value::Int32(0));
    assert_eq!(field(&r, SAMPLE_RATE), &Value::Double(0.01));
    for (limit, rate, filter, counted) in
        [(0, 0.0, "z0", 0), (-1, 0.0, "z1", 3), (0, 1.0, "z2", 3), (2, 0.0, "z3", 2)]
    {
        c.set(limit, rate);
        for i in 0..3 {
            c.ok(DB, &format!(r#"{{"find":"c","filter":{{"{filter}":{i}}}}}"#));
        }
        let n = c
            .entries()
            .iter()
            .find(|d| matches!(field(d, "key.queryShape.filter"), Value::Document(f) if f.get(filter).is_some()))
            .map_or(0, |d| match field(d, "metrics.execCount") {
                Value::Int64(n) => *n,
                other => panic!("{other:?}"),
            });
        assert_eq!(n, counted, "{limit} {rate}");
    }
    let r = c.ok("admin", &format!(r#"{{"getParameter":1,"{SAMPLE_RATE}":1}}"#));
    assert_eq!(field(&r, SAMPLE_RATE), &Value::Int32(0));
}

#[test]
fn set_parameter_refuses_what_refdb_refuses() {
    let mut c = Client::new();
    let cases = [
        (
            format!(r#"{{"setParameter":1,"{SAMPLE_RATE}":"x"}}"#),
            format!("Failed validating {SAMPLE_RATE}: Unable to coerce value to correct type"),
        ),
        (
            format!(r#"{{"setParameter":1,"{SAMPLE_RATE}":true}}"#),
            format!("Failed validating {SAMPLE_RATE}: Unable to coerce value to correct type"),
        ),
        (
            format!(r#"{{"setParameter":1,"{SAMPLE_RATE}":2}}"#),
            format!("Invalid value for parameter {SAMPLE_RATE}: 2 is not less than or equal to 1"),
        ),
        (
            format!(r#"{{"setParameter":1,"{SAMPLE_RATE}":-0.5}}"#),
            format!(
                "Invalid value for parameter {SAMPLE_RATE}: -0.5 is not greater than or equal to 0"
            ),
        ),
        (
            format!(r#"{{"setParameter":1,"{RATE_LIMIT}":-2}}"#),
            format!(
                "Invalid value for parameter {RATE_LIMIT}: -2 is not greater than or equal to -1"
            ),
        ),
        (
            format!(r#"{{"setParameter":1,"{RATE_LIMIT}":"x"}}"#),
            format!("Failed validating {RATE_LIMIT}: Unable to coerce value to integral type"),
        ),
        (
            format!(r#"{{"setParameter":1,"{RATE_LIMIT}":{{"$numberLong":"5000000000"}}}}"#),
            format!("Failed validating {RATE_LIMIT}: Out of bounds coercing to integral value"),
        ),
    ];
    for (text, msg) in cases {
        let r = c.run("admin", &text);
        assert_eq!(field(&r, "code"), &Value::Int32(2), "{text}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{text}");
    }
    let r = c.ok("admin", &format!(r#"{{"setParameter":1,"{RATE_LIMIT}":1.7}}"#));
    assert_eq!(field(&r, "was"), &Value::Int32(0));
    let r = c.ok("admin", &format!(r#"{{"getParameter":1,"{RATE_LIMIT}":1}}"#));
    assert_eq!(field(&r, RATE_LIMIT), &Value::Int32(1));
}

#[test]
fn an_index_read_counts_the_keys_and_documents_it_examined() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    c.ok(DB, r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    c.ok(DB, r#"{"find":"c","filter":{"a":2}}"#);
    c.ok(DB, r#"{"count":"c","query":{"b":1}}"#);
    let e = c.entries();
    let by = |command: &str| {
        e.iter()
            .find(|d| field(d, "key.queryShape.command") == &Value::from(command))
            .unwrap_or_else(|| panic!("{command} in {e:?}"))
            .clone()
    };
    let find = by("find");
    assert_eq!(field(&find, "metrics.queryExec.keysExamined.sum"), &Value::Int64(1));
    assert_eq!(field(&find, "metrics.queryExec.docsExamined.sum"), &Value::Int64(1));
    let count = by("count");
    assert_eq!(field(&count, "metrics.queryExec.docsReturned.sum"), &Value::Int64(1));
    assert_eq!(field(&count, "metrics.queryExec.docsExamined.sum"), &Value::Int64(3));
}

#[test]
fn an_equality_lookup_reads_the_foreign_side_through_its_index() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    let docs: Vec<String> = (0..60).map(|i| format!(r#"{{"_id":{i},"k":{i}}}"#)).collect();
    c.ok(DB, &format!(r#"{{"insert":"f","documents":[{}]}}"#, docs.join(",")));
    c.ok(DB, r#"{"createIndexes":"f","indexes":[{"key":{"k":1},"name":"k_1"}]}"#);
    let lookup = r#"{"$lookup":{"from":"f","localField":"a","foreignField":"k","as":"j"}}"#;
    let r = c.ok(
        DB,
        &format!(r#"{{"aggregate":"c","pipeline":[{{"$match":{{"a":{{"$lte":2}}}}}},{lookup}],"cursor":{{}}}}"#),
    );
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b.len(), 2);
    for d in b {
        let Value::Document(d) = d else { panic!() };
        let Value::Array(j) = field(d, "j") else { panic!("{d:?}") };
        assert_eq!(j.len(), 1, "{d:?}");
        assert_eq!(
            field(d, "a"),
            match &j[0] {
                Value::Document(f) => field(f, "k"),
                other => panic!("{other:?}"),
            }
        );
    }
    let e = c.entries();
    let agg = e
        .iter()
        .find(|d| field(d, "key.queryShape.command") == &Value::from("aggregate"))
        .expect("the aggregate");
    let Value::Int64(examined) = *field(agg, "metrics.queryExec.docsExamined.sum") else {
        panic!("{agg:?}")
    };
    // The three local documents and the two foreign ones, not the 60 of `f`.
    assert!(examined < 10, "{examined}");
}
