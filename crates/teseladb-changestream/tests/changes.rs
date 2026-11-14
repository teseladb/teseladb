//! The changes a range's versions give (014 FR-002, FR-003, FR-009): each version of a document
//! with the meta written next to it at the same mark, in the merge order, and the type of its
//! event.

use teseladb_changestream::changes::{Change, ChangeError, Versioned, changes, gives_event};
use teseladb_changestream::meta::{ChangeMeta, Moved, TxnInfo, meta_key};

fn m(
    txn: Option<TxnInfo>,
    op: u32,
    update: Option<UpdateDescription>,
    replace: bool,
) -> ChangeMeta {
    ChangeMeta { txn, retry: None, op, update, replace, delete: false, doc: None, moved: None }
}
use teseladb_changestream::order::EventKey;
use teseladb_changestream::token::EventKind;
use teseladb_changestream::update_desc::UpdateDescription;
use teseladb_changestream::{DOCUMENT_VALUE, HISTORY_VALUE};
use teseladb_hlc::Hlc;

/// A document key of collection 7: `01 · 0 · 7 · 0 · …`.
fn doc(id: u8) -> Vec<u8> {
    let mut k = vec![1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0];
    k.extend_from_slice(&[0x20, id]);
    k
}

/// An index entry of the same collection.
fn entry(id: u8) -> Vec<u8> {
    let mut k = vec![1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1];
    k.extend_from_slice(&[0x20, id]);
    k
}

fn ts(p: u64) -> Hlc {
    Hlc { physical: p, logical: 0 }
}

fn put(key: Vec<u8>, at: u64, t: u8, v: &[u8]) -> Versioned {
    Versioned { key, ts: ts(at), value: Some((t, v.to_vec())) }
}

fn meta(m: &ChangeMeta) -> Vec<u8> {
    m.encode().unwrap()
}

#[test]
fn versions_become_changes_in_the_merge_order() {
    let txn = TxnInfo { id: 5, lsid: 9, uid: [0; 32], txn_number: 3 };
    let update = UpdateDescription::default();
    let m_upd = m(None, 1, Some(update.clone()), false);
    let m_rep = m(None, 0, None, true);
    let m_txn = m(Some(txn.clone()), 0, None, false);
    // In key order and, within a key, from the newest, as a time scan gives them.
    let items = vec![
        put(doc(1), 30, DOCUMENT_VALUE, b"d1@30"),
        put(doc(1), 10, DOCUMENT_VALUE, b"d1@10"),
        put(meta_key(&doc(1), 0), 30, HISTORY_VALUE, &meta(&m_rep)),
        put(doc(2), 20, DOCUMENT_VALUE, b"d2@20"),
        put(doc(2), 10, DOCUMENT_VALUE, b"d2@10"),
        put(meta_key(&doc(2), 0), 20, HISTORY_VALUE, &meta(&m_txn)),
        put(meta_key(&doc(2), 1), 10, HISTORY_VALUE, &meta(&m_upd)),
        Versioned { key: doc(3), ts: ts(25), value: None },
        put(entry(1), 30, 2, b""),
        Versioned { key: entry(2), ts: ts(30), value: None },
    ];
    let got = changes(items).unwrap();
    let key = |at: u64, txn: u128, op: u32, k: Vec<u8>| EventKey { ts: ts(at), txn, op, key: k };
    let want = vec![
        Change {
            event: key(10, 0, 0, doc(1)),
            kind: EventKind::Insert,
            doc: Some(b"d1@10".to_vec()),
            meta: None,
        },
        Change {
            event: key(10, 0, 1, doc(2)),
            kind: EventKind::Update,
            doc: Some(b"d2@10".to_vec()),
            meta: Some(m_upd),
        },
        Change {
            event: key(20, 5, 0, doc(2)),
            kind: EventKind::Insert,
            doc: Some(b"d2@20".to_vec()),
            meta: Some(m_txn),
        },
        Change { event: key(25, 0, 0, doc(3)), kind: EventKind::Delete, doc: None, meta: None },
        Change {
            event: key(30, 0, 0, doc(1)),
            kind: EventKind::Replace,
            doc: Some(b"d1@30".to_vec()),
            meta: Some(m_rep),
        },
    ];
    assert_eq!(got, want);
}

/// A document that `shardCollection` keys again (018 FR-001) leaves its old key and takes its new
/// one with an internal meta: both are changes, so the search indexes follow the document, and
/// the meta says that no change stream or oplog shows them.
#[test]
fn a_move_that_keys_a_document_again_is_a_delete_and_an_insert() {
    let gone = ChangeMeta { delete: true, moved: Some(Moved::Internal), ..m(None, 0, None, false) };
    let back = ChangeMeta { moved: Some(Moved::Internal), ..m(None, 0, None, false) };
    let items = vec![
        Versioned { key: doc(1), ts: ts(10), value: None },
        put(meta_key(&doc(1), 0), 10, HISTORY_VALUE, &meta(&gone)),
        put(doc(2), 10, DOCUMENT_VALUE, b"d"),
        put(meta_key(&doc(2), 0), 10, HISTORY_VALUE, &meta(&back)),
    ];
    let got = changes(items).unwrap();
    let kinds: Vec<(Vec<u8>, EventKind, Option<Moved>)> =
        got.into_iter().map(|c| (c.event.key, c.kind, c.meta.and_then(|m| m.moved))).collect();
    assert_eq!(
        kinds,
        [
            (doc(1), EventKind::Delete, Some(Moved::Internal)),
            (doc(2), EventKind::Insert, Some(Moved::Internal)),
        ]
    );
}

/// A document whose shard key an update changed (018 FR-001, D-118) leaves its old key with a
/// deletion that is a change for the indexes and no event; the version at the new key gives the
/// update's event; a move of `shardCollection` gives none on either side.
#[test]
fn only_the_changes_a_client_made_give_events() {
    let away = ChangeMeta { delete: true, moved: Some(Moved::Away), ..m(None, 0, None, false) };
    let here = ChangeMeta { moved: Some(Moved::From(doc(1))), ..m(None, 0, None, true) };
    let internal = ChangeMeta { moved: Some(Moved::Internal), ..m(None, 0, None, false) };
    let items = vec![
        Versioned { key: doc(1), ts: ts(10), value: None },
        put(meta_key(&doc(1), 0), 10, HISTORY_VALUE, &meta(&away)),
        put(doc(2), 10, DOCUMENT_VALUE, b"d"),
        put(meta_key(&doc(2), 0), 10, HISTORY_VALUE, &meta(&here)),
        put(doc(3), 20, DOCUMENT_VALUE, b"e"),
        put(meta_key(&doc(3), 0), 20, HISTORY_VALUE, &meta(&internal)),
        put(doc(4), 30, DOCUMENT_VALUE, b"f"),
    ];
    let got: Vec<(Vec<u8>, EventKind, bool)> = changes(items)
        .unwrap()
        .iter()
        .map(|c| (c.event.key.clone(), c.kind, gives_event(c)))
        .collect();
    assert_eq!(
        got,
        [
            (doc(1), EventKind::Delete, false),
            (doc(2), EventKind::Replace, true),
            (doc(3), EventKind::Insert, false),
            (doc(4), EventKind::Insert, true),
        ]
    );
}

#[test]
fn a_damaged_meta_stops_the_stream() {
    let items = vec![
        put(doc(1), 10, DOCUMENT_VALUE, b"d"),
        put(meta_key(&doc(1), 0), 10, HISTORY_VALUE, &[9]),
    ];
    assert!(matches!(changes(items), Err(ChangeError::Meta(_))));
}

/// A transaction that writes a document several times gives one change per write, in their order,
/// each with the document that write left (measured on a reference router 9.0.4 on 01-11: insert,
/// update and replace of one document, two updates of another and the deletion of the first, six
/// events in the order of the statements).
#[test]
fn a_transaction_gives_one_change_per_write_of_a_document() {
    let txn = TxnInfo { id: 5, lsid: 9, uid: [0; 32], txn_number: 3 };
    let at = |op: u32, d: Option<&[u8]>| ChangeMeta {
        doc: d.map(<[u8]>::to_vec),
        ..m(Some(txn.clone()), op, None, false)
    };
    let desc = |f: &str| UpdateDescription {
        updated: vec![(f.into(), teseladb_bson::Value::Int32(1))],
        ..UpdateDescription::default()
    };
    let metas = [
        (doc(1), at(0, Some(b"v1"))),
        (doc(1), ChangeMeta { update: Some(desc("a")), ..at(1, Some(b"v1a")) }),
        (doc(1), ChangeMeta { replace: true, ..at(2, Some(b"v5")) }),
        (doc(0), ChangeMeta { update: Some(desc("a")), ..at(3, Some(b"z1")) }),
        (doc(0), ChangeMeta { update: Some(desc("b")), ..at(4, Some(b"z2")) }),
        (doc(1), ChangeMeta { delete: true, ..at(5, None) }),
    ];
    let mut items = vec![
        Versioned { key: doc(0), ts: ts(50), value: Some((DOCUMENT_VALUE, b"z2".to_vec())) },
        Versioned { key: doc(1), ts: ts(50), value: None },
    ];
    items.extend(metas.iter().map(|(k, x)| put(meta_key(k, x.op), 50, HISTORY_VALUE, &meta(x))));
    let got = changes(items).unwrap();
    let kinds: Vec<(u32, EventKind, Option<Vec<u8>>)> =
        got.iter().map(|c| (c.event.op, c.kind, c.doc.clone())).collect();
    let want = vec![
        (0, EventKind::Insert, Some(b"v1".to_vec())),
        (1, EventKind::Update, Some(b"v1a".to_vec())),
        (2, EventKind::Replace, Some(b"v5".to_vec())),
        (3, EventKind::Update, Some(b"z1".to_vec())),
        (4, EventKind::Update, Some(b"z2".to_vec())),
        (5, EventKind::Delete, None),
    ];
    assert_eq!(kinds, want);
    assert!(got.iter().all(|c| c.event.txn == 5 && c.event.ts == ts(50)));
}

#[test]
fn a_document_key_names_its_tenant_and_collection() {
    use teseladb_changestream::changes::collection_of;
    let mut k = doc(1);
    k[4] = 3;
    assert_eq!(collection_of(&k), Some((3, 7)));
    assert_eq!(collection_of(&entry(1)), None);
    assert_eq!(collection_of(&[1, 0]), None);
}

#[test]
fn keys_of_another_space_and_history_versions_that_are_no_meta_give_nothing() {
    use teseladb_changestream::changes::{collection_of, is_document_key};
    let mut other = doc(1);
    other[0] = 2;
    assert!(!is_document_key(&other) && collection_of(&other).is_none());
    // A history version whose key does not end as a meta's: no change.
    let mut odd = doc(2);
    odd.extend_from_slice(&[9, 0, 0, 0, 1]);
    let bytes = meta(&m(None, 0, None, false));
    let found = changes(vec![put(odd, 10, HISTORY_VALUE, &bytes)]).unwrap();
    assert!(found.is_empty());
}
