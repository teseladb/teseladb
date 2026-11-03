//! What each document's version keeps for its event (014 FR-002, FR-005, FR-009): the description
//! of an update by operators, the transaction (`lsid`, `txnNumber`) and the position of the
//! document in the write, stored next to the document as a version of its own at the same mark.

use teseladb_bson::{Document, Value};
use teseladb_changestream::meta::{
    ChangeMeta, DocWrite, META_FORMAT, META_FORMAT_RETRY, MetaError, RetryInfo, TxnInfo,
    doc_of_meta, history, history_with, lsid_document, meta_key,
};
use teseladb_changestream::update_desc::UpdateDescription;
use teseladb_hlc::Hlc;

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
    ChangeMeta { txn, retry: None, op, update, replace, delete: false, doc: None, moved: None }
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
    // And an update its description.
    let upd = DocWrite { update: Some(desc()), ..write(b"\x01a") };
    assert_eq!(
        history(&[upd], None),
        vec![(meta_key(b"\x01a", 0), m(None, 0, Some(desc()), false))]
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

fn retry(prev: Option<Hlc>, image: u8) -> RetryInfo {
    RetryInfo { lsid: 0x0a0b, uid: [3; 32], txn_number: 11, stmt_id: 4, prev, image }
}

/// A retryable write's meta (018 FR-009) is format 3 and reads back with its session; a meta
/// without one stays format 2, and a format 2 meta with the retry flag is refused.
#[test]
fn a_retryable_write_keeps_its_session() {
    let prev = Some(Hlc { physical: 1_700_000_000_000, logical: 2 });
    for r in [retry(None, 0), retry(prev, 1), retry(prev, 2)] {
        let meta = ChangeMeta { retry: Some(r), ..m(None, 1, Some(desc()), false) };
        let bytes = meta.encode().unwrap();
        assert_eq!(bytes[0], META_FORMAT_RETRY);
        assert_eq!(ChangeMeta::decode(&bytes), Ok(meta));
    }
    assert_eq!(retry(None, 1).image_name(), Some("preImage"));
    assert_eq!(retry(None, 2).image_name(), Some("postImage"));
    assert_eq!(retry(None, 0).image_name(), None);
    let plain = m(None, 1, None, false).encode().unwrap();
    assert_eq!(plain[0], META_FORMAT);
    let mut wrong =
        ChangeMeta { retry: Some(retry(None, 0)), ..m(None, 1, None, false) }.encode().unwrap();
    wrong[0] = META_FORMAT;
    assert!(matches!(ChangeMeta::decode(&wrong), Err(MetaError::Flags(_))));
    let mut unflagged = plain.clone();
    unflagged[0] = META_FORMAT_RETRY;
    assert!(matches!(ChangeMeta::decode(&unflagged), Err(MetaError::Flags(_))));
    let mut image =
        ChangeMeta { retry: Some(retry(None, 0)), ..m(None, 1, None, false) }.encode().unwrap();
    let last = image.len() - 1;
    image[last] = 9;
    assert_eq!(ChangeMeta::decode(&image), Err(MetaError::Flags(9)));
    assert_eq!(ChangeMeta::decode(&image[..20]), Err(MetaError::Truncated));
}

/// Every document of a retryable write has a meta, the first insert too; inside a transaction the
/// transaction's session wins.
#[test]
fn every_write_of_a_retryable_write_has_a_meta() {
    let writes = [DocWrite { key: b"a".to_vec(), ..DocWrite::default() }];
    assert!(history(&writes, None).is_empty());
    let r = retry(None, 0);
    let metas = history_with(&writes, None, Some(&r));
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].1.retry, Some(r));
    let t = txn();
    let in_txn = history_with(&writes, Some((&t, 0)), Some(&r));
    assert_eq!(in_txn[0].1.retry, None);
}
