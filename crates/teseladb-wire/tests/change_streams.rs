//! `$changeStream` over the in-memory node of `common` (014 FR-001, FR-002, FR-013): the open, the
//! `getMore`s with their events and `postBatchResumeToken`, the scopes and the errors, as a
//! `refrouter` 9.0.4 answers them (measured on 01-11).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_changestream::token::Position;

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn stream(spec: Document, rest: Vec<Document>) -> Value {
    let mut stages = vec![Document::new().with("$changeStream", spec)];
    stages.extend(rest);
    arr(stages)
}

fn open(m: &mut Mem, db: &str, target: Value, pipeline: Value) -> Document {
    m.run(
        db,
        Document::new()
            .with("aggregate", target)
            .with("pipeline", pipeline)
            .with("cursor", Document::new()),
    )
}

fn id_of(r: &Document) -> i64 {
    match field(r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

fn get_more(m: &mut Mem, db: &str, id: i64, coll: &str, extra: Document) -> Document {
    let mut cmd = Document::new().with("getMore", id).with("collection", coll);
    for (k, v) in extra.iter() {
        cmd.push(k, v.clone());
    }
    m.run(db, cmd)
}

fn batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.nextBatch") {
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

fn token(r: &Document) -> Position {
    match field(r, "cursor.postBatchResumeToken._data") {
        Value::String(s) => Position::decode(s).expect("a token"),
        other => panic!("{other:?}"),
    }
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        other => panic!("no error: {other:?} in {r:?}"),
    }
}

fn insert(m: &mut Mem, coll: &str, docs: Vec<Document>) {
    let r = m.run(DB, Document::new().with("insert", coll).with("documents", arr(docs)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn create(m: &mut Mem, coll: &str) {
    m.run(DB, Document::new().with("create", coll));
}

#[test]
fn the_open_answers_an_empty_first_batch_and_a_high_water_mark() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let r = open(&mut m, DB, "c".into(), stream(Document::new(), vec![]));
    let cursor = match r.get("cursor") {
        Some(Value::Document(c)) => c.clone(),
        other => panic!("{other:?} in {r:?}"),
    };
    let keys: Vec<&str> = cursor.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["firstBatch", "postBatchResumeToken", "id", "ns"]);
    assert_eq!(field(&r, "cursor.ns"), &Value::from("t.c"));
    assert_ne!(id_of(&r), 0);
    assert!(matches!(token(&r), Position::HighWater(_)));
    let whole = open(&mut m, DB, 1.into(), stream(Document::new(), vec![]));
    assert_eq!(field(&whole, "cursor.ns"), &Value::from("t.$cmd.aggregate"));
    let missing = open(&mut m, DB, "nope".into(), stream(Document::new(), vec![]));
    assert_ne!(id_of(&missing), 0, "a collection that does not exist yet can be watched");
}

#[test]
fn events_come_in_commit_order_with_their_document_keys() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let r = open(&mut m, DB, "c".into(), stream(Document::new(), vec![]));
    let id = id_of(&r);
    insert(&mut m, "c", vec![doc(&[("_id", 1.into()), ("a", 1.into())])]);
    let set = Document::new().with("$set", doc(&[("b", 2.into())]));
    let u = Document::new().with("q", doc(&[("_id", 1.into())])).with("u", set);
    m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])));
    let d = Document::new().with("q", doc(&[("_id", 1.into())])).with("limit", 1);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![d])));
    let r = get_more(&mut m, DB, id, "c", Document::new());
    let cursor_keys: Vec<String> = match r.get("cursor") {
        Some(Value::Document(c)) => c.iter().map(|(k, _)| k.to_owned()).collect(),
        other => panic!("{other:?}"),
    };
    assert_eq!(cursor_keys, ["id", "ns", "nextBatch", "postBatchResumeToken"]);
    let events = batch(&r);
    let kinds: Vec<&Value> = events.iter().map(|e| field(e, "operationType")).collect();
    assert_eq!(kinds, [&Value::from("insert"), &Value::from("update"), &Value::from("delete")]);
    for e in &events {
        assert_eq!(field(e, "documentKey"), &Value::Document(doc(&[("_id", 1.into())])));
        assert_eq!(
            field(e, "ns"),
            &Value::Document(doc(&[("db", "t".into()), ("coll", "c".into())]))
        );
    }
    assert_eq!(
        field(&events[0], "fullDocument"),
        &Value::Document(doc(&[("_id", 1.into()), ("a", 1.into())]))
    );
    assert!(events[1].get("fullDocument").is_none(), "an update brings no document by default");
    let last = match field(&events[2], "_id._data") {
        Value::String(s) => Position::decode(s).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(token(&r), last, "the token of the last event, at the mark read");
    let empty = get_more(&mut m, DB, id, "c", Document::new().with("maxTimeMS", 10));
    assert!(batch(&empty).is_empty());
    m.now += 5;
    let later = get_more(&mut m, DB, id, "c", Document::new());
    assert!(matches!(token(&later), Position::HighWater(_)), "past the events: a high-water mark");
}

#[test]
fn a_small_batch_leaves_events_waiting_and_resuming_goes_on_after_a_token() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    insert(&mut m, "c", (1..=3).map(|i| doc(&[("_id", i.into())])).collect());
    let one = get_more(&mut m, DB, id, "c", Document::new().with("batchSize", 1));
    let first = batch(&one);
    assert_eq!(first.len(), 1);
    assert!(matches!(token(&one), Position::Event(_)));
    let data = field(&first[0], "_id").clone();
    let resumed =
        open(&mut m, DB, "c".into(), stream(Document::new().with("resumeAfter", data), vec![]));
    let rest = batch(&get_more(&mut m, DB, id_of(&resumed), "c", Document::new()));
    let keys: Vec<&Value> = rest.iter().map(|e| field(e, "documentKey._id")).collect();
    assert_eq!(keys, [&Value::Int32(2), &Value::Int32(3)]);
    let two = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    assert_eq!(two.len(), 2, "the waiting events");
}

#[test]
fn stages_after_the_stream_filter_and_an_id_they_change_kills_the_cursor() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let only_two = vec![Document::new().with("$match", doc(&[("documentKey._id", 2.into())]))];
    let filtered = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), only_two)));
    let no_id = vec![Document::new().with("$project", doc(&[("_id", 0.into())]))];
    let broken = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), no_id)));
    insert(&mut m, "c", (1..=3).map(|i| doc(&[("_id", i.into())])).collect());
    let r = get_more(&mut m, DB, filtered, "c", Document::new());
    let events = batch(&r);
    assert_eq!(events.len(), 1);
    assert_eq!(field(&events[0], "documentKey._id"), &Value::Int32(2));
    let r = get_more(&mut m, DB, broken, "c", Document::new());
    assert_eq!(code(&r), 280, "{r:?}");
    let msg = match r.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        msg.starts_with(
            "Executor error during getMore :: caused by :: Encountered an event whose _id field"
        ),
        "{msg}"
    );
    assert!(msg.ends_with("but found: {}"), "{msg}");
    let again = get_more(&mut m, DB, broken, "c", Document::new());
    assert_eq!(code(&again), 43, "the error killed the cursor");
}

#[test]
fn a_database_stream_sees_its_collections_and_update_lookup_reads_the_document() {
    let mut m = Mem::new();
    create(&mut m, "a");
    create(&mut m, "b");
    let spec = Document::new().with("fullDocument", "updateLookup");
    let id = id_of(&open(&mut m, DB, 1.into(), stream(spec, vec![])));
    let cluster = Document::new().with("allChangesForCluster", true);
    let all = id_of(&open(&mut m, "admin", 1.into(), stream(cluster, vec![])));
    insert(&mut m, "a", vec![doc(&[("_id", 1.into())])]);
    insert(&mut m, "b", vec![doc(&[("_id", 2.into())])]);
    let set = Document::new().with("$set", doc(&[("x", 9.into())]));
    let u = Document::new().with("q", doc(&[("_id", 2.into())])).with("u", set);
    m.run(DB, Document::new().with("update", "b").with("updates", arr(vec![u])));
    // Collections created later get their writes above the wall clock less the clock offsets.
    m.now += 1_000;
    let events = batch(&get_more(&mut m, DB, id, "$cmd.aggregate", Document::new()));
    let colls: Vec<&Value> = events.iter().map(|e| field(e, "ns.coll")).collect();
    assert_eq!(colls, [&Value::from("a"), &Value::from("b"), &Value::from("b")]);
    assert_eq!(
        field(&events[2], "fullDocument"),
        &Value::Document(doc(&[("_id", 2.into()), ("x", 9.into())]))
    );
    let everything = batch(&get_more(&mut m, "admin", all, "$cmd.aggregate", Document::new()));
    assert_eq!(everything.len(), 3);
}

#[test]
fn opens_on_internal_namespaces_views_and_time_series_fail_as_on_a_refrouter() {
    let mut m = Mem::new();
    m.run(
        DB,
        Document::new().with("create", "ts").with("timeseries", doc(&[("timeField", "t".into())])),
    );
    m.run(
        DB,
        Document::new().with("create", "v").with("viewOn", "c").with("pipeline", arr(vec![])),
    );
    let plain = || stream(Document::new(), vec![]);
    let cases: Vec<(&str, Value, Value, i32, &str)> = vec![
        (
            "admin",
            1.into(),
            plain(),
            73,
            "$changeStream may not be opened on the internal admin database",
        ),
        (
            "admin",
            "x".into(),
            plain(),
            73,
            "$changeStream may not be opened on the internal admin database",
        ),
        (
            "config",
            "x".into(),
            plain(),
            73,
            "$changeStream may not be opened on the internal config database",
        ),
        (
            DB,
            "system.views".into(),
            plain(),
            73,
            "$changeStream may not be opened on the internal t.system.views collection",
        ),
        (
            DB,
            "system.buckets.ts".into(),
            plain(),
            73,
            "$changeStream may not be opened on the internal t.system.buckets.ts collection",
        ),
        (DB, "ts".into(), plain(), 115, "Cannot run aggregation on timeseries with namespace t.ts"),
        (DB, "v".into(), plain(), 166, "Namespace t.v is a view, not a collection"),
        (
            DB,
            "c".into(),
            stream(Document::new(), vec![Document::new().with("$out", "z")]),
            20,
            "$out is not permitted in a $changeStream pipeline",
        ),
        (
            DB,
            "c".into(),
            stream(Document::new(), vec![Document::new().with("$changeStreamSplitLargeEvent", 1)]),
            7_182_800,
            "$changeStreamSplitLargeEvent spec should be an empty object",
        ),
        (
            DB,
            "c".into(),
            arr(vec![
                Document::new().with("$match", Document::new()),
                Document::new().with("$changeStream", Document::new()),
            ]),
            40_602,
            "$_internalChangeStreamOplogMatch is only valid as the first stage in a pipeline",
        ),
    ];
    for (db, target, pipeline, want, msg) in cases {
        let r = open(&mut m, db, target.clone(), pipeline);
        assert_eq!(code(&r), want, "{db} {target:?}: {r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{db} {target:?}");
    }
    let r = m.run(
        DB,
        Document::new()
            .with("aggregate", "c")
            .with("pipeline", plain())
            .with("cursor", Document::new())
            .with("readConcern", doc(&[("level", "local".into())])),
    );
    assert_eq!(code(&r), 72);
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(
            "Command aggregate does not support { readConcern: { level: \"local\", provenance: \"clientSupplied\" } } :: caused by :: Aggregation stage $changeStream cannot run with a readConcern other than 'majority'. Current readConcern: local"
        ))
    );
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn old_token(m: &Mem, age_ms: u64) -> Value {
    let ts = teseladb_hlc::Hlc { physical: m.now - age_ms, logical: 0 };
    Value::Document(Document::new().with("_data", Position::HighWater(ts).encode()))
}

#[test]
fn resuming_past_the_history_retention_loses_the_history() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let lost = |m: &mut Mem, spec: Document| {
        let r = open(m, DB, "c".into(), stream(spec, vec![]));
        (code(&r), r.get("errmsg").cloned())
    };
    let msg = Some(Value::from(
        "Resume of change stream was not possible, as the resume point may no longer be in the oplog.",
    ));
    let spec = Document::new().with("resumeAfter", old_token(&m, DAY_MS + 1_000));
    assert_eq!(lost(&mut m, spec), (286, msg.clone()));
    let (time, _) =
        teseladb_hlc::to_timestamp(teseladb_hlc::Hlc { physical: m.now - 2 * DAY_MS, logical: 0 });
    let at = Document::new().with("startAtOperationTime", Value::Timestamp { time, increment: 1 });
    assert_eq!(lost(&mut m, at), (286, msg));
    // Within the window, and with a longer retention, the stream opens.
    let recent = Document::new().with("startAfter", old_token(&m, DAY_MS - 60_000));
    assert_ne!(id_of(&open(&mut m, DB, "c".into(), stream(recent, vec![]))), 0);
    let longer = Document::new().with(
        "setClusterParameter",
        doc(&[("teselaHistoryRetention", doc(&[("seconds", Value::Int32(3 * 86_400))]).into())]),
    );
    let r = m.run("admin", longer);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let spec = Document::new().with("resumeAfter", old_token(&m, DAY_MS + 1_000));
    assert_ne!(id_of(&open(&mut m, DB, "c".into(), stream(spec, vec![]))), 0);
}

#[test]
fn a_cursor_that_falls_behind_the_retention_or_a_failed_read_ends_resumable() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    m.fail_changes = Some(teseladb_wire::error::CommandError::new(
        teseladb_wire::error::code(91).unwrap(),
        "the node is shutting down",
    ));
    let r = get_more(&mut m, DB, id, "c", Document::new());
    assert_eq!(code(&r), 91);
    assert_eq!(
        r.get("errorLabels"),
        Some(&Value::Array(vec![Value::from("ResumableChangeStreamError")])),
        "{r:?}"
    );
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    m.now += DAY_MS + 1_000;
    let r = get_more(&mut m, DB, id, "c", Document::new());
    assert_eq!(code(&r), 286, "{r:?}");
    assert!(r.get("errorLabels").is_none());
}
