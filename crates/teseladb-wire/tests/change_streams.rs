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

/// A stream resumed after an event its own `$match` leaves out cannot find where to resume: its
/// first `getMore` fails with 280 and kills the cursor; one that keeps the event resumes (measured
/// on a `refrouter` 9.0.4 by the battery's `cs-resume-after`, 04-11-2026).
#[test]
fn resuming_after_an_event_the_stages_leave_out_is_fatal() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    insert(&mut m, "c", (1..=3).map(|i| doc(&[("_id", i.into())])).collect());
    let first = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    let data = field(&first[0], "_id").clone();
    let only = |k: i32| vec![Document::new().with("$match", doc(&[("documentKey._id", k.into())]))];
    let spec = Document::new().with("resumeAfter", data.clone());
    let lost = open(&mut m, DB, "c".into(), stream(spec.clone(), only(3)));
    assert_eq!(field(&lost, "ok"), &Value::Double(1.0));
    let r = get_more(&mut m, DB, id_of(&lost), "c", Document::new());
    assert_eq!(code(&r), 280, "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    let Value::Document(t) = &data else { panic!("{data:?}") };
    let Some(Value::String(hex)) = t.get("_data") else { panic!("{t:?}") };
    let want =
        format!("cannot resume stream; the resume token was not found. {{_data: \"{hex}\"}}");
    assert_eq!(msg, &want);
    assert_eq!(code(&get_more(&mut m, DB, id_of(&lost), "c", Document::new())), 43);
    // A `$match` that keeps the event resumes after it.
    let kept = open(&mut m, DB, "c".into(), stream(spec, only(1)));
    let r = get_more(&mut m, DB, id_of(&kept), "c", Document::new());
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
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
            Value::Array(vec![
                Value::Document(Document::new().with("$changeStream", Document::new())),
                Value::Int32(1),
            ]),
            14,
            "Each element of the 'pipeline' array must be an object",
        ),
        (
            DB,
            "c".into(),
            stream(Document::new(), vec![Document::new()]),
            40_323,
            "A pipeline stage specification object must contain exactly one field.",
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
    // `majority` is the stream's own read concern.
    let r = m.run(
        DB,
        Document::new()
            .with("aggregate", "c")
            .with("pipeline", plain())
            .with("cursor", Document::new())
            .with("readConcern", doc(&[("level", "majority".into())])),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn old_token(m: &Mem, age_ms: u64) -> Value {
    let ts = teseladb_hlc::Hlc { physical: m.now - age_ms, logical: 0 };
    Value::Document(Document::new().with("_data", Position::HighWater(ts).encode()))
}

#[test]
fn resuming_past_the_history_retention_loses_the_history() {
    // Measured on 01-11 after the oplog rolled over: the open succeeds, the `getMore` fails.
    let mut m = Mem::new();
    create(&mut m, "c");
    let lost = |m: &mut Mem, spec: Document| {
        let id = id_of(&open(m, DB, "c".into(), stream(spec, vec![])));
        let r = get_more(m, DB, id, "c", Document::new());
        (code(&r), r.get("errmsg").cloned(), r.get("errorLabels").cloned())
    };
    let message = |ts: teseladb_hlc::Hlc| {
        let (t, i) = teseladb_hlc::to_timestamp(ts);
        Some(Value::from(format!(
            "Executor error during getMore :: caused by :: Resume of change stream was not possible, as the resume point may no longer be in the oplog (resumeTimestamp: Timestamp({t}, {i}))"
        )))
    };
    let label = Some(Value::Array(vec![Value::from("NonResumableChangeStreamError")]));
    let old = teseladb_hlc::Hlc { physical: m.now - DAY_MS - 1_000, logical: 0 };
    let spec = Document::new().with("resumeAfter", old_token(&m, DAY_MS + 1_000));
    assert_eq!(lost(&mut m, spec), (286, message(old), label.clone()));
    let (time, _) =
        teseladb_hlc::to_timestamp(teseladb_hlc::Hlc { physical: m.now - 2 * DAY_MS, logical: 0 });
    let at = Document::new().with("startAtOperationTime", Value::Timestamp { time, increment: 1 });
    let at_ts = teseladb_hlc::from_timestamp((time, 1));
    assert_eq!(lost(&mut m, at), (286, message(at_ts), label));
    // Within the window, and with a longer retention, the stream reads.
    let read = |m: &mut Mem, spec: Document| {
        let id = id_of(&open(m, DB, "c".into(), stream(spec, vec![])));
        get_more(m, DB, id, "c", Document::new()).get("ok").cloned()
    };
    let recent = Document::new().with("startAfter", old_token(&m, DAY_MS - 60_000));
    assert_eq!(read(&mut m, recent), Some(Value::Double(1.0)));
    let longer = Document::new().with(
        "setClusterParameter",
        doc(&[("teselaHistoryRetention", doc(&[("seconds", Value::Int32(3 * 86_400))]).into())]),
    );
    let r = m.run("admin", longer);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let spec = Document::new().with("resumeAfter", old_token(&m, DAY_MS + 1_000));
    assert_eq!(read(&mut m, spec), Some(Value::Double(1.0)));
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
    let label = Value::Array(vec![Value::from("NonResumableChangeStreamError")]);
    assert_eq!(r.get("errorLabels"), Some(&label));
}

#[test]
fn a_get_more_returns_every_event_written_before_it_started() {
    // The resolved mark goes 200 ms behind the clock (014 FR-004); a `refrouter` returns every event
    // written before the `getMore`, so the batch waits until the mark passes its start.
    let mut m = Mem::new();
    m.resolved_lag_ms = 200;
    m.pause_ms = 50;
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    m.now += 10;
    insert(&mut m, "c", vec![doc(&[("_id", 1.into())])]);
    m.now += 150;
    insert(&mut m, "c", vec![doc(&[("_id", 2.into())])]);
    m.now += 100;
    m.pauses_left = 20;
    let started = m.now;
    let both = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    let keys: Vec<&Value> = both.iter().map(|e| field(e, "documentKey._id")).collect();
    assert_eq!(keys, [&Value::Int32(1), &Value::Int32(2)]);
    assert!(m.now - started <= 250, "the wait stops once the mark passes the start");
    // A full batch answers at once, and the wait never goes past `maxTimeMS`.
    insert(&mut m, "c", (3..=5).map(|i| doc(&[("_id", i.into())])).collect());
    let at = m.now;
    let one = batch(&get_more(&mut m, DB, id, "c", Document::new().with("batchSize", 1)));
    assert_eq!(one.len(), 1);
    assert_eq!(m.now, at + 200, "the batch fills once the mark passes the writes");
    m.now += 10;
    insert(&mut m, "c", vec![doc(&[("_id", 6.into())])]);
    let at = m.now;
    let short = batch(&get_more(&mut m, DB, id, "c", Document::new().with("maxTimeMS", 100)));
    assert!(m.now - at <= 150, "bounded by maxTimeMS");
    assert!(short.len() >= 2, "{short:?}");
}

#[test]
fn a_first_match_on_namespaces_or_catalog_events_reads_fewer_ranges() {
    // 014 FR-012: the ranges of the collections the `$match` leaves out are not read, and the
    // events are those the whole `$match` lets through.
    let mut m = Mem::new();
    create(&mut m, "a");
    create(&mut m, "b");
    let only_a = Document::new().with("$match", Document::new().with("ns.coll", "a"));
    let id = id_of(&open(&mut m, DB, 1.into(), stream(Document::new(), vec![only_a])));
    let all = id_of(&open(&mut m, DB, 1.into(), stream(Document::new(), vec![])));
    let drops = Document::new().with("$match", Document::new().with("operationType", "drop"));
    let ddl = id_of(&open(&mut m, DB, 1.into(), stream(Document::new(), vec![drops])));
    m.now += 10;
    insert(&mut m, "a", vec![doc(&[("_id", 1.into())])]);
    insert(&mut m, "b", vec![doc(&[("_id", 2.into())])]);
    m.now += 10;
    let before = m.change_reads;
    let got = batch(&get_more(&mut m, DB, id, "$cmd.aggregate", Document::new()));
    assert_eq!(m.change_reads - before, 1, "only the ranges of `a`");
    let colls: Vec<&Value> = got.iter().map(|e| field(e, "ns.coll")).collect();
    assert_eq!(colls, [&Value::from("a")]);
    let before = m.change_reads;
    assert_eq!(batch(&get_more(&mut m, DB, all, "$cmd.aggregate", Document::new())).len(), 2);
    assert_eq!(m.change_reads - before, 2);
    let before = m.change_reads;
    assert!(batch(&get_more(&mut m, DB, ddl, "$cmd.aggregate", Document::new())).is_empty());
    assert_eq!(m.change_reads, before, "catalog events only: no document reads");
}

#[test]
fn the_node_counts_its_open_streams_and_the_events_they_return() {
    // 014 FR-018 (other tests of this binary open streams at the same time: only lower bounds).
    use teseladb_wire::commands::change_stream::stream_metrics;
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    let (open_now, events) = stream_metrics();
    assert!(open_now >= 1);
    m.now += 10;
    insert(&mut m, "c", vec![doc(&[("_id", 1.into())]), doc(&[("_id", 2.into())])]);
    m.now += 10;
    assert_eq!(batch(&get_more(&mut m, DB, id, "c", Document::new())).len(), 2);
    assert!(stream_metrics().1 >= events + 2);
}

#[test]
fn a_batch_holds_events_up_to_the_reply_s_bytes() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    let big = "x".repeat(9 * 1024 * 1024);
    for n in 1..=2 {
        insert(&mut m, "c", vec![doc(&[("_id", n.into()), ("a", big.as_str().into())])]);
    }
    // Two events of 9 MiB do not fit in one reply of 16 MiB: the second waits for the next.
    for want in [1, 2] {
        let got = batch(&get_more(&mut m, DB, id, "c", Document::new()));
        assert_eq!(got.len(), 1);
        assert_eq!(field(&got[0], "documentKey._id"), &Value::Int32(want));
    }
}

#[test]
fn a_cluster_stream_leaves_out_the_internal_databases() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let spec = Document::new().with("allChangesForCluster", true);
    let id = id_of(&open(&mut m, "admin", 1.into(), stream(spec, vec![])));
    let r = m.run(
        "admin",
        Document::new().with("insert", "x").with("documents", arr(vec![doc(&[("_id", 1.into())])])),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    insert(&mut m, "c", vec![doc(&[("_id", 2.into())])]);
    let got = batch(&get_more(&mut m, "admin", id, "$cmd.aggregate", Document::new()));
    let keys: Vec<&Value> = got.iter().map(|e| field(e, "documentKey._id")).collect();
    assert_eq!(keys, [&Value::Int32(2)], "{got:?}");
}

fn errmsg(r: &Document) -> String {
    match r.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// Measured on a `refrouter` 9.0.4 on 02-11: an expression that fails on an event fails the
/// `getMore`, a long `_id` is cut in the message, and `$rand` gives a number.
#[test]
fn the_stages_errors_and_numbers_are_those_of_a_refrouter() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let mut open_with = |stage: Document| {
        id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![stage])))
    };
    let long_id = open_with(
        Document::new().with("$addFields", doc(&[("_id", "x".repeat(200).as_str().into())])),
    );
    let divide = Document::new().with(
        "$divide",
        Value::Array(vec![
            Value::Int32(1),
            Value::Document(Document::new().with(
                "$subtract",
                Value::Array(vec![Value::from("$documentKey._id"), Value::Int32(1)]),
            )),
        ]),
    );
    let by_zero = open_with(Document::new().with("$addFields", Document::new().with("q", divide)));
    let rand = Document::new().with("r", Document::new().with("$rand", Document::new()));
    let random = open_with(Document::new().with("$addFields", rand));
    insert(&mut m, "c", vec![doc(&[("_id", 1.into())])]);
    let r = get_more(&mut m, DB, long_id, "c", Document::new());
    assert_eq!(code(&r), 280, "{r:?}");
    let cut = format!("but found: {{ _id: \"{}...\" }}", "x".repeat(150));
    assert!(errmsg(&r).ends_with(&cut), "{}", errmsg(&r));
    let r = get_more(&mut m, DB, by_zero, "c", Document::new());
    assert_eq!(code(&r), 2, "{r:?}");
    assert!(
        errmsg(&r).ends_with("Executor error during getMore :: caused by :: can't $divide by zero"),
        "{}",
        errmsg(&r)
    );
    let got = batch(&get_more(&mut m, DB, random, "c", Document::new()));
    assert!(matches!(field(&got[0], "r"), Value::Double(x) if (0.0..1.0).contains(x)), "{got:?}");
}

/// Measured on a `refrouter` 9.0.4 on 02-11: `$changeStreamSplitLargeEvent` takes only `{}`.
#[test]
fn the_split_stage_takes_an_empty_object_only() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let split = Document::new().with("$changeStreamSplitLargeEvent", doc(&[("a", 1.into())]));
    let r = open(&mut m, DB, "c".into(), stream(Document::new(), vec![split]));
    assert_eq!(code(&r), 7_182_800, "{r:?}");
    assert_eq!(errmsg(&r), "$changeStreamSplitLargeEvent spec should be an empty object");
}

/// A `$match` on a geometry after `$changeStream` goes through the node's geometry, as a
/// `refrouter` 9.0.4 filters `fullDocument` with `$geoWithin`.
#[test]
fn a_geometry_filter_after_the_stream_keeps_the_events_inside() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let square = r#"{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}"#;
    let filter = teseladb_bson::extjson::parse(&format!(
        r#"{{"$match":{{"fullDocument.p":{{"$geoWithin":{{"$geometry":{square}}}}}}}}}"#
    ))
    .expect("stage");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![filter])));
    let point = |x: f64| {
        Document::new()
            .with("type", "Point")
            .with("coordinates", Value::Array(vec![x.into(), 1.0.into()]))
    };
    insert(&mut m, "c", vec![doc(&[("_id", 1.into())]).with("p", point(1.0))]);
    insert(&mut m, "c", vec![doc(&[("_id", 2.into())]).with("p", point(5.0))]);
    let got = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(field(&got[0], "documentKey._id"), &Value::Int32(1));
}

/// A stream on a collection renamed into another database, opened at a mark before the
/// rename, sees the events written under its old name.
#[test]
fn a_stream_opened_before_a_rename_to_another_database_sees_the_old_name_s_events() {
    let mut m = Mem::new();
    create(&mut m, "c");
    m.now += 1;
    let at = m.now;
    insert(&mut m, "c", vec![doc(&[("_id", 1.into())])]);
    m.now += 1;
    let rename = Document::new().with("renameCollection", format!("{DB}.c")).with("to", "other.d");
    let r = m.run("admin", rename);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let time = u32::try_from(at / 1000).expect("seconds");
    let spec =
        Document::new().with("startAtOperationTime", Value::Timestamp { time, increment: 0 });
    let r = open(&mut m, DB, "c".into(), stream(spec, vec![]));
    let id = id_of(&r);
    // A write under the new name is not this stream's.
    let r = m.run(
        "other",
        Document::new().with("insert", "d").with("documents", arr(vec![doc(&[("_id", 2.into())])])),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    // Nor a write to another collection of the database.
    create(&mut m, "o");
    insert(&mut m, "o", vec![doc(&[("_id", 3.into())])]);
    let got = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    let kinds: Vec<&Value> = got.iter().map(|e| field(e, "operationType")).collect();
    assert_eq!(kinds.first(), Some(&&Value::String("insert".into())), "{got:?}");
    let inserted: Vec<&Value> = got
        .iter()
        .filter(|e| field(e, "operationType") == &Value::from("insert"))
        .map(|e| field(e, "documentKey._id"))
        .collect();
    assert_eq!(inserted, [&Value::Int32(1)], "{got:?}");
}

/// An event larger than a reply's 16 MiB goes alone in its batch, as a `refrouter` 9.0.4 returns
/// an event of up to 16 MiB and 16 KiB (measured on 01-11).
#[test]
fn an_event_over_the_reply_s_bytes_goes_alone() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    let big = "x".repeat(16 * 1024 * 1024 - 64);
    insert(&mut m, "c", vec![doc(&[("_id", 1.into()), ("a", big.as_str().into())])]);
    let got = batch(&get_more(&mut m, DB, id, "c", Document::new()));
    assert_eq!(got.len(), 1);
    assert_eq!(field(&got[0], "documentKey._id"), &Value::Int32(1));
}

/// A drop seen one event per batch: the `drop` first, the `invalidate` next, and the cursor
/// closes at the `getMore` after it.
#[test]
fn a_drop_read_one_event_at_a_time_ends_with_the_invalidate() {
    let mut m = Mem::new();
    create(&mut m, "c");
    let id = id_of(&open(&mut m, DB, "c".into(), stream(Document::new(), vec![])));
    m.now += 1;
    let r = m.run(DB, Document::new().with("drop", "c"));
    m.now += 1;
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let one = Document::new().with("batchSize", 1);
    let first = get_more(&mut m, DB, id, "c", one.clone());
    assert_eq!(field(&batch(&first)[0], "operationType"), &Value::from("drop"));
    let second = get_more(&mut m, DB, id, "c", one);
    assert_eq!(field(&batch(&second)[0], "operationType"), &Value::from("invalidate"));
    // Measured on a `refrouter` 9.0.4 on 02-11: the batch with the `invalidate` keeps the cursor
    // open, and the next `getMore` closes it with an empty batch.
    assert_ne!(field(&second, "cursor.id"), &Value::Int64(0));
    let last = get_more(&mut m, DB, id, "c", Document::new());
    assert!(batch(&last).is_empty());
    assert_eq!(field(&last, "cursor.id"), &Value::Int64(0));
}
