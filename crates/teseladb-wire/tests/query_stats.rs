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
        replica_set: None,
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
fn a_hint_is_part_of_the_key_after_the_client() {
    // RefDb 9.0.4 (02-11-2026): the hint, outside the shape, after `client` and before
    // `maxTimeMS`; an index's name as `{$hint: name}`; each hint its own entry.
    let mut c = Client::new();
    c.set(-1, 0.0);
    c.ok(DB, r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    c.ok(DB, r#"{"find":"c","filter":{"a":3},"hint":{"a":1},"maxTimeMS":500,"batchSize":3}"#);
    c.ok(DB, r#"{"find":"c","filter":{"a":3},"hint":"a_1"}"#);
    c.ok(DB, r#"{"find":"c","filter":{"a":3}}"#);
    let e = c.entries();
    let finds: Vec<&Document> =
        e.iter().filter(|d| field(d, "key.queryShape.command") == &Value::from("find")).collect();
    assert_eq!(finds.len(), 3, "{finds:?}");
    let with = |h: &str| {
        finds
            .iter()
            .find(|d| matches!(d.get("key"), Some(Value::Document(k)) if k.get("hint") == Some(&Value::Document(cmd(h)))))
            .unwrap_or_else(|| panic!("{h} in {finds:?}"))
    };
    let Value::Document(k) = field(with(r#"{"a":1}"#), "key") else { panic!() };
    assert_eq!(keys(k), ["queryShape", "readConcern", "client", "hint", "maxTimeMS", "batchSize"]);
    let Value::Document(k) = field(with(r#"{"$hint":"a_1"}"#), "key") else { panic!() };
    assert_eq!(keys(k), ["queryShape", "readConcern", "client", "hint"]);
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

/// The `true` count of question `q` of `metrics.queryPlanner` in entry `e`.
fn yes<'a>(e: &'a Document, q: &str) -> &'a Value {
    field(e, &format!("metrics.queryPlanner.{q}.true"))
}

#[test]
fn the_planner_part_says_how_each_query_was_planned() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    c.ok(DB, r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"},{"key":{"b":1},"name":"b_1"}]}"#);
    c.ok(DB, r#"{"find":"c","filter":{"z":1},"sort":{"z":1}}"#);
    c.ok(DB, r#"{"find":"c","filter":{},"sort":{"b":-1}}"#);
    c.ok(DB, r#"{"find":"c","filter":{"a":2,"b":{"$gt":0}}}"#);
    c.ok(
        DB,
        r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$a"}},{"$sort":{"_id":1}}],"cursor":{}}"#,
    );
    c.ok(
        DB,
        r#"{"aggregate":"c","pipeline":[{"$match":{"a":1}},{"$group":{"_id":"$b"}}],"cursor":{}}"#,
    );
    c.ok(DB, r#"{"distinct":"c","key":"b"}"#);
    let e = c.entries();
    let by = |command: &str, part: &str, v: &str| {
        e.iter()
            .find(|d| {
                field(d, "key.queryShape.command") == &Value::from(command)
                    && format!("{:?}", d.get("key")).contains(v)
                    && format!("{:?}", d.get("key")).contains(part)
            })
            .unwrap_or_else(|| panic!("{command} {v} in {e:?}"))
            .clone()
    };
    let one = &Value::Int64(1);
    let none = &Value::Int64(0);
    assert_eq!(yes(&by("find", "sort", "\"z\""), "hasSortStage"), one);
    assert_eq!(yes(&by("find", "sort", "\"b\""), "hasSortStage"), none);
    let multi = by("find", "filter", "$gt");
    assert_eq!(yes(&multi, "fromMultiPlanner"), one);
    assert_eq!(yes(&multi, "hasSortStage"), none);
    assert_eq!(yes(&multi, "fromPlanCache"), none);
    assert_eq!(yes(&by("aggregate", "pipeline", "$sort"), "hasSortStage"), one);
    let grouped = by("aggregate", "pipeline", "$match");
    assert_eq!(yes(&grouped, "hasSortStage"), none);
    assert_eq!(yes(&grouped, "usedDisk"), none);
    assert_eq!(field(&grouped, "metrics.queryPlanner.usedDisk.false"), one);
    let planner = match field(&grouped, "metrics.queryPlanner") {
        Value::Document(d) => d.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        keys(&planner),
        [
            "hasSortStage",
            "usedDisk",
            "fromMultiPlanner",
            "fromPlanCache",
            "planningTimeMicros",
            "costBasedRanker"
        ]
    );
    let metrics = match field(&grouped, "metrics") {
        Value::Document(d) => keys(d),
        other => panic!("{other:?}"),
    };
    let at = metrics.iter().position(|k| k == "queryExec").expect("queryExec");
    assert_eq!(metrics[at + 1..at + 4], ["queryPlanner", "writes", "firstSeenTimestamp"]);
    assert_eq!(field(&grouped, "metrics.writes.keysDeleted.sum"), &Value::Int64(0));
    assert_eq!(
        field(&grouped, "metrics.queryPlanner.costBasedRanker.nDocsSampled.max"),
        &Value::Int64(0)
    );
    assert_eq!(yes(&by("distinct", "key", "\"b\""), "hasSortStage"), none);
}

/// The collection `q` of 50 documents with `a` cycling through 0..5 and `b` their number, and its
/// indexes on `a` and on `b`, as the measure of `fromPlanCache` (02-11-2026) had it.
fn fifty(c: &mut Client) {
    let docs: Vec<String> =
        (0..50).map(|i| format!(r#"{{"_id":{i},"a":{},"b":{i}}}"#, i % 5)).collect();
    c.ok(DB, &format!(r#"{{"insert":"q","documents":[{}]}}"#, docs.join(",")));
    c.ok(DB, r#"{"createIndexes":"q","indexes":[{"key":{"a":1},"name":"a_1"},{"key":{"b":1},"name":"b_1"}]}"#);
}

/// The entry of the `find` on `q` whose filter's text contains `part`.
fn find_on_q(c: &mut Client, part: &str) -> Document {
    c.entries()
        .into_iter()
        .find(|d| {
            let key = format!("{:?}", d.get("key"));
            field(d, "key.queryShape.command") == &Value::from("find")
                && key.contains("\"q\"")
                && key.contains(part)
        })
        .unwrap_or_else(|| panic!("the find on q with {part} in {:?}", c.entries()))
}

#[test]
fn a_shape_runs_from_the_plan_cache_once_its_entry_is_active() {
    // RefDb 9.0.4, six runs of `{a: 3, b: {$gt: 3}}`: the first two plans compete (the first
    // leaves an inactive entry, the second makes it active), the other four take the cached plan.
    let mut c = Client::new();
    c.set(-1, 0.0);
    fifty(&mut c);
    for _ in 0..6 {
        c.ok(DB, r#"{"find":"q","filter":{"a":3,"b":{"$gt":3}}}"#);
    }
    let e = find_on_q(&mut c, "$gt");
    assert_eq!(yes(&e, "fromMultiPlanner"), &Value::Int64(2), "{e:?}");
    assert_eq!(field(&e, "metrics.queryPlanner.fromMultiPlanner.false"), &Value::Int64(4));
    assert_eq!(yes(&e, "fromPlanCache"), &Value::Int64(4), "{e:?}");
    assert_eq!(field(&e, "metrics.queryPlanner.fromPlanCache.false"), &Value::Int64(2));
    // The cached runs found what the competing ones found.
    assert_eq!(field(&e, "metrics.queryExec.docsReturned.sum"), &Value::Int64(54));
}

#[test]
fn a_cached_plan_that_works_ten_times_more_is_planned_again() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    fifty(&mut c);
    let query = r#"{"find":"q","filter":{"a":3,"b":{"$gt":45}}}"#;
    c.ok(DB, query);
    c.ok(DB, query);
    // Two thousand documents past `b: 45` that the filter on `a` rejects: the scan of `b` the
    // cache holds now reads them all without giving a batch.
    let more: Vec<String> =
        (100..2100).map(|i| format!(r#"{{"_id":{i},"a":4,"b":{i}}}"#)).collect();
    c.ok(DB, &format!(r#"{{"insert":"q","documents":[{}]}}"#, more.join(",")));
    c.ok(DB, query);
    // RefDb 9.0.4 drops the entry and keeps nothing of the plans that competed again.
    let r = c.ok(DB, r#"{"aggregate":"q","pipeline":[{"$planCacheStats":{}}],"cursor":{}}"#);
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()), "{r:?}");
    let e = find_on_q(&mut c, "$gt");
    assert_eq!(yes(&e, "fromMultiPlanner"), &Value::Int64(3), "{e:?}");
    assert_eq!(yes(&e, "fromPlanCache"), &Value::Int64(0), "{e:?}");
    assert_eq!(field(&e, "metrics.queryExec.docsReturned.sum"), &Value::Int64(3));
}

#[test]
fn a_hinted_read_never_takes_the_cached_plan() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    fifty(&mut c);
    for _ in 0..2 {
        c.ok(DB, r#"{"find":"q","filter":{"a":3,"b":{"$gt":3}}}"#);
    }
    c.ok(DB, r#"{"find":"q","filter":{"a":3,"b":{"$gt":3}},"hint":{"a":1}}"#);
    let e = find_on_q(&mut c, "hint");
    assert_eq!(yes(&e, "fromPlanCache"), &Value::Int64(0), "{e:?}");
}

#[test]
fn a_cached_plan_reads_the_values_of_the_query_that_runs_it() {
    // The plan the cache keeps is a plan of the shape: each query of that shape scans its own values.
    // Ten queries `{a, b}` with other values each: from the third on they take the cached plan, and
    // each finds its own document (measured on RefDb 9.0.4: a cached entry keeps no bounds).
    let mut c = Client::new();
    c.set(-1, 0.0);
    fifty(&mut c);
    for i in 0..10 {
        let q = format!(r#"{{"find":"q","filter":{{"a":{},"b":{i}}}}}"#, i % 5);
        let r = c.ok(DB, &q);
        let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
        assert_eq!(batch.len(), 1, "{q}: {r:?}");
        let Value::Document(d) = &batch[0] else { panic!("{r:?}") };
        assert_eq!(d.get("_id"), Some(&Value::Int32(i)), "{q}");
    }
    let e = find_on_q(&mut c, "\"b\"");
    assert!(yes(&e, "fromPlanCache") != &Value::Int64(0), "the plan came from the cache: {e:?}");
}

#[test]
fn a_cached_or_plan_reads_the_values_of_the_query_that_runs_it() {
    let mut c = Client::new();
    c.set(-1, 0.0);
    fifty(&mut c);
    for i in 0..8 {
        let q = format!(
            r#"{{"find":"q","filter":{{"$or":[{{"b":{i}}},{{"b":{}}}]}},"sort":{{"_id":1}}}}"#,
            i + 20
        );
        let r = c.ok(DB, &q);
        let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
        let ids: Vec<&Value> = batch
            .iter()
            .map(|d| match d {
                Value::Document(d) => d.get("_id").unwrap_or(&Value::Null),
                other => other,
            })
            .collect();
        assert_eq!(ids, [&Value::Int32(i), &Value::Int32(i + 20)], "{q}");
    }
}
