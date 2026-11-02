//! 014 T017: the catalog events of `$changeStream` over the in-memory node of `common` (FR-002,
//! FR-014): `create`, `createIndexes`, `modify`, `dropIndexes`, `rename`, `drop`,
//! `dropDatabase` and `invalidate`, merged with the documents' events by their mark, as a
//! `refrouter` 9.0.4 gives them (measured on 01-11).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_changestream::token::{EventKind, Position};

const DB: &str = "ddl";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn open(m: &mut Mem, db: &str, target: Value, spec: Document, rest: Vec<Document>) -> Document {
    let mut stages = vec![Document::new().with("$changeStream", spec)];
    stages.extend(rest);
    let cmd = Document::new()
        .with("aggregate", target)
        .with("pipeline", arr(stages))
        .with("cursor", Document::new());
    m.run(db, cmd)
}

fn id_of(r: &Document) -> i64 {
    match field(r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

fn get_more(m: &mut Mem, db: &str, id: i64, coll: &str) -> Document {
    m.run(db, Document::new().with("getMore", id).with("collection", coll).with("maxTimeMS", 1))
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

fn types(events: &[Document]) -> Vec<String> {
    events
        .iter()
        .map(|e| match field(e, "operationType") {
            Value::String(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

/// Runs `cmd` on `db` a millisecond later, so that each step has its own mark.
fn step(m: &mut Mem, db: &str, cmd: Document) -> Document {
    m.now += 1;
    let r = m.run(db, cmd);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    r
}

fn insert(m: &mut Mem, coll: &str, id: i32) {
    let docs = arr(vec![doc(&[("_id", id.into()), ("a", id.into())])]);
    step(m, DB, Document::new().with("insert", coll).with("documents", docs));
}

fn token_of(e: &Document) -> Position {
    match field(e, "_id._data") {
        Value::String(s) => Position::decode(s).unwrap(),
        other => panic!("{other:?}"),
    }
}

fn pbrt(r: &Document) -> Position {
    match field(r, "cursor.postBatchResumeToken._data") {
        Value::String(s) => Position::decode(s).unwrap(),
        other => panic!("{other:?}"),
    }
}

fn index(name: &str, key: Document) -> Document {
    Document::new().with("key", key).with("name", name)
}

#[test]
fn a_database_stream_with_expanded_events_sees_the_catalog_in_order() {
    let mut m = Mem::new();
    let all =
        open(&mut m, "admin", 1.into(), Document::new().with("allChangesForCluster", true), vec![]);
    let exp = Document::new().with("showExpandedEvents", true);
    let id = id_of(&open(&mut m, DB, 1.into(), exp, vec![]));
    let plain = id_of(&open(&mut m, DB, 1.into(), Document::new(), vec![]));
    step(&mut m, DB, Document::new().with("create", "c"));
    insert(&mut m, "c", 1);
    let ix = arr(vec![index("a_1", doc(&[("a", 1.into())]))]);
    step(&mut m, DB, Document::new().with("createIndexes", "c").with("indexes", ix));
    let validator = doc(&[("a", Value::Document(doc(&[("$exists", true.into())])))]);
    step(&mut m, DB, Document::new().with("collMod", "c").with("validator", validator.clone()));
    let hide = doc(&[("name", "a_1".into()), ("hidden", true.into())]);
    step(&mut m, DB, Document::new().with("collMod", "c").with("index", hide));
    step(&mut m, DB, Document::new().with("dropIndexes", "c").with("index", "a_1"));
    insert(&mut m, "c", 2);
    let rename = Document::new().with("renameCollection", "ddl.c").with("to", "ddl.r");
    step(&mut m, "admin", rename);
    insert(&mut m, "r", 3);
    let view =
        Document::new().with("create", "v").with("viewOn", "r").with("pipeline", arr(vec![]));
    step(&mut m, DB, view);
    step(&mut m, DB, Document::new().with("drop", "r"));
    step(&mut m, DB, Document::new().with("dropDatabase", 1));
    m.now += 1;
    let r = get_more(&mut m, DB, id, "$cmd.aggregate");
    let events = batch(&r);
    assert_eq!(
        types(&events),
        [
            "create",
            "insert",
            "createIndexes",
            "modify",
            "modify",
            "dropIndexes",
            "insert",
            "rename",
            "insert",
            "create",
            "drop",
            "dropDatabase",
            "invalidate"
        ]
    );
    assert_eq!(id_of(&r), 0, "the invalidate closes the cursor");
    assert_eq!(pbrt(&r), token_of(&events[12]));
    let create = &events[0];
    assert_eq!(
        keys(create),
        [
            "_id",
            "operationType",
            "clusterTime",
            "collectionUUID",
            "wallTime",
            "ns",
            "operationDescription",
            "nsType"
        ]
    );
    let id_index = doc(&[
        ("v", 2.into()),
        ("key", Value::Document(doc(&[("_id", 1.into())]))),
        ("name", "_id_".into()),
    ]);
    assert_eq!(
        field(create, "operationDescription"),
        &Value::Document(Document::new().with("idIndex", id_index))
    );
    assert_eq!(field(create, "nsType"), &Value::from("collection"));
    let ix_spec = field(&events[2], "operationDescription.indexes");
    assert!(matches!(ix_spec, Value::Array(a) if a.len() == 1), "{ix_spec:?}");
    assert_eq!(
        field(&events[3], "operationDescription"),
        &Value::Document(Document::new().with("validator", validator))
    );
    assert_eq!(
        keys(match field(&events[3], "stateBeforeChange.collectionOptions") {
            Value::Document(d) => d,
            other => panic!("{other:?}"),
        }),
        ["uuid"]
    );
    assert_eq!(field(&events[4], "operationDescription.index.hidden"), &Value::Boolean(true));
    assert_eq!(field(&events[4], "stateBeforeChange.indexOptions.hidden"), &Value::Boolean(false));
    let ren = &events[7];
    assert_eq!(keys(ren)[0], "to");
    assert_eq!(field(ren, "operationDescription.to.coll"), &Value::from("r"));
    assert_eq!(field(&events[8], "ns.coll"), &Value::from("r"), "after the rename, the new name");
    assert_eq!(field(&events[6], "ns.coll"), &Value::from("c"), "before it, the old one");
    assert_eq!(field(&events[9], "nsType"), &Value::from("view"));
    assert!(events[9].get("collectionUUID").is_none());
    assert_eq!(field(&events[11], "ns"), &Value::Document(doc(&[("db", DB.into())])));
    assert!(events[12].get("ns").is_none());
    // Without `showExpandedEvents`: the documents, `rename`, `drop`, `dropDatabase`.
    let r = get_more(&mut m, DB, plain, "$cmd.aggregate");
    assert_eq!(
        types(&batch(&r)),
        ["insert", "insert", "rename", "insert", "drop", "dropDatabase", "invalidate"]
    );
    assert!(batch(&r)[2].get("collectionUUID").is_none());
    // The cluster's stream is not invalidated.
    let r = get_more(&mut m, "admin", id_of(&all), "$cmd.aggregate");
    let t = types(&batch(&r));
    assert_eq!(t.last().map(String::as_str), Some("dropDatabase"));
    assert_ne!(id_of(&r), 0);
}

#[test]
fn a_drop_keeps_the_events_written_before_it_and_invalidates_the_collection_stream() {
    let mut m = Mem::new();
    m.run(DB, Document::new().with("create", "c"));
    m.now += 1;
    let r = open(&mut m, DB, "c".into(), Document::new(), vec![]);
    let id = id_of(&r);
    insert(&mut m, "c", 1);
    assert_eq!(types(&batch(&get_more(&mut m, DB, id, "c"))), ["insert"]);
    // Written after the last read, then dropped: the event is still there.
    insert(&mut m, "c", 2);
    step(&mut m, DB, Document::new().with("drop", "c"));
    m.now += 1;
    let r = get_more(&mut m, DB, id, "c");
    let events = batch(&r);
    assert_eq!(types(&events), ["insert", "drop", "invalidate"]);
    assert_eq!(field(&events[0], "documentKey._id"), &Value::Int32(2));
    assert_eq!(id_of(&r), 0);
    let drop = field(&events[1], "_id").clone();
    let inv = field(&events[2], "_id").clone();
    match (token_of(&events[1]), token_of(&events[2])) {
        (Position::Event(d), Position::Event(i)) => {
            assert_eq!((d.kind, i.kind), (EventKind::Drop, EventKind::Invalidate));
            assert_eq!(d.event, i.event, "the invalidate has the drop's place");
        }
        other => panic!("{other:?}"),
    }
    // resumeAfter the invalidate fails; startAfter it goes on with the new collection.
    let e = open(&mut m, DB, "c".into(), Document::new().with("resumeAfter", inv.clone()), vec![]);
    assert_eq!(e.get("code"), Some(&Value::Int32(260)), "{e:?}");
    let after =
        id_of(&open(&mut m, DB, "c".into(), Document::new().with("startAfter", inv), vec![]));
    step(&mut m, DB, Document::new().with("create", "c"));
    insert(&mut m, "c", 9);
    m.now += 1;
    let r = get_more(&mut m, DB, after, "c");
    assert_eq!(types(&batch(&r)), ["insert"]);
    assert_eq!(field(&batch(&r)[0], "documentKey._id"), &Value::Int32(9));
    // resumeAfter or startAfter the drop: the invalidate at once.
    for how in ["resumeAfter", "startAfter"] {
        let r = open(&mut m, DB, "c".into(), Document::new().with(how, drop.clone()), vec![]);
        let more = get_more(&mut m, DB, id_of(&r), "c");
        assert_eq!(types(&batch(&more)), ["invalidate"], "{how}");
        assert_eq!(id_of(&more), 0);
    }
}

#[test]
fn a_rename_onto_a_watched_collection_names_the_target_and_a_filtered_invalidate_still_closes() {
    let mut m = Mem::new();
    m.run(DB, Document::new().with("create", "c"));
    m.run(DB, Document::new().with("create", "t"));
    m.now += 1;
    let watch = id_of(&open(
        &mut m,
        DB,
        "c".into(),
        Document::new().with("showExpandedEvents", true),
        vec![],
    ));
    let only_inserts = Document::new().with("$match", doc(&[("operationType", "insert".into())]));
    let filtered = id_of(&open(&mut m, DB, "c".into(), Document::new(), vec![only_inserts]));
    insert(&mut m, "c", 1);
    let rename = Document::new()
        .with("renameCollection", "ddl.t")
        .with("to", "ddl.c")
        .with("dropTarget", true);
    step(&mut m, "admin", rename);
    m.now += 1;
    let r = get_more(&mut m, DB, watch, "c");
    let events = batch(&r);
    assert_eq!(types(&events), ["insert", "rename", "invalidate"]);
    assert_eq!(field(&events[1], "ns.coll"), &Value::from("t"));
    assert!(matches!(field(&events[1], "operationDescription.dropTarget"), Value::Binary { .. }));
    let r = get_more(&mut m, DB, filtered, "c");
    assert_eq!(types(&batch(&r)), ["insert"]);
    assert_eq!(id_of(&r), 0, "the invalidate the stages dropped still closes the cursor");
    assert!(matches!(pbrt(&r), Position::Event(t) if t.kind == EventKind::Invalidate));
}

#[test]
fn a_change_older_than_the_catalog_log_keeps_is_history_lost() {
    let mut m = Mem::new();
    m.run(DB, Document::new().with("create", "c"));
    m.now += 10;
    let id = id_of(&open(&mut m, DB, "c".into(), Document::new(), vec![]));
    m.ddl.floor = teseladb_hlc::Hlc { physical: m.now + 1, logical: 0 };
    m.now += 5;
    let r = get_more(&mut m, DB, id, "c");
    assert_eq!(r.get("code"), Some(&Value::Int32(286)), "{r:?}");
    let label = Value::Array(vec![Value::from("NonResumableChangeStreamError")]);
    assert_eq!(r.get("errorLabels"), Some(&label));
}

#[test]
fn a_rename_onto_the_watched_collection_ends_its_stream() {
    let mut m = Mem::new();
    step(&mut m, DB, Document::new().with("create", "c"));
    step(&mut m, DB, Document::new().with("create", "r"));
    let expanded = Document::new().with("showExpandedEvents", true);
    let id = id_of(&open(&mut m, DB, "r".into(), expanded.clone(), vec![]));
    let other = id_of(&open(&mut m, DB, "r".into(), expanded, vec![]));
    // The catalog changes of another collection are not this stream's.
    step(&mut m, DB, Document::new().with("create", "d"));
    let rename = Document::new()
        .with("renameCollection", format!("{DB}.c"))
        .with("to", format!("{DB}.r"))
        .with("dropTarget", true);
    step(&mut m, "admin", rename);
    let got = batch(&get_more(&mut m, DB, id, "r"));
    assert_eq!(types(&got), ["rename", "invalidate"], "{got:?}");
    assert_eq!(types(&batch(&get_more(&mut m, DB, other, "r"))), ["rename", "invalidate"]);
}

#[test]
fn a_view_s_drop_carries_no_collection_uuid() {
    let mut m = Mem::new();
    step(&mut m, DB, Document::new().with("create", "c"));
    let expanded = Document::new().with("showExpandedEvents", true);
    let id = id_of(&open(&mut m, DB, 1.into(), expanded, vec![]));
    let view =
        Document::new().with("create", "v").with("viewOn", "c").with("pipeline", arr(vec![]));
    step(&mut m, DB, view);
    step(&mut m, DB, Document::new().with("drop", "v"));
    let got = batch(&get_more(&mut m, DB, id, "$cmd.aggregate"));
    assert_eq!(types(&got), ["create", "drop"], "{got:?}");
    assert!(got[1].get("collectionUUID").is_none(), "{:?}", got[1]);
}

#[test]
fn a_database_stream_follows_a_renamed_collection_and_leaves_out_other_databases() {
    let mut m = Mem::new();
    step(&mut m, DB, Document::new().with("create", "c"));
    let id = id_of(&open(&mut m, DB, 1.into(), Document::new(), vec![]));
    insert(&mut m, "c", 1);
    let rename =
        Document::new().with("renameCollection", format!("{DB}.c")).with("to", format!("{DB}.r"));
    step(&mut m, "admin", rename);
    insert(&mut m, "r", 2);
    let other = arr(vec![doc(&[("_id", 3.into())])]);
    step(&mut m, "elsewhere", Document::new().with("insert", "c").with("documents", other));
    let got = batch(&get_more(&mut m, DB, id, "$cmd.aggregate"));
    assert_eq!(types(&got), ["insert", "rename", "insert"], "{got:?}");
    assert_eq!(field(&got[0], "ns.coll"), &Value::from("c"));
    assert_eq!(field(&got[2], "ns.coll"), &Value::from("r"));
}
