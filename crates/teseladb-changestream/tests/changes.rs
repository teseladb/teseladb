//! The changes a range's versions give (014 FR-002, FR-003, FR-009): each version of a document
//! with the meta written next to it at the same mark, in the merge order, and the type of its
//! event.

use teseladb_changestream::changes::{Change, ChangeError, Versioned, changes};
use teseladb_changestream::meta::{ChangeMeta, TxnInfo, meta_key};
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
    let m_upd = ChangeMeta { txn: None, op: 1, update: Some(update.clone()), replace: false };
    let m_rep = ChangeMeta { txn: None, op: 0, update: None, replace: true };
    let m_txn = ChangeMeta { txn: Some(txn.clone()), op: 0, update: None, replace: false };
    // In key order and, within a key, from the newest, as a time scan gives them.
    let items = vec![
        put(doc(1), 30, DOCUMENT_VALUE, b"d1@30"),
        put(doc(1), 10, DOCUMENT_VALUE, b"d1@10"),
        put(meta_key(&doc(1)), 30, HISTORY_VALUE, &meta(&m_rep)),
        put(doc(2), 20, DOCUMENT_VALUE, b"d2@20"),
        put(doc(2), 10, DOCUMENT_VALUE, b"d2@10"),
        put(meta_key(&doc(2)), 20, HISTORY_VALUE, &meta(&m_txn)),
        put(meta_key(&doc(2)), 10, HISTORY_VALUE, &meta(&m_upd)),
        Versioned { key: doc(3), ts: ts(25), value: None },
        put(entry(1), 30, 2, b""),
        Versioned { key: entry(2), ts: ts(30), value: None },
        // A meta left by a write whose document version went (a purge): no change.
        put(meta_key(&doc(4)), 15, HISTORY_VALUE, &meta(&m_upd)),
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

#[test]
fn a_damaged_meta_stops_the_stream() {
    let items = vec![
        put(doc(1), 10, DOCUMENT_VALUE, b"d"),
        put(meta_key(&doc(1)), 10, HISTORY_VALUE, &[9]),
    ];
    assert!(matches!(changes(items), Err(ChangeError::Meta(_))));
}
