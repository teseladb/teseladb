//! Writes of capped collections (015 T007, FR-003): insertion order, the oldest documents evicted
//! in the same write, `_id` kept unique by its own `_id_` index, updates and deletions that move
//! the collection's state, and the refusal inside a transaction, against what a `refrouter` 9.0.4
//! answered on 31-10-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("m", json(cmd))
}

fn ids(m: &mut Mem, cmd: &str) -> Vec<Value> {
    let r = run(m, cmd);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "_id").clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ints(v: &[i32]) -> Vec<Value> {
    v.iter().map(|i| Value::Int32(*i)).collect()
}

#[test]
fn an_insert_past_max_keeps_the_newest_in_insertion_order() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":256,"max":3}"#);
    let r = run(
        &mut m,
        r#"{"insert":"cap","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3},{"_id":4,"a":4},{"_id":5,"a":5}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(5), "{r:?}");
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[3, 4, 5]));
    // Inserted in descending `_id` order, they come back in insertion order.
    run(&mut m, r#"{"create":"desc","capped":true,"size":100000}"#);
    run(&mut m, r#"{"insert":"desc","documents":[{"_id":10},{"_id":9},{"_id":8},{"_id":0}]}"#);
    assert_eq!(ids(&mut m, r#"{"find":"desc"}"#), ints(&[10, 9, 8, 0]));
}

#[test]
fn an_insert_past_size_evicts_by_bytes() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":1024}"#);
    // 20 documents of 100 bytes in a collection of 1 KiB: the last 10 stay.
    let pad = "x".repeat(100 - 24);
    for i in 0..20 {
        let r =
            run(&mut m, &format!(r#"{{"insert":"cap","documents":[{{"_id":{i},"p":"{pad}"}}]}}"#));
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    }
    assert_eq!(
        ids(&mut m, r#"{"find":"cap","batchSize":100}"#),
        ints(&(10..20).collect::<Vec<_>>())
    );
    // One batch of 20 does the same, oldest first.
    run(&mut m, r#"{"create":"batch","capped":true,"size":1024}"#);
    let docs: Vec<String> = (0..20).map(|i| format!(r#"{{"_id":{i},"p":"{pad}"}}"#)).collect();
    let r = run(&mut m, &format!(r#"{{"insert":"batch","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(20), "{r:?}");
    assert_eq!(
        ids(&mut m, r#"{"find":"batch","batchSize":100}"#),
        ints(&(10..20).collect::<Vec<_>>())
    );
}

#[test]
fn a_repeated_id_is_a_duplicate_of_the_id_index() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":256,"max":3}"#);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":4},{"_id":5}]}"#);
    let r = run(&mut m, r#"{"insert":"cap","documents":[{"_id":5,"a":9},{"_id":6}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(0), "{r:?}");
    let Value::Document(e) = field(&r, "writeErrors.0") else { panic!("{r:?}") };
    assert_eq!(field(e, "code"), &Value::Int32(11000));
    assert_eq!(
        field(e, "errmsg"),
        &Value::String(
            "E11000 duplicate key error collection: m.cap index: _id_ dup key: { _id: 5 }".into()
        )
    );
    assert_eq!(field(e, "keyPattern"), &Value::Document(json(r#"{"_id":1}"#)));
    assert_eq!(field(e, "keyValue"), &Value::Document(json(r#"{"_id":5}"#)));
    let r =
        run(&mut m, r#"{"insert":"cap","documents":[{"_id":6,"a":9},{"_id":6}],"ordered":false}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1));
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[4, 5, 6]));
    // An evicted `_id` can come back.
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":7},{"_id":4}]}"#);
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[6, 7, 4]));
}

#[test]
fn updates_and_deletions_change_the_state_and_the_next_insert_evicts() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":256,"max":3}"#);
    run(
        &mut m,
        r#"{"insert":"cap","documents":[{"_id":4,"a":4},{"_id":5,"a":5},{"_id":6,"a":6}]}"#,
    );
    let r = run(&mut m, r#"{"delete":"cap","deletes":[{"q":{"_id":5},"limit":1}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let pad = "x".repeat(300);
    let r = run(
        &mut m,
        &format!(
            r#"{{"update":"cap","updates":[{{"q":{{"_id":4}},"u":{{"$set":{{"s":"{pad}"}}}}}}]}}"#
        ),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    // The collection is over `size` until the next insert, which evicts the grown document.
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[4, 6]));
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":8}]}"#);
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[6, 8]));
    let r = run(&mut m, r#"{"find":"cap","filter":{"_id":8}}"#);
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(8));
    let r = run(&mut m, r#"{"find":"cap","filter":{"_id":4}}"#);
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()));
}

#[test]
fn an_upsert_goes_at_the_end_and_find_and_modify_removes() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"n","capped":true,"size":100000}"#);
    run(
        &mut m,
        r#"{"insert":"n","documents":[{"_id":10},{"_id":9},{"_id":8},{"_id":7},{"_id":6},{"_id":0}]}"#,
    );
    let r = run(&mut m, r#"{"delete":"n","deletes":[{"q":{"_id":{"$gt":7}},"limit":0}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
    let r = run(&mut m, r#"{"findAndModify":"n","query":{"_id":7},"remove":true}"#);
    assert_eq!(field(&r, "value"), &Value::Document(json(r#"{"_id":7}"#)), "{r:?}");
    let r = run(
        &mut m,
        r#"{"update":"n","updates":[{"q":{"_id":50},"u":{"$set":{"a":1}},"upsert":true}]}"#,
    );
    assert_eq!(field(&r, "upserted.0._id"), &Value::Int32(50), "{r:?}");
    assert_eq!(ids(&mut m, r#"{"find":"n"}"#), ints(&[6, 0, 50]));
    // An upsert past `max` evicts the oldest.
    run(&mut m, r#"{"create":"small","capped":true,"size":1000,"max":2}"#);
    run(&mut m, r#"{"insert":"small","documents":[{"_id":1},{"_id":2}]}"#);
    run(
        &mut m,
        r#"{"update":"small","updates":[{"q":{"_id":3},"u":{"$set":{"a":1}},"upsert":true}]}"#,
    );
    assert_eq!(ids(&mut m, r#"{"find":"small"}"#), ints(&[2, 3]));
}

#[test]
fn a_secondary_index_loses_the_entries_of_evicted_documents() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":1000,"max":2}"#);
    let r = run(
        &mut m,
        r#"{"createIndexes":"cap","indexes":[{"key":{"a":1},"name":"a_1","unique":true}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    run(
        &mut m,
        r#"{"insert":"cap","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3,"a":3}]}"#,
    );
    // `a: 1` went with its document: it can be inserted again; `a: 3` is still there.
    let r = run(&mut m, r#"{"insert":"cap","documents":[{"_id":4,"a":1}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"insert":"cap","documents":[{"_id":5,"a":3}]}"#);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000), "{r:?}");
}

/// Runs `cmd` through the dispatch, which keeps the session's transactions.
fn dispatched(m: &mut Mem, cmd: &str) -> Document {
    use teseladb_wire::connection::{Connection, Request};
    use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    };
    let bytes = teseladb_wire::msg::OpMsg::encode(
        1,
        0,
        0,
        &json(cmd).with("$db", "m").to_bytes().expect("a command"),
    );
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().expect("a request").expect("a request");
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info, Some((&mut *m, &shared)));
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_transaction_does_not_write_a_capped_collection() {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(teseladb_wire::commands::NodeSessions::new(
        teseladb_wire::commands::Timeouts::default(),
    ));
    run(&mut m, r#"{"create":"cap","capped":true,"size":4096,"max":2}"#);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":1}]}"#);
    let txn = |n: i32| {
        format!(
            r#""lsid":{{"id":{{"$binary":{{"base64":"AQEBAQEBAQEBAQEBAQEBAQ==","subType":"04"}}}}}},"txnNumber":{{"$numberLong":"{n}"}},"autocommit":false,"startTransaction":true"#
        )
    };
    let r = dispatched(
        &mut m,
        &format!(r#"{{"insert":"cap","documents":[{{"_id":101}}],{}}}"#, txn(1)),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(263), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::String("Collection 'm.cap' is a capped collection. Writes in transactions are not allowed on capped collections.".into())
    );
    let r = dispatched(
        &mut m,
        &format!(r#"{{"delete":"cap","deletes":[{{"q":{{}},"limit":0}}],{}}}"#, txn(2)),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(20), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::String(
            "Cannot remove from a capped collection in a multi-document transaction: m.cap".into()
        )
    );
    assert_eq!(ids(&mut m, r#"{"find":"cap"}"#), ints(&[1]));
}

/// `listCollections` of the collection `name`.
fn listed(m: &mut Mem, name: &str) -> Document {
    let filter = Value::Document(doc(&[("name", Value::from(name))]));
    m.run("m", doc(&[("listCollections", Value::Int32(1)), ("filter", filter)]))
}

fn options(m: &mut Mem, name: &str) -> Document {
    let r = listed(m, name);
    let Value::Document(o) = field(&r, "cursor.firstBatch.0.options") else { panic!("{r:?}") };
    o.clone()
}

fn uuid(m: &mut Mem, name: &str) -> Value {
    let r = listed(m, name);
    field(&r, "cursor.firstBatch.0.info.uuid").clone()
}

#[test]
fn convert_to_capped_keeps_the_options_and_drops_the_indexes() {
    let mut m = Mem::new();
    run(
        &mut m,
        r#"{"create":"p","validator":{"a":{"$type":"int"}},"validationLevel":"moderate","validationAction":"warn"}"#,
    );
    run(&mut m, r#"{"insert":"p","documents":[{"_id":1,"a":1},{"_id":2,"a":2}]}"#);
    run(&mut m, r#"{"createIndexes":"p","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    let before = uuid(&mut m, "p");
    let r = run(&mut m, r#"{"convertToCapped":"p","size":1000.7,"max":5,"foo":1}"#);
    assert_eq!(r, json(r#"{"ok":1.0}"#));
    assert_eq!(
        options(&mut m, "p"),
        json(
            r#"{"capped":true,"size":{"$numberLong":"1000"},"validator":{"a":{"$type":"int"}},"validationLevel":"moderate","validationAction":"warn"}"#
        )
    );
    assert_ne!(uuid(&mut m, "p"), before);
    let r = run(&mut m, r#"{"listIndexes":"p"}"#);
    assert_eq!(ids(&mut m, r#"{"find":"p","sort":{"_id":1}}"#), ints(&[1, 2]));
    let Value::Array(idx) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(idx.len(), 1, "only _id_: {r:?}");
    // Now capped: it keeps its limit and `_id` stays unique.
    let r = run(&mut m, r#"{"insert":"p","documents":[{"_id":1}]}"#);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000), "{r:?}");
    // Converting again takes the new size.
    run(&mut m, r#"{"convertToCapped":"p","size":{"$numberLong":"500"}}"#);
    assert_eq!(field(&options(&mut m, "p"), "size"), &Value::Int64(500));
}

#[test]
fn convert_to_capped_evicts_what_goes_over_size() {
    let mut m = Mem::new();
    let pad = "x".repeat(100);
    let docs: Vec<String> = (1..=3).map(|i| format!(r#"{{"_id":{i},"a":"{pad}"}}"#)).collect();
    run(&mut m, &format!(r#"{{"insert":"p","documents":[{}]}}"#, docs.join(",")));
    run(&mut m, r#"{"convertToCapped":"p","size":300}"#);
    assert_eq!(ids(&mut m, r#"{"find":"p"}"#).len(), 2);
    run(&mut m, &format!(r#"{{"insert":"p","documents":[{{"_id":4,"a":"{pad}"}}]}}"#));
    let left = ids(&mut m, r#"{"find":"p"}"#);
    assert_eq!((left.len(), left.last()), (2, Some(&Value::Int32(4))));
}

#[test]
fn convert_to_capped_refuses_as_a_refrouter_does() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"p"}"#);
    run(&mut m, r#"{"create":"v","viewOn":"p","pipeline":[]}"#);
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t"}}"#);
    run(&mut m, r#"{"create":"cl","clusteredIndex":{"key":{"_id":1},"unique":true}}"#);
    let cases = [
        (r#"{"convertToCapped":"p"}"#, 72, "Capped collection size must be greater than zero"),
        (
            r#"{"convertToCapped":"p","size":0}"#,
            72,
            "Capped collection size must be greater than zero",
        ),
        (
            r#"{"convertToCapped":"p","size":-5}"#,
            72,
            "Capped collection size must be greater than zero",
        ),
        (
            r#"{"convertToCapped":"p","size":"a"}"#,
            72,
            "Capped collection size must be greater than zero",
        ),
        (r#"{"convertToCapped":"p","size":2e15}"#, 2, "size cannot exceed 1 PB"),
        (r#"{"convertToCapped":"nope","size":100}"#, 26, "source collection m.nope does not exist"),
        (r#"{"convertToCapped":"v","size":100}"#, 166, "Namespace m.v is a view, not a collection"),
        (
            r#"{"convertToCapped":"ts","size":100}"#,
            20,
            "Cannot convert a timeseries collection to a capped collection",
        ),
        (
            r#"{"convertToCapped":"cl","size":100}"#,
            6_127_800,
            "Clustered capped collection only available with 'enableTestCommands' server parameter",
        ),
        (r#"{"convertToCapped":1,"size":100}"#, 73, "Collection name has invalid type int"),
    ];
    for (cmd, code, msg) in cases {
        let r = run(&mut m, cmd);
        assert_eq!(
            (field(&r, "code"), field(&r, "errmsg")),
            (&Value::Int32(code), &Value::String(msg.to_owned())),
            "{cmd}"
        );
    }
    // A `refrouter` 9.0.4 has no `cloneCollectionAsCapped` (measured).
    let r = dispatched(&mut m, r#"{"cloneCollectionAsCapped":"p","toCollection":"q","size":100}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(59), "{r:?}");
    let r = run(&mut m, r#"{"convertToCapped":1125899906842624,"size":1}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(73), "{r:?}");
    let r = run(&mut m, r#"{"convertToCapped":"p","size":{"$numberLong":"1125899906842624"}}"#);
    assert_eq!(r, json(r#"{"ok":1.0}"#));
}

#[test]
fn a_dropped_capped_collection_leaves_no_key_behind() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":1000}"#);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":1},{"_id":2}]}"#);
    let before = m.data.len();
    assert!(before > 0);
    run(&mut m, r#"{"drop":"cap"}"#);
    let data_keys = m.data.keys().filter(|k| k.first() == Some(&1)).count();
    assert_eq!(data_keys, 0, "documents, `_id_` entries and the state are gone");
}

#[test]
fn coll_stats_shows_each_kind_as_a_refrouter_does() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"cap","capped":true,"size":4096,"max":10}"#);
    run(&mut m, r#"{"insert":"cap","documents":[{"_id":1},{"_id":2}]}"#);
    run(&mut m, r#"{"create":"cap2","capped":true,"size":4096}"#);
    run(&mut m, r#"{"create":"cl","clusteredIndex":{"key":{"_id":1},"unique":true}}"#);
    run(&mut m, r#"{"create":"v","viewOn":"cap","pipeline":[]}"#);
    run(&mut m, r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    run(
        &mut m,
        r#"{"insert":"ts","documents":[{"t":{"$date":"2026-01-01T00:00:00Z"},"m":1},{"t":{"$date":"2026-01-01T00:00:00Z"},"m":2}]}"#,
    );
    let keys = |d: &Document| d.iter().map(|(k, _)| k.to_owned()).collect::<Vec<_>>();
    let r = run(&mut m, r#"{"collStats":"cap"}"#);
    assert_eq!(
        keys(&r),
        [
            "sharded",
            "capped",
            "max",
            "ns",
            "count",
            "size",
            "storageSize",
            "totalIndexSize",
            "totalSize",
            "indexSizes",
            "avgObjSize",
            "maxSize",
            "nindexes",
            "scaleFactor",
            "nchunks",
            "ok"
        ]
    );
    assert_eq!(
        (field(&r, "capped"), field(&r, "max"), field(&r, "maxSize")),
        (&Value::Boolean(true), &Value::Int32(10), &Value::Int32(4096))
    );
    let r = run(&mut m, r#"{"collStats":"cap2"}"#);
    assert_eq!((field(&r, "max"), field(&r, "maxSize")), (&Value::Int32(0), &Value::Int32(4096)));
    let r = run(&mut m, r#"{"collStats":"cl"}"#);
    assert_eq!(
        (field(&r, "indexSizes"), field(&r, "totalIndexSize")),
        (&Value::Document(Document::new()), &Value::Int32(0))
    );
    let r = run(&mut m, r#"{"collStats":"v"}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(166), "{r:?}");
    let r = run(&mut m, r#"{"collStats":"ts"}"#);
    assert!(r.get("count").is_none(), "{r:?}");
    assert_eq!(field(&r, "timeseries.bucketCount"), &Value::Int32(2));
    assert_eq!(field(&r, "timeseries.bucketsNs"), &Value::String("m.ts".into()));
}
