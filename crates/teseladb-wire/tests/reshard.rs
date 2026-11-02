//! 018 T005: `reshardCollection`, `refineCollectionShardKey` and `abortReshardCollection`
//! (FR-002) over the in-memory node of `common`: the copy at the clone mark, the double write,
//! the critical section, the rename, the abort and the events, with the replies and messages of
//! a `refrouter` 9.0.4 of two shards (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::reshard::{Action, Phase, Resharding, next_action, reshard_plan};
use teseladb_wire::commands::{Ctx, Shared};

const DB: &str = "rs";

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn ok(r: &Document) {
    assert_eq!(field(r, "ok"), &Value::Double(1.0), "{r:?}");
}

fn fails(r: &Document, code: i32, msg: &str) {
    assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{r:?}");
}

fn admin(m: &mut Mem, cmd: &str) -> Document {
    m.run("admin", json(cmd))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run(DB, json(cmd))
}

/// `rs.c` sharded by `{a: 1}` with documents `docs`.
fn sharded(docs: &str) -> Mem {
    let mut m = Mem::new();
    ok(&admin(&mut m, r#"{"shardCollection":"rs.c","key":{"a":1}}"#));
    if docs != "[]" {
        ok(&run(&mut m, &format!(r#"{{"insert":"c","documents":{docs}}}"#)));
    }
    m
}

/// `n` documents `{_id: i, a: i, b: i}`.
fn docs(n: i32) -> String {
    let all: Vec<String> = (0..n).map(|i| format!(r#"{{"_id":{i},"a":{i},"b":{i}}}"#)).collect();
    format!("[{}]", all.join(","))
}

fn batch(r: &Document, at: &str) -> Vec<Document> {
    match field(r, at) {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn find_all(m: &mut Mem) -> Vec<Document> {
    batch(&run(m, r#"{"find":"c","sort":{"_id":1}}"#), "cursor.firstBatch")
}

fn config_entry(m: &mut Mem) -> Document {
    let r = m.run("config", json(r#"{"find":"collections","filter":{"_id":"rs.c"}}"#));
    batch(&r, "cursor.firstBatch").into_iter().next().expect("the entry")
}

fn index_names(m: &mut Mem) -> Vec<String> {
    batch(&run(m, r#"{"listIndexes":"c"}"#), "cursor.firstBatch")
        .iter()
        .map(|d| match d.get("name") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// A resharding started by hand, for the steps between its phases.
fn started(m: &mut Mem, cmd: &str) -> (Resharding, Shared) {
    let shared = std::mem::take(&mut m.shared);
    let cmd = json(cmd);
    let plan = {
        let mut ctx = Ctx { backend: &mut *m, shared: &shared, db: "admin", cmd: &cmd };
        reshard_plan(&mut ctx).expect("a plan").expect("a new key")
    };
    let r = Resharding::start(m, &shared, plan, &cmd).expect("started");
    (r, shared)
}

#[test]
fn its_arguments_fail_as_on_a_refrouter() {
    let mut m = sharded(&docs(3));
    fails(
        &admin(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"performVerification":true}"#),
        72,
        "Cannot set 'performVerification' to true when reshardingDocumentVerification is false",
    );
    ok(&run(&mut m, r#"{"insert":"u","documents":[{"_id":1}]}"#));
    let cases: [(&str, i32, &str); 11] = [
        (
            r#"{"reshardCollection":"rs.u","key":{"a":1}}"#,
            26,
            "Collection 'rs.u' not found in cluster catalog",
        ),
        (
            r#"{"reshardCollection":"rs.nope","key":{"a":1}}"#,
            26,
            "Collection 'rs.nope' not found in cluster catalog",
        ),
        (r#"{"reshardCollection":"rs.c","key":{}}"#, 2, "Shard key is empty"),
        (
            r#"{"reshardCollection":"rs.c","key":{"a":2}}"#,
            2,
            "Shard key { a: 2 } is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field a",
        ),
        (
            r#"{"reshardCollection":"rs.c","key":{"a":1},"unique":true}"#,
            2,
            "The unique field must be false",
        ),
        (
            r#"{"reshardCollection":"rs.c"}"#,
            40414,
            "BSON field 'reshardCollection.key' is missing but a required field",
        ),
        (
            r#"{"reshardCollection":5,"key":{"a":1}}"#,
            14,
            "BSON field 'reshardCollection.reshardCollection' is the wrong type 'int', expected type 'string'",
        ),
        (
            r#"{"reshardCollection":"rs.c","key":{"a":1},"collation":{"locale":"fr"}}"#,
            2,
            "The collation for reshardCollection must be {locale: 'simple'}, but found: { locale: \"fr\" }",
        ),
        (
            r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":"x"}"#,
            14,
            "BSON field 'reshardCollection.numInitialChunks' is the wrong type 'string', expected types '[int, long, decimal, double]'",
        ),
        (
            r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":0}"#,
            28747,
            "size argument to $sample must be a positive integer",
        ),
        (
            r#"{"reshardCollection":"rs.c","key":{"b":1}}"#,
            4952606,
            "The shard key provided does not have enough cardinality to make the required number of chunks of 90, it can only make 2 chunks",
        ),
    ];
    for (cmd, code, msg) in cases {
        fails(&admin(&mut m, cmd), code, msg);
    }
    fails(
        &run(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1}}"#),
        13,
        "reshardCollection may only be run against the admin database.",
    );
    // The fields of its IDL, checked before it runs.
    let e = teseladb_wire::generic::check_unknown_fields(
        &json(r#"{"reshardCollection":"rs.c","key":{"a":1},"bogus":1}"#),
        "reshardCollection",
    )
    .expect_err("an unknown field");
    assert_eq!(
        (e.code.code, e.errmsg.as_str()),
        (40415, "BSON field 'reshardCollection.bogus' is an unknown field.")
    );
    let known = r#"{"reshardCollection":"rs.c","key":{"a":1},"numInitialChunks":1,"forceRedistribution":true,"zones":[]}"#;
    teseladb_wire::generic::check_unknown_fields(&json(known), "reshardCollection").expect("known");
    // The same key answers at once, whatever else it asks.
    ok(&admin(&mut m, r#"{"reshardCollection":"rs.c","key":{"a":1},"numInitialChunks":-1}"#));
}

#[test]
fn the_cardinality_counts_every_tenth_document_of_the_new_order() {
    // Measured: 1 to 10 documents make 2 chunks, 20 make 3, 100 make 11; two values make 3.
    for (n, key, can) in [(0, "b", 1), (1, "b", 2), (10, "b", 2), (20, "b", 3), (100, "b", 11)] {
        let mut m = sharded(&docs(n));
        let cmd = format!(
            r#"{{"reshardCollection":"rs.c","key":{{"{key}":1}},"numInitialChunks":1000}}"#
        );
        let msg = format!(
            "The shard key provided does not have enough cardinality to make the required number of chunks of 1000, it can only make {can} chunks"
        );
        fails(&admin(&mut m, &cmd), 4952606, &msg);
    }
    let mut m = sharded(&docs(100));
    ok(&run(
        &mut m,
        r#"{"update":"c","updates":[{"q":{"_id":{"$lt":50}},"u":{"$set":{"d":0}},"multi":true},{"q":{"_id":{"$gte":50}},"u":{"$set":{"d":1}},"multi":true}]}"#,
    ));
    let msg = "The shard key provided does not have enough cardinality to make the required number of chunks of 1000, it can only make 3 chunks";
    fails(
        &admin(&mut m, r#"{"reshardCollection":"rs.c","key":{"d":1},"numInitialChunks":1000}"#),
        4952606,
        msg,
    );
    // A thousand documents give the 90 chunks a resharding needs by default.
    let mut m = sharded(&docs(1000));
    ok(&admin(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1}}"#));
}

#[test]
fn a_resharding_rewrites_the_collection_in_the_new_order() {
    let mut m = sharded(r#"[{"_id":1,"a":1,"b":5},{"_id":2,"a":2,"b":6},{"_id":3,"a":3}]"#);
    let before = config_entry(&mut m);
    ok(&admin(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":"hashed"},"numInitialChunks":1}"#));
    assert_eq!(find_all(&mut m).len(), 3);
    let after = config_entry(&mut m);
    assert_eq!(after.get("key"), Some(&Value::Document(json(r#"{"b":"hashed"}"#))));
    assert_ne!(after.get("uuid"), before.get("uuid"), "a new UUID");
    // The new key's index is made; the old ones stay.
    assert_eq!(index_names(&mut m), ["_id_", "a_1", "b_hashed"]);
    // Reads by the new key and by `_id` find the documents.
    let r = run(&mut m, r#"{"find":"c","filter":{"b":6}}"#);
    assert_eq!(batch(&r, "cursor.firstBatch").len(), 1, "{r:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":3}}"#);
    assert_eq!(batch(&r, "cursor.firstBatch").len(), 1, "{r:?}");
    // The copy is gone: only the collection is listed.
    let r = run(&mut m, r#"{"listCollections":1,"nameOnly":true}"#);
    assert_eq!(batch(&r, "cursor.firstBatch").len(), 1, "{r:?}");
    // Writes go on under the new key.
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":4,"b":7}]}"#));
    ok(&run(&mut m, r#"{"delete":"c","deletes":[{"q":{"_id":1},"limit":1}]}"#));
    let ids: Vec<Value> = find_all(&mut m).iter().map(|d| d.get("_id").cloned().unwrap()).collect();
    assert_eq!(ids, [Value::Int32(2), Value::Int32(3), Value::Int32(4)]);
}

#[test]
fn writes_after_the_clone_mark_reach_the_copy() {
    let mut m = sharded(&docs(5));
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    assert_eq!(r.phase(), Phase::Apply);
    m.shared = shared;
    m.now += 5;
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":10,"a":10,"b":10}]}"#));
    ok(&run(&mut m, r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"b":100,"x":1}}}]}"#));
    ok(&run(&mut m, r#"{"delete":"c","deletes":[{"q":{"_id":2},"limit":1}]}"#));
    // A document whose shard key changes moves to another key in a transaction.
    let txn = r#"{"update":"c","updates":[{"q":{"_id":3},"u":{"$set":{"a":30}}}],"lsid":{"id":{"$binary":{"base64":"AAAAAAAAAAAAAAAAAAAAAA==","subType":"04"}}},"txnNumber":{"$numberLong":"100"}}"#;
    ok(&run(&mut m, txn));
    let shared = std::mem::take(&mut m.shared);
    m.now += 5;
    r.catch_up(&mut m, &shared).expect("applied");
    m.now += 5;
    ok(&{
        m.shared = shared;
        run(&mut m, r#"{"insert":"c","documents":[{"_id":11,"a":11,"b":11}]}"#)
    });
    let shared = std::mem::take(&mut m.shared);
    m.now += 5;
    r.drive(&mut m, &shared).expect("committed");
    assert_eq!(r.phase(), Phase::Committed);
    r.finish(&shared);
    m.shared = shared;
    let got = find_all(&mut m);
    let ids: Vec<Value> = got.iter().map(|d| d.get("_id").cloned().unwrap()).collect();
    let want: Vec<Value> = [0, 1, 3, 4, 10, 11].into_iter().map(Value::Int32).collect();
    assert_eq!(ids, want, "{got:?}");
    assert_eq!(got[1].get("b"), Some(&Value::Int32(100)));
    assert_eq!(got[2].get("a"), Some(&Value::Int32(30)));
}

/// The catch-up finds the copy's version of a document from the history when an update left
/// the new key alone, and reads it from the copy's `_id_` index otherwise: every document is
/// in the copy once, as it is now.
#[test]
fn the_catch_up_leaves_one_version_of_each_document_in_the_copy() {
    let mut m = sharded(&docs(6));
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    m.shared = shared;
    let writes = [
        r#"{"update":"c","updates":[{"q":{"_id":0},"u":{"$set":{"x":1}}}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"b":50}}}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":2},"u":{"_id":2,"a":2,"b":70}}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":3},"u":{"$unset":{"b":""}}}]}"#,
        r#"{"insert":"c","documents":[{"_id":20,"a":20,"b":20}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":20},"u":{"$set":{"x":20}}}]}"#,
        r#"{"delete":"c","deletes":[{"q":{"_id":4},"limit":1}]}"#,
    ];
    for w in writes {
        m.now += 5;
        ok(&run(&mut m, w));
    }
    let shared = std::mem::take(&mut m.shared);
    m.now += 5;
    r.catch_up(&mut m, &shared).expect("applied");
    m.shared = shared;
    // A second round: the copy holds the versions of the first.
    for w in [
        r#"{"update":"c","updates":[{"q":{"_id":0},"u":{"$set":{"x":2}}}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"y":1}}}]}"#,
        r#"{"update":"c","updates":[{"q":{"_id":20},"u":{"$inc":{"b":1}}}]}"#,
    ] {
        m.now += 5;
        ok(&run(&mut m, w));
    }
    let shared = std::mem::take(&mut m.shared);
    m.now += 5;
    r.drive(&mut m, &shared).expect("committed");
    r.finish(&shared);
    m.shared = shared;
    let got = find_all(&mut m);
    let want = json(
        r#"{"all":[{"_id":0,"a":0,"b":0,"x":2},{"_id":1,"a":1,"b":50,"y":1},{"_id":2,"a":2,"b":70},{"_id":3,"a":3},{"_id":5,"a":5,"b":5},{"_id":20,"a":20,"b":21,"x":20}]}"#,
    );
    let want = batch(&want, "all");
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (g, w) in got.iter().zip(&want) {
        for (k, v) in w.iter() {
            assert_eq!(g.get(k), Some(v), "{g:?}");
        }
    }
    assert_eq!(got[3].get("b"), None, "{got:?}");
}

#[test]
fn writes_wait_for_the_critical_section() {
    let mut m = sharded(&docs(3));
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    r.enter_critical(&mut m, &shared).expect("critical");
    m.shared = shared;
    // With no time left the write fails; reads go on.
    let w = run(&mut m, r#"{"insert":"c","documents":[{"_id":9,"b":9}]}"#);
    let errors = batch(&w, "writeErrors");
    assert_eq!(errors[0].get("code"), Some(&Value::Int32(50)), "{w:?}");
    let u = run(&mut m, r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"z":1}}}]}"#);
    assert!(u.get("writeErrors").is_some(), "{u:?}");
    assert_eq!(find_all(&mut m).len(), 3);
    // The waits a write is given: it lands once the rename lifts the section.
    let shared = std::mem::take(&mut m.shared);
    r.drive(&mut m, &shared).expect("committed");
    r.finish(&shared);
    m.shared = shared;
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":9,"b":9}]}"#));
    assert_eq!(find_all(&mut m).len(), 4);
}

#[test]
fn an_abort_drops_the_copy_and_fails_the_resharding() {
    let mut m = sharded(&docs(3));
    fails(
        &admin(&mut m, r#"{"abortReshardCollection":"rs.c"}"#),
        339,
        "Could not find resharding-related metadata that matches the given namespace",
    );
    let before = config_entry(&mut m);
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    r.enter_critical(&mut m, &shared).expect("critical");
    m.shared = shared;
    // `currentOp` lists it with its metrics.
    let ops = batch(&admin(&mut m, r#"{"currentOp":1}"#), "inprog");
    let op = ops
        .iter()
        .find(|o| matches!(o.get("desc"), Some(Value::String(d)) if d.starts_with("ReshardingMetricsRecipientService ")))
        .expect("the resharding in currentOp");
    assert_eq!(op.get("recipientState"), Some(&Value::from("strict-consistency")));
    assert_eq!(op.get("documentsCopied"), Some(&Value::Int64(3)));
    assert_eq!(op.get("provenance"), Some(&Value::from("reshardCollection")));
    ok(&admin(&mut m, r#"{"abortReshardCollection":"rs.c"}"#));
    let shared = std::mem::take(&mut m.shared);
    let e = r.drive(&mut m, &shared).expect_err("aborted");
    assert_eq!((e.code.code, e.errmsg.as_str()), (341, "resharding aborted by user"));
    assert_eq!(r.phase(), Phase::Aborted);
    r.finish(&shared);
    m.shared = shared;
    assert_eq!(config_entry(&mut m), before);
    let r = run(&mut m, r#"{"listCollections":1,"nameOnly":true}"#);
    assert_eq!(batch(&r, "cursor.firstBatch").len(), 1, "{r:?}");
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":9,"a":9}]}"#));
    let ops = batch(&admin(&mut m, r#"{"currentOp":1}"#), "inprog");
    assert!(ops.iter().all(|o| o.get("recipientState").is_none()), "{ops:?}");
}

#[test]
fn refine_extends_the_key_and_keeps_the_uuid() {
    let mut m = sharded(r#"[{"_id":1,"a":1,"c":5},{"_id":2,"a":2}]"#);
    ok(&run(&mut m, r#"{"insert":"u","documents":[{"_id":1}]}"#));
    let cases: [(&str, i32, &str); 4] = [
        (
            r#"{"refineCollectionShardKey":"rs.c","key":{"b":1}}"#,
            72,
            "refineCollectionShardKey shard key { b: 1 } does not extend the current shard key { a: 1 }",
        ),
        (
            r#"{"refineCollectionShardKey":"rs.nope","key":{"a":1}}"#,
            26,
            "RefineCollectionShardKey: collection rs.nope does not exists",
        ),
        (
            r#"{"refineCollectionShardKey":"rs.c","key":{"a":1,"c":1}}"#,
            72,
            "Please create an index that starts with the proposed shard key before refining the collection's shard key. ",
        ),
        (
            r#"{"refineCollectionShardKey":"rs.u","key":{"a":1}}"#,
            118,
            "Can't execute refineCollectionShardKey on unsharded collection rs.u",
        ),
    ];
    for (cmd, code, msg) in cases {
        fails(&admin(&mut m, cmd), code, msg);
    }
    // The same key answers at once; unknown fields are not checked (measured).
    ok(&admin(&mut m, r#"{"refineCollectionShardKey":"rs.c","key":{"a":1},"bogus":1}"#));
    ok(&run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"a":1,"c":1},"name":"ac"}]}"#));
    let before = config_entry(&mut m);
    ok(&admin(&mut m, r#"{"refineCollectionShardKey":"rs.c","key":{"a":1,"c":1}}"#));
    let after = config_entry(&mut m);
    assert_eq!(after.get("uuid"), before.get("uuid"));
    assert_eq!(after.get("key"), Some(&Value::Document(json(r#"{"a":1,"c":1}"#))));
    assert_eq!(find_all(&mut m).len(), 2);
    assert_eq!(index_names(&mut m), ["_id_", "a_1", "ac"]);
}

fn open_stream(m: &mut Mem) -> i64 {
    let r = run(
        m,
        r#"{"aggregate":"c","pipeline":[{"$changeStream":{"showExpandedEvents":true}}],"cursor":{}}"#,
    );
    match field(&r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

fn next_events(m: &mut Mem, id: i64) -> Vec<Document> {
    let more = doc(&[
        ("getMore", Value::Int64(id)),
        ("collection", Value::from("c")),
        ("maxTimeMS", Value::Int32(1)),
    ]);
    let r = m.run(DB, more);
    batch(&r, "cursor.nextBatch")
}

fn strip(mut e: Document) -> Document {
    for k in ["_id", "clusterTime", "wallTime"] {
        e.remove(k);
    }
    e
}

#[test]
fn a_stream_sees_the_resharding_and_goes_on() {
    let mut m = sharded(r#"[{"_id":1,"a":1,"b":5}]"#);
    let old = config_entry(&mut m).get("uuid").cloned().unwrap();
    let id = open_stream(&mut m);
    m.now += 1;
    ok(&admin(
        &mut m,
        r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1,"collation":{"locale":"simple"}}"#,
    ));
    m.now += 1;
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":2,"b":6}]}"#));
    m.now += 1;
    let new = config_entry(&mut m).get("uuid").cloned().unwrap();
    let events: Vec<Document> = next_events(&mut m, id).into_iter().map(strip).collect();
    assert_eq!(events.len(), 2, "{events:?}");
    let desc = Document::new()
        .with("reshardUUID", new.clone())
        .with("shardKey", json(r#"{"b":1}"#))
        .with("oldShardKey", json(r#"{"a":1}"#))
        .with("unique", false)
        .with("numInitialChunks", Value::Int64(1))
        .with("collation", json(r#"{"locale":"simple"}"#))
        .with("provenance", "reshardCollection");
    let want = Document::new()
        .with("operationType", "reshardCollection")
        .with("collectionUUID", old)
        .with("ns", json(r#"{"db":"rs","coll":"c"}"#))
        .with("operationDescription", desc);
    assert_eq!(events[0], want);
    assert_eq!(events[1].get("operationType"), Some(&Value::from("insert")));
    assert_eq!(events[1].get("collectionUUID"), Some(&new));
    assert_eq!(events[1].get("documentKey"), Some(&Value::Document(json(r#"{"b":6,"_id":2}"#))));
}

#[test]
fn a_stream_sees_the_refine() {
    let mut m = sharded(r#"[{"_id":1,"a":1,"c":5}]"#);
    ok(&run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"a":1,"c":1},"name":"ac"}]}"#));
    let uuid = config_entry(&mut m).get("uuid").cloned().unwrap();
    let id = open_stream(&mut m);
    m.now += 1;
    ok(&admin(&mut m, r#"{"refineCollectionShardKey":"rs.c","key":{"a":1,"c":1}}"#));
    m.now += 1;
    let events: Vec<Document> = next_events(&mut m, id).into_iter().map(strip).collect();
    let want = Document::new()
        .with("operationType", "refineCollectionShardKey")
        .with("collectionUUID", uuid)
        .with("ns", json(r#"{"db":"rs","coll":"c"}"#))
        .with(
            "operationDescription",
            Document::new()
                .with("shardKey", json(r#"{"a":1,"c":1}"#))
                .with("oldShardKey", json(r#"{"a":1}"#)),
        );
    assert_eq!(events, [want]);
}

#[test]
fn a_write_that_found_the_collection_before_the_cutover_is_not_lost() {
    use teseladb_meta::catalog::Namespace;
    use teseladb_wire::commands::Backend;
    use teseladb_wire::commands::crud::{Writer, index_rules, rules_of};
    use teseladb_wire::commands::mutation::{Bound, DocMutation};
    use teseladb_wire::commands::sharding::document_key;
    let mut m = sharded(&docs(2));
    let ns = Namespace { db: DB.to_owned(), coll: "c".to_owned() };
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    // The writer read the collection before its critical section.
    let old = m.collection(&ns).expect("read").expect("the collection");
    let rules =
        rules_of(&ns, &old, true, None, index_rules(&mut m, &old).expect("rules")).expect("rules");
    m.now += 5;
    let d = json(r#"{"_id":7,"a":7,"b":7}"#);
    let key = document_key(&old, &json(r#"{"a":1}"#), &d).expect("a key");
    let insert = DocMutation::Insert { docs: vec![d], ordered: true, rules };
    Writer::internal()
        .write(&mut m, &Bound { mutation: insert, keys: vec![key] }, 0)
        .expect("written");
    // It reached its range before the fence: the critical section's catch-up takes it.
    r.enter_critical(&mut m, &shared).expect("critical");
    m.now += 5;
    r.drive(&mut m, &shared).expect("committed");
    r.finish(&shared);
    m.shared = shared;
    let ids: Vec<Value> = find_all(&mut m).iter().map(|d| d.get("_id").cloned().unwrap()).collect();
    assert_eq!(ids, [Value::Int32(0), Value::Int32(1), Value::Int32(7)]);
}

/// 018 T005, the race the fence closes: a write that read the collection before the critical
/// section and reaches its range after the last read of the history (here, after the rename) is
/// refused with `StaleConfig` instead of landing in the dropped collection.
#[test]
fn a_write_that_reaches_the_range_after_the_last_catch_up_is_refused() {
    use teseladb_meta::catalog::Namespace;
    use teseladb_wire::commands::Backend;
    use teseladb_wire::commands::crud::{Writer, index_rules, rules_of};
    use teseladb_wire::commands::mutation::{Bound, DocMutation};
    use teseladb_wire::commands::sharding::document_key;
    let mut m = sharded(&docs(2));
    let ns = Namespace { db: DB.to_owned(), coll: "c".to_owned() };
    let (mut r, shared) =
        started(&mut m, r#"{"reshardCollection":"rs.c","key":{"b":1},"numInitialChunks":1}"#);
    r.copy(&mut m, &shared).expect("copied");
    let old = m.collection(&ns).expect("read").expect("the collection");
    let rules =
        rules_of(&ns, &old, true, None, index_rules(&mut m, &old).expect("rules")).expect("rules");
    m.now += 5;
    r.drive(&mut m, &shared).expect("committed");
    r.finish(&shared);
    m.shared = shared;
    m.now += 5;
    let d = json(r#"{"_id":7,"a":7,"b":7}"#);
    let key = document_key(&old, &json(r#"{"a":1}"#), &d).expect("a key");
    let insert = DocMutation::Insert { docs: vec![d], ordered: true, rules };
    let e = Writer::internal()
        .write(&mut m, &Bound { mutation: insert, keys: vec![key] }, 0)
        .expect_err("the fence refuses it");
    assert_eq!(e.code.code, 13388, "{e:?}");
    // The statement sent again finds the new collection.
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":7,"a":7,"b":7}]}"#));
    let ids: Vec<Value> = find_all(&mut m).iter().map(|d| d.get("_id").cloned().unwrap()).collect();
    assert_eq!(ids, [Value::Int32(0), Value::Int32(1), Value::Int32(7)]);
}

/// Every key of the data space fenced, as a critical section that ends at the next pause
/// (`lift`) or outlasts the operation.
fn fence_all(m: &mut Mem, lift: bool) {
    m.fences = vec![(vec![0], vec![0xff; 16])];
    m.lift_on_pause = lift;
}

#[test]
fn statements_a_fence_refused_go_again_after_it() {
    let mut m = sharded(&docs(3));
    fence_all(&mut m, true);
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":9,"a":9}]}"#));
    fence_all(&mut m, true);
    ok(&run(&mut m, r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"z":1}}}]}"#));
    fence_all(&mut m, true);
    ok(&run(&mut m, r#"{"delete":"c","deletes":[{"q":{"_id":2},"limit":1}]}"#));
    let got = find_all(&mut m);
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[1].get("z"), Some(&Value::Int32(1)));
    // A fence that outlasts the operation: its time runs out.
    fence_all(&mut m, false);
    let w = run(&mut m, r#"{"insert":"c","documents":[{"_id":10,"a":10}]}"#);
    assert_eq!(batch(&w, "writeErrors")[0].get("code"), Some(&Value::Int32(50)), "{w:?}");
    let u = run(&mut m, r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"z":2}}}]}"#);
    assert_eq!(batch(&u, "writeErrors")[0].get("code"), Some(&Value::Int32(50)), "{u:?}");
    // A `multi` statement may have changed documents before the fence: it is not repeated.
    fence_all(&mut m, true);
    let u = run(&mut m, r#"{"update":"c","updates":[{"q":{},"u":{"$inc":{"n":1}},"multi":true}]}"#);
    assert_eq!(batch(&u, "writeErrors")[0].get("code"), Some(&Value::Int32(13388)), "{u:?}");
}

#[test]
fn the_steps_follow_the_model() {
    let steps = [
        (Phase::Clone, false, Action::Copy),
        (Phase::Apply, false, Action::CatchUp),
        (Phase::Apply, true, Action::EnterCritical),
        (Phase::Critical, false, Action::CatchUp),
        (Phase::Critical, true, Action::Rename),
        (Phase::Committed, false, Action::CatchUp),
        (Phase::Committed, true, Action::Done),
        (Phase::Aborted, false, Action::Done),
    ];
    for (phase, caught_up, want) in steps {
        assert_eq!(next_action(phase, caught_up), want, "{phase:?} {caught_up}");
    }
}
