//! The event documents (014 FR-002): the fields of each type in the order a `refrouter` 9.0.4 returns
//! them, measured on 31-10 with `watch()` from `refshell` (with and without `showExpandedEvents`,
//! `fullDocument: "updateLookup"`, `fullDocumentBeforeChange: "whenAvailable"` and a transaction).

use teseladb_changestream::event::{ChangeEvent, Namespace, operation_type};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_hlc::{Hlc, from_timestamp, to_timestamp};
use teseladb_bson::{Document, Value};

fn event(kind: EventKind) -> ChangeEvent {
    let ts = Hlc { physical: 1_730_198_400_123, logical: 2 };
    let mut key = Document::new();
    key.push("_id", 1);
    ChangeEvent {
        token: ResumeToken::new(EventKey { ts, txn: 0, op: 0, key: b"k".to_vec() }, kind),
        cluster_time: ts,
        wall_ms: 1_730_198_400_123,
        ns: Namespace { db: "shop".into(), coll: Some("orders".into()) },
        collection_uuid: Some([7; 16]),
        document_key: Some(key),
        update_description: None,
        full_document: None,
        before: None,
        txn: None,
        to: None,
        operation_description: None,
        state_before: None,
        ns_type: None,
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
    // Both: the pre-image first (measured on 01-11); a replacement's comes last.
    e.full_document = Some(Value::Null);
    let both = e.to_document(false);
    assert_eq!(keys(&both)[8..], ["updateDescription", "fullDocumentBeforeChange", "fullDocument"]);
    let mut r = event(EventKind::Replace);
    r.full_document = Some(Value::Document(Document::new()));
    r.before = Some(Value::Null);
    let tail = ["fullDocument", "ns", "documentKey", "fullDocumentBeforeChange"];
    assert_eq!(keys(&r.to_document(false))[4..], tail);
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

/// An event from a range's change (014 FR-002, D-114): its token is its place in the merge, its
/// times the commit mark, and the transaction and the description come from the version's meta.
#[test]
fn an_event_comes_from_its_change() {
    use teseladb_changestream::changes::Change;
    use teseladb_changestream::meta::{ChangeMeta, TxnInfo, lsid_document};
    use teseladb_changestream::update_desc::UpdateDescription;
    let ts = Hlc { physical: 1_730_198_400_123, logical: 2 };
    let txn = TxnInfo { id: 3, lsid: 4, uid: [5; 32], txn_number: 6 };
    let update = UpdateDescription {
        updated: vec![("a".into(), Value::Int32(1))],
        ..UpdateDescription::default()
    };
    let doc = Document::new().with("_id", 1).with("a", 1);
    let change = Change {
        event: EventKey { ts, txn: 3, op: 2, key: b"k".to_vec() },
        kind: EventKind::Update,
        doc: Some(doc.to_bytes().unwrap()),
        meta: Some(ChangeMeta {
            txn: Some(txn),
            retry: None,
            op: 2,
            update: Some(update.clone()),
            replace: false,
            delete: false,
            doc: None,
            moved: None,
        }),
    };
    let ns = Namespace { db: "shop".into(), coll: Some("orders".into()) };
    let key = Document::new().with("_id", 1);
    let e = ChangeEvent::of(&change, ns.clone(), Some(key.clone()), false).unwrap();
    assert_eq!(e.token, ResumeToken::new(change.event.clone(), EventKind::Update));
    assert_eq!((e.cluster_time, e.wall_ms), (ts, 1_730_198_400_123));
    assert_eq!(e.txn, Some((lsid_document(4, &[5; 32]), 6)));
    assert_eq!(e.update_description, Some(update.to_document(false)));
    assert_eq!((e.document_key, e.full_document, e.ns), (Some(key.clone()), None, ns.clone()));
    // An insert carries its document; a deletion nothing but its key.
    let insert = Change { kind: EventKind::Insert, meta: None, ..change.clone() };
    let e = ChangeEvent::of(&insert, ns.clone(), Some(key.clone()), true).unwrap();
    assert_eq!(e.full_document, Some(Value::Document(doc)));
    assert_eq!((e.txn, e.update_description), (None, None));
    let delete = Change { kind: EventKind::Delete, doc: None, meta: None, ..change.clone() };
    let e = ChangeEvent::of(&delete, ns.clone(), Some(key), true).unwrap();
    assert_eq!(e.full_document, None);
    // A stored document that does not read back stops the stream.
    let broken = Change { kind: EventKind::Insert, doc: Some(vec![1, 2]), ..change };
    assert!(ChangeEvent::of(&broken, ns, None, true).is_err());
}

#[test]
fn catalog_events_have_the_fields_of_refrouter_in_its_order() {
    use teseladb_changestream::event::Namespace as Ns;
    let ts = Hlc { physical: 1_730_198_400_123, logical: 2 };
    let token = |kind| ResumeToken::new(EventKey { ts, txn: 0, op: 1, key: Vec::new() }, kind);
    let ns = Ns { db: "shop".into(), coll: Some("orders".into()) };
    let mut rename = ChangeEvent::catalog(token(EventKind::Rename), ns.clone());
    rename.to = Some(Ns { db: "shop".into(), coll: Some("old".into()) });
    rename.collection_uuid = Some([1; 16]);
    rename.operation_description = Some(Document::new().with("to", 1));
    assert_eq!(
        keys(&rename.to_document(true)),
        [
            "to",
            "_id",
            "operationType",
            "clusterTime",
            "collectionUUID",
            "wallTime",
            "ns",
            "operationDescription"
        ]
    );
    assert_eq!(
        keys(&rename.to_document(false)),
        ["to", "_id", "operationType", "clusterTime", "wallTime", "ns"]
    );
    let mut create = ChangeEvent::catalog(token(EventKind::Create), ns.clone());
    create.operation_description = Some(Document::new());
    create.state_before = Some(Document::new());
    create.ns_type = Some("view");
    assert_eq!(
        keys(&create.to_document(true)),
        [
            "_id",
            "operationType",
            "clusterTime",
            "wallTime",
            "ns",
            "operationDescription",
            "stateBeforeChange",
            "nsType"
        ]
    );
    let db =
        ChangeEvent::catalog(token(EventKind::DropDatabase), Ns { db: "shop".into(), coll: None });
    let d = db.to_document(true);
    assert_eq!(keys(&d), ["_id", "operationType", "clusterTime", "wallTime", "ns"]);
    assert_eq!(d.get("ns"), Some(&Value::Document(Document::new().with("db", "shop"))));
    assert_eq!(d.get("wallTime"), Some(&Value::DateTime(1_730_198_400_123)));
    let inv = ChangeEvent::catalog(token(EventKind::Invalidate), ns);
    assert_eq!(keys(&inv.to_document(true)), ["_id", "operationType", "clusterTime", "wallTime"]);
}

#[test]
fn an_insert_without_its_document_names_none() {
    let d = event(EventKind::Insert).to_document(false);
    assert!(!keys(&d).contains(&"fullDocument"), "{:?}", keys(&d));
}
