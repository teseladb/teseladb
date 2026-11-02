//! Indexes on the meta value and the time of a time series used over its buckets (015 FR-008,
//! T014), as a `refnode` 9.0.4 counted them in `$indexStats` on 02-11-2026: a read with a
//! condition on the meta value scans the default index `m_1_t_1`, one on the time alone reads
//! the buckets in `_id` order unless an index on the time exists, one on a measurement field
//! scans the buckets; each lookup of a bucket by an insert scans the default index too.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::{NodeSessions, Shared, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const DB: &str = "ti";
const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const H: i64 = 3_600_000;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    };
    let bytes = OpMsg::encode(1, 0, 0, &json(cmd).with("$db", DB).to_bytes().unwrap());
    let mut c = Connection::new(9);
    c.receive(&bytes);
    let req: Request = c.next_request().unwrap().unwrap();
    let shared: Shared = std::mem::take(&mut m.shared);
    let mut conn = Connection::new(1);
    let r = match dispatch_with(&mut conn, &req, &info, Some((m, &shared))) {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    };
    m.shared = shared;
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
    r
}

fn date(ms: i64) -> String {
    format!(r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#)
}

/// `accesses.ops` of each index of `s`, by name.
fn ops(m: &mut Mem) -> Vec<(String, i64)> {
    let pipeline = r#"[{"$indexStats":{}},{"$project":{"_id":0,"name":1,"o":"$accesses.ops"}},{"$sort":{"name":1}}]"#;
    let r = run(m, &format!(r#"{{"aggregate":"s","pipeline":{pipeline},"cursor":{{}}}}"#));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    batch
        .iter()
        .map(|d| {
            let Value::Document(d) = d else { panic!("{d:?}") };
            let Value::String(name) = field(d, "name") else { panic!("{d:?}") };
            let n = match field(d, "o") {
                Value::Int64(n) => *n,
                Value::Int32(n) => i64::from(*n),
                other => panic!("{other:?}"),
            };
            (name.clone(), n)
        })
        .collect()
}

/// How many measurements a `find` with `filter` returns.
fn found(m: &mut Mem, filter: &str) -> usize {
    let r = run(m, &format!(r#"{{"find":"s","filter":{filter},"batchSize":1000}}"#));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    batch.len()
}

fn named(list: &[(&str, i64)]) -> Vec<(String, i64)> {
    list.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

#[test]
fn reads_by_meta_value_scan_the_default_index_over_the_buckets() {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    run(&mut m, r#"{"create":"s","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let docs: Vec<String> =
        (0..30).map(|i| format!(r#"{{"t":{},"m":{},"v":{i}}}"#, date(T0 + i * H), i % 3)).collect();
    run(&mut m, &format!(r#"{{"insert":"s","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 3)]));
    assert_eq!(found(&mut m, r#"{"m":1}"#), 10);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 4)]));
    let from = date(T0 + 5 * H);
    assert_eq!(found(&mut m, &format!(r#"{{"t":{{"$gte":{from}}}}}"#)), 25);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 4)]));
    let some = format!(r#"{{"m":{{"$in":[1,2]}},"t":{{"$lt":{from}}}}}"#);
    assert_eq!(found(&mut m, &some), 3);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 5)]));
    assert_eq!(found(&mut m, r#"{"v":3}"#), 1);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 5)]));
    assert_eq!(found(&mut m, r#"{"m":1,"v":{"$gt":3}}"#), 9);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 6)]));
    let index = json(r#"{"createIndexes":"s","indexes":[{"key":{"t":1},"name":"t_1"}]}"#);
    let r = m.run(DB, index);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run(DB, doc(&[("listIndexes", Value::from("s"))]));
    let Value::Array(listed) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(listed.len(), 2, "{r:?}");
    assert_eq!(found(&mut m, &format!(r#"{{"t":{{"$gte":{from}}}}}"#)), 25);
    assert_eq!(ops(&mut m), named(&[("m_1_t_1", 6), ("t_1", 1)]));
}
