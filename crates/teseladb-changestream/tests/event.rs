//! The event documents (014 FR-002): the fields of each type in the order a `refrouter` 9.0.4 returns
//! them, measured on 31-10 with `watch()` from `refshell` (with and without `showExpandedEvents`,
//! `fullDocument: "updateLookup"`, `fullDocumentBeforeChange: "whenAvailable"` and a transaction).

use teseladb_bson::{Document, Value};
use teseladb_changestream::event::{ChangeEvent, Namespace, operation_type};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_hlc::{Hlc, from_timestamp, to_timestamp};

fn event(kind: EventKind) -> ChangeEvent {
    let ts = Hlc { physical: 1_730_198_400_123, logical: 2 };
    let mut key = Document::new();
    key.push("_id", 1);
    ChangeEvent {
        token: ResumeToken { event: EventKey { ts, txn: 0, op: 0, key: b"k".to_vec() }, kind },
        cluster_time: ts,
        wall_ms: 1_730_198_400_123,
        ns: Namespace { db: "shop".into(), coll: Some("orders".into()) },
        collection_uuid: Some([7; 16]),
        document_key: Some(key),
        update_description: None,
        full_document: None,
        before: None,
        txn: None,
    }
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

#[test]
fn an_insert_has_its_full_document_before_its_namespace() {
    let mut e = event(EventKind::Insert);
    e.full_document = Some(Value::Document(Document::new()));
    let d = e.to_document(true);
    let expanded = [
        "_id",
        "operationType",
        "clusterTime",
        "collectionUUID",
        "wallTime",
        "fullDocument",
        "ns",
        "documentKey",
    ];
    assert_eq!(keys(&d), expanded);
    let plain = e.to_document(false);
    assert_eq!(
        keys(&plain),
        ["_id", "operationType", "clusterTime", "wallTime", "fullDocument", "ns", "documentKey"]
    );
    assert_eq!(plain.get("operationType"), Some(&Value::String("insert".into())));
    let Some(Value::Timestamp { time, increment }) = plain.get("clusterTime") else {
        panic!("{plain:?}")
    };
    assert_eq!(from_timestamp((*time, *increment)), e.cluster_time);
    assert_eq!((*time, *increment), to_timestamp(e.cluster_time));
    let Some(Value::Document(id)) = plain.get("_id") else { panic!("{plain:?}") };
    let Some(Value::String(data)) = id.get("_data") else { panic!("{id:?}") };
    assert_eq!(ResumeToken::decode(data), Ok(e.token.clone()));
}

#[test]
fn an_update_has_its_description_and_then_the_documents_asked_for() {
    let mut e = event(EventKind::Update);
    let mut desc = Document::new();
    desc.push("updatedFields", Document::new());
    e.update_description = Some(desc);
    e.full_document = Some(Value::Document(Document::new()));
    let tail = ["ns", "documentKey", "updateDescription", "fullDocument"];
    assert_eq!(keys(&e.to_document(true))[5..], tail);
    e.full_document = None;
    e.before = Some(Value::Null);
    let mut lsid = Document::new();
    lsid.push("id", Value::Binary { subtype: 4, bytes: vec![1; 16] });
    e.txn = Some((lsid, 7));
    let d = e.to_document(false);
    let measured = [
        "txnNumber",
        "lsid",
        "_id",
        "operationType",
        "clusterTime",
        "wallTime",
        "ns",
        "documentKey",
        "updateDescription",
        "fullDocumentBeforeChange",
    ];
    assert_eq!(keys(&d), measured);
    assert_eq!(d.get("txnNumber"), Some(&Value::Int64(7)));
}

#[test]
fn a_deletion_and_a_database_event_carry_what_they_have() {
    let d = event(EventKind::Delete).to_document(true);
    assert_eq!(
        keys(&d),
        ["_id", "operationType", "clusterTime", "collectionUUID", "wallTime", "ns", "documentKey"]
    );
    let mut drop_db = event(EventKind::DropDatabase);
    drop_db.ns.coll = None;
    drop_db.document_key = None;
    drop_db.collection_uuid = None;
    let d = drop_db.to_document(true);
    assert_eq!(keys(&d), ["_id", "operationType", "clusterTime", "wallTime", "ns"]);
    let Some(Value::Document(ns)) = d.get("ns") else { panic!("{d:?}") };
    assert_eq!(keys(ns), ["db"]);
}

#[test]
fn every_type_has_refdb_s_operation_type() {
    let names = [
        (EventKind::Insert, "insert"),
        (EventKind::Update, "update"),
        (EventKind::Replace, "replace"),
        (EventKind::Delete, "delete"),
        (EventKind::Drop, "drop"),
        (EventKind::Rename, "rename"),
        (EventKind::DropDatabase, "dropDatabase"),
        (EventKind::Invalidate, "invalidate"),
        (EventKind::Create, "create"),
        (EventKind::CreateIndexes, "createIndexes"),
        (EventKind::DropIndexes, "dropIndexes"),
        (EventKind::Modify, "modify"),
        (EventKind::ShardCollection, "shardCollection"),
        (EventKind::RefineCollectionShardKey, "refineCollectionShardKey"),
        (EventKind::ReshardCollection, "reshardCollection"),
    ];
    for (kind, name) in names {
        assert_eq!(operation_type(kind), name);
        assert_eq!(event(kind).kind(), kind);
    }
}
