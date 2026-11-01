//! What each document's version keeps for its event (014 FR-002, FR-005, FR-009): the description
//! of an update by operators, the transaction (`lsid`, `txnNumber`) and the position of the
//! document in the write, stored next to the document as a version of its own at the same mark.

use teseladb_bson::{Document, Value};
use teseladb_changestream::meta::{
    ChangeMeta, DocWrite, MetaError, TxnInfo, doc_of_meta, history, lsid_document, meta_key,
};
use teseladb_changestream::update_desc::UpdateDescription;

fn desc() -> UpdateDescription {
    UpdateDescription {
        updated: vec![("a".into(), Value::Int32(2))],
        removed: vec!["m".into()],
        ..UpdateDescription::default()
    }
}

fn m(
    txn: Option<TxnInfo>,
    op: u32,
    update: Option<UpdateDescription>,
    replace: bool,
) -> ChangeMeta {
    ChangeMeta { txn, op, update, replace, delete: false, doc: None }
}

fn txn() -> TxnInfo {
    TxnInfo { id: 0xfeed, lsid: 0x0102, uid: [7; 32], txn_number: 9 }
}

#[test]
fn a_meta_reads_back_what_it_was_made_of() {
    let all = [
        m(None, 0, Some(desc()), false),
        m(Some(txn()), 3, None, false),
        m(Some(txn()), u32::MAX, Some(desc()), false),
        m(None, 1, None, false),
        m(None, 0, None, true),
        ChangeMeta { delete: true, doc: None, ..m(Some(txn()), 2, None, false) },
        ChangeMeta { doc: Some(b"doc".to_vec()), ..m(Some(txn()), 2, Some(desc()), false) },
    ];
    for m in all {
        let bytes = m.encode().unwrap();
        assert_eq!(ChangeMeta::decode(&bytes), Ok(m));
    }
}

#[test]
fn a_damaged_meta_is_refused() {
    let bytes = m(Some(txn()), 3, Some(desc()), false).encode().unwrap();
    assert_eq!(ChangeMeta::decode(&[]), Err(MetaError::Truncated));
    assert_eq!(ChangeMeta::decode(&[1, 0, 0, 0, 0, 0]), Err(MetaError::Format(1)));
    assert_eq!(ChangeMeta::decode(&bytes[..10]), Err(MetaError::Truncated));
    let mut flags = bytes.clone();
    flags[1] = 0x80;
    assert_eq!(ChangeMeta::decode(&flags), Err(MetaError::Flags(0x80)));
    // A description cut short or that is no BSON, and bytes after a meta without a document.
    assert_eq!(ChangeMeta::decode(&bytes[..bytes.len() - 1]), Err(MetaError::Truncated));
    let mut garbled = bytes.clone();
    let desc_at = 2 + 4 + 72 + 4;
    garbled[desc_at] = 0xff;
    assert!(matches!(ChangeMeta::decode(&garbled), Err(MetaError::Description(_))));
    let mut extra = m(None, 1, None, false).encode().unwrap();
    extra.push(0);
    assert_eq!(ChangeMeta::decode(&extra), Err(MetaError::Trailing));
}

#[test]
fn the_metas_go_right_after_their_document_in_the_order_of_their_writes() {
    let doc = b"\x01\0\0\0\0\0\0\0\x07\0\0\0\0\x03k".to_vec();
    let (first, second) = (meta_key(&doc, 0), meta_key(&doc, 1));
    assert!(doc < first && first < second && second.starts_with(&doc));
    assert_eq!(second.len(), doc.len() + 5);
    assert_eq!(doc_of_meta(&second), Some(&doc[..]));
    assert_eq!(doc_of_meta(b"abc"), None);
    assert_eq!(doc_of_meta(&[&doc[..], &[1, 0, 0, 0, 0]].concat()), None, "no meta suffix");
}

fn write(key: &[u8]) -> DocWrite {
    DocWrite { key: key.to_vec(), ..DocWrite::default() }
}

#[test]
fn a_write_keeps_a_meta_only_for_the_documents_it_says_something_of() {
    let c = DocWrite { update: Some(desc()), ..write(b"\x01c") };
    let writes = [write(b"\x01a"), write(b"\x01b"), c];
    // Alone, the first document of a write needs nothing; the others keep their position, and
    // the updated one its description.
    let want = vec![
        (meta_key(b"\x01b", 1), m(None, 1, None, false)),
        (meta_key(b"\x01c", 2), m(None, 2, Some(desc()), false)),
    ];
    assert_eq!(history(&writes, None), want);
    assert!(history(&writes[..1], None).is_empty());
    // In a transaction every write keeps one, from the transaction's next position, with its type
    // and the document it left.
    let ins = DocWrite { doc: Some(b"d".to_vec()), ..write(b"\x01a") };
    let del = DocWrite { delete: true, ..write(b"\x01a") };
    let got = history(&[ins, del], Some((&txn(), 4)));
    let first = ChangeMeta { doc: Some(b"d".to_vec()), ..m(Some(txn()), 4, None, false) };
    let second = ChangeMeta { delete: true, ..m(Some(txn()), 5, None, false) };
    assert_eq!(got, vec![(meta_key(b"\x01a", 4), first), (meta_key(b"\x01a", 5), second)]);
    // A replacement says so, even as the first document of a write.
    let rep = DocWrite { replace: true, ..write(b"\x01a") };
    assert_eq!(history(&[rep], None), vec![(meta_key(b"\x01a", 0), m(None, 0, None, true))]);
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
