//! [`Document::append_fragments`]: a document written as pieces, the documents that keep their
//! bytes taken by reference, gives the bytes [`Document::append_to`] writes.

use teseladb_bson::{BsonError, Document, Fragments, Limits, RawDoc, Value};

fn stored(d: &Document) -> Document {
    Document::from_stored(d.to_bytes().expect("encodes")).expect("stored")
}

fn reply(n: usize, width: usize) -> Document {
    let docs: Vec<Value> = (0..n)
        .map(|i| {
            let d = Document::new()
                .with("_id", i64::try_from(i).expect("small"))
                .with("pad", "x".repeat(width));
            Value::Document(stored(&d))
        })
        .collect();
    let cursor = Document::new().with("firstBatch", docs).with("id", 0i64).with("ns", "db.c");
    Document::new().with("cursor", cursor).with("ok", 1.0)
}

fn joined(f: &Fragments<'_>) -> Vec<u8> {
    f.slices().concat()
}

#[test]
fn a_reply_of_stored_documents_is_the_bytes_append_to_writes() {
    for (n, width) in [(0, 0), (1, 10), (3, 200), (101, 40)] {
        let doc = reply(n, width);
        let mut want = b"head".to_vec();
        doc.append_to(&mut want, Limits::default()).expect("writes");
        let mut f = Fragments::new(64);
        f.extend(b"head");
        doc.append_fragments(&mut f, Limits::default()).expect("writes");
        assert_eq!(f.len(), want.len());
        assert_eq!(joined(&f), want, "{n} documents of {width}");
    }
}

#[test]
fn stored_documents_of_the_threshold_or_more_are_borrowed_and_smaller_ones_copied() {
    let doc = reply(4, 200);
    let mut f = Fragments::new(64);
    doc.append_fragments(&mut f, Limits::default()).expect("writes");
    let Some(Value::Document(cursor)) = doc.get("cursor") else { panic!("cursor") };
    let Some(Value::Array(batch)) = cursor.get("firstBatch") else { panic!("batch") };
    let kept: Vec<&[u8]> = batch
        .iter()
        .map(|v| match v {
            Value::Document(d) => d.canonical_bytes().expect("kept"),
            _ => panic!("a document"),
        })
        .collect();
    let slices = f.slices();
    for k in &kept {
        assert!(
            slices.iter().any(|s| std::ptr::eq(s.as_ptr(), k.as_ptr()) && s.len() == k.len()),
            "each stored document goes out from its own buffer"
        );
    }
    assert_eq!(f.borrowed(), kept.iter().map(|k| k.len()).sum::<usize>());

    let small = reply(4, 1);
    let mut g = Fragments::new(64);
    small.append_fragments(&mut g, Limits::default()).expect("writes");
    assert_eq!(g.borrowed(), 0);
    assert_eq!(g.slices().len(), 1);
}

#[test]
fn an_owned_value_overwritten_in_place_lands_in_the_joined_bytes() {
    let doc = reply(2, 100);
    let mut f = Fragments::new(1);
    f.extend(&[0; 4]);
    doc.append_fragments(&mut f, Limits::default()).expect("writes");
    let total = u32::try_from(f.len()).expect("small").to_le_bytes();
    f.overwrite(0, &total);
    let bytes = joined(&f);
    assert_eq!(bytes[..4], total);
    RawDoc::from_bytes(&bytes[4..]).expect("the document reads back");
}

#[test]
fn the_limits_refuse_what_append_to_refuses() {
    let doc = reply(3, 200);
    let limits = Limits { max_size: 300, max_depth: 100 };
    let mut f = Fragments::new(1);
    let err = doc.append_fragments(&mut f, limits).expect_err("too large");
    assert!(matches!(err, BsonError::TooLarge { .. }), "{err:?}");
    let deep = Limits { max_size: 1 << 20, max_depth: 3 };
    let mut g = Fragments::new(1);
    let err = doc.append_fragments(&mut g, deep).expect_err("too deep");
    assert!(matches!(err, BsonError::TooDeep { .. }), "{err:?}");
}

#[test]
fn a_stored_document_alone_is_one_borrowed_piece() {
    let doc = stored(&Document::new().with("a", "x".repeat(100)));
    let mut f = Fragments::new(8);
    doc.append_fragments(&mut f, Limits::default()).expect("writes");
    assert!(!f.is_empty());
    let slices = f.slices();
    assert_eq!(slices.len(), 1);
    assert!(std::ptr::eq(slices[0].as_ptr(), doc.canonical_bytes().expect("kept").as_ptr()));
    assert_eq!(f.to_vec(), doc.to_bytes().expect("encodes"));
}

#[test]
fn no_bytes_are_one_empty_piece() {
    let f = Fragments::new(0);
    assert!(f.is_empty());
    assert_eq!(f.slices(), vec![&[] as &[u8]]);
}

#[test]
fn into_vec_gives_the_own_buffer_when_nothing_was_borrowed() {
    let owned = Document::new().with("ok", 1.0);
    let mut f = Fragments::new(1);
    owned.append_fragments(&mut f, Limits::default()).expect("writes");
    let at = f.slices()[0].as_ptr();
    let v = f.into_vec();
    assert_eq!(v.as_ptr(), at, "the same buffer");
    assert_eq!(v, owned.to_bytes().expect("encodes"));
    let doc = reply(2, 100);
    let mut g = Fragments::new(1);
    doc.append_fragments(&mut g, Limits::default()).expect("writes");
    let joined = g.to_vec();
    assert_eq!(g.into_vec(), joined);
}
