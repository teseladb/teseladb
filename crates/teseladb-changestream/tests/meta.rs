//! What each document's version keeps for its event (014 FR-002, FR-005, FR-009): the description
//! of an update by operators, the transaction (`lsid`, `txnNumber`) and the position of the
//! document in the write, stored next to the document as a version of its own at the same mark.

use teseladb_bson::{Document, Value};
use teseladb_changestream::meta::{
    ChangeMeta, MetaError, TxnInfo, doc_of_meta, history, lsid_document, meta_key,
};
use teseladb_changestream::update_desc::UpdateDescription;

fn desc() -> UpdateDescription {
    UpdateDescription {
        updated: vec![("a".into(), Value::Int32(2))],
        removed: vec!["m".into()],
        ..UpdateDescription::default()
    }
}

fn txn() -> TxnInfo {
    TxnInfo { id: 0xfeed, lsid: 0x0102, uid: [7; 32], txn_number: 9 }
}

#[test]
fn a_meta_reads_back_what_it_was_made_of() {
    let all = [
        ChangeMeta { txn: None, op: 0, update: Some(desc()), replace: false },
        ChangeMeta { txn: Some(txn()), op: 3, update: None, replace: false },
        ChangeMeta { txn: Some(txn()), op: u32::MAX, update: Some(desc()), replace: false },
        ChangeMeta { txn: None, op: 1, update: None, replace: false },
        ChangeMeta { txn: None, op: 0, update: None, replace: true },
    ];
    for m in all {
        let bytes = m.encode().unwrap();
        assert_eq!(ChangeMeta::decode(&bytes), Ok(m));
    }
}

#[test]
fn a_damaged_meta_is_refused() {
    let bytes = ChangeMeta { txn: Some(txn()), op: 3, update: Some(desc()), replace: false }
        .encode()
        .unwrap();
    assert_eq!(ChangeMeta::decode(&[]), Err(MetaError::Truncated));
    assert_eq!(ChangeMeta::decode(&[2, 0, 0, 0, 0, 0]), Err(MetaError::Format(2)));
    assert_eq!(ChangeMeta::decode(&bytes[..10]), Err(MetaError::Truncated));
    let mut flags = bytes.clone();
    flags[1] = 0x80;
    assert_eq!(ChangeMeta::decode(&flags), Err(MetaError::Flags(0x80)));
    // A description cut short, and bytes after a meta without one.
    assert!(matches!(
        ChangeMeta::decode(&bytes[..bytes.len() - 1]),
        Err(MetaError::Description(_))
    ));
    let mut extra = ChangeMeta { txn: None, op: 1, update: None, replace: false }.encode().unwrap();
    extra.push(0);
    assert_eq!(ChangeMeta::decode(&extra), Err(MetaError::Trailing));
}

#[test]
fn the_meta_goes_right_after_its_document() {
    let doc = b"\x01\0\0\0\0\0\0\0\x07\0\0\0\0\x03k".to_vec();
    let key = meta_key(&doc);
    assert!(key > doc && key.starts_with(&doc) && key.len() == doc.len() + 1);
    assert_eq!(doc_of_meta(&key), Some(&doc[..]));
    assert_eq!(doc_of_meta(b""), None);
}

#[test]
fn a_write_keeps_a_meta_only_for_the_documents_it_says_something_of() {
    let docs = [b"\x01a".to_vec(), b"\x01b".to_vec(), b"\x01c".to_vec()];
    // Alone, the first document of a write needs nothing; the others keep their position, and
    // the updated one its description.
    let updates = [(b"\x01c".to_vec(), desc())];
    let got = history(&docs, (&updates, &[]), None);
    let want = vec![
        (meta_key(b"\x01b"), ChangeMeta { txn: None, op: 1, update: None, replace: false }),
        (meta_key(b"\x01c"), ChangeMeta { txn: None, op: 2, update: Some(desc()), replace: false }),
    ];
    assert_eq!(got, want);
    // In a transaction every document keeps one, from the transaction's next position.
    let got = history(&docs[..1], (&[], &[]), Some((&txn(), 4)));
    assert_eq!(
        got,
        vec![(
            meta_key(b"\x01a"),
            ChangeMeta { txn: Some(txn()), op: 4, update: None, replace: false }
        )]
    );
    assert!(history(&docs[..1], (&[], &[]), None).is_empty());
    // A replacement says so, even as the first document of a write.
    let got = history(&docs[..1], (&[], &docs[..1]), None);
    assert_eq!(
        got,
        vec![(meta_key(b"\x01a"), ChangeMeta { txn: None, op: 0, update: None, replace: true })]
    );
}

/// The `lsid` of an event (measured on a `refrouter` 9.0.4 on 01-11): `{id: UUID, uid: BinData(0)}`.
#[test]
fn the_lsid_of_an_event_has_its_id_and_uid() {
    let d = lsid_document(0x0102, &[7; 32]);
    let mut id = vec![0; 14];
    id.extend_from_slice(&[1, 2]);
    let want = Document::new()
        .with("id", Value::Binary { subtype: 4, bytes: id })
        .with("uid", Value::Binary { subtype: 0, bytes: vec![7; 32] });
    assert_eq!(d, want);
}
