//! Key spaces and versions (003 FR-011 to FR-013, T017): every space is built and taken apart
//! again, a collection with its indexes is one contiguous interval with no other collection in
//! it, and the versions of a logical key go from newest to oldest with the logical key taken out
//! without decoding anything.

use teseladb_bson::{Document, RawDoc, RawValue};
use teseladb_keys::{
    Direction, FieldSpec, Hlc, IndexKeySpec, KeyBuilder, KeyParts, PrimaryKey, VersionedKey,
    bucket, space,
};

fn with<R>(v: impl Into<teseladb_bson::Value>, f: impl FnOnce(RawValue<'_>) -> R) -> R {
    let bytes = Document::new().with("v", v.into()).to_bytes().unwrap();
    f(RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap())
}

fn ascending(n: usize) -> IndexKeySpec {
    IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }; n], buckets: 0 }
}

#[test]
fn every_space_is_built_and_taken_apart() {
    let doc = with(7, |id| KeyBuilder::document(3, 5, id));
    assert_eq!(doc.as_bytes()[0], space::DATA);
    let bucket_7 = with(7, |id| bucket(id, 16));
    match KeyParts::of(doc.as_bytes()).unwrap() {
        KeyParts::Data { tenant, collection, index, rest } => {
            assert_eq!((tenant, collection, index), (3, 5, 0));
            assert_eq!(rest[0], bucket_7, "the document key starts with its bucket");
        }
        other => panic!("{other:?}"),
    }
    // The same _id as 7, 7.0 or 7L: the same key and the same bucket.
    let same = with(7.0, |id| KeyBuilder::document(3, 5, id));
    assert_eq!(same, doc);

    let capped = KeyBuilder::document_capped(3, 5, 42);
    let KeyParts::Data { index: 0, rest, .. } = KeyParts::of(capped.as_bytes()).unwrap() else {
        panic!()
    };
    assert_eq!(rest, 42u64.to_be_bytes());
    let txn = KeyBuilder::txn_record(&[9; 16]);
    assert_eq!(KeyParts::of(txn.as_bytes()).unwrap(), KeyParts::Txn { id: &[9; 16] });
    let sys = KeyBuilder::system(3, 2, b"users");
    assert_eq!(
        KeyParts::of(sys.as_bytes()).unwrap(),
        KeyParts::System { tenant: 3, table: 2, key: b"users" }
    );
    let local = KeyBuilder::range_local(77, 1, b"applied");
    assert_eq!(
        KeyParts::of(local.as_bytes()).unwrap(),
        KeyParts::RangeLocal { range: 77, kind: 1, rest: b"applied" }
    );
    let meta = KeyBuilder::meta(2, b"\x01end");
    assert_eq!(
        KeyParts::of(meta.as_bytes()).unwrap(),
        KeyParts::Meta { level: 2, end: b"\x01end" }
    );
    // The spaces in order: range data, documents, transactions, system, meta-ranges.
    assert!(local < doc && doc < txn && txn < sys && sys < meta);
    for bad in [
        &[][..],
        &[0x05],
        &[space::DATA, 0, 0],
        &[space::SYSTEM, 1],
        &[space::META, 3],
        &[space::RANGE_LOCAL],
    ] {
        assert!(KeyParts::of(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_collection_and_its_indexes_are_one_interval() {
    let spec = ascending(1);
    let mut keys = Vec::new();
    for collection in [4, 5, 6] {
        for n in 0..50 {
            let pk = with(n, PrimaryKey::id);
            keys.push((collection, with(n, |id| KeyBuilder::document(1, collection, id))));
            keys.push((
                collection,
                with(n % 7, |v| KeyBuilder::index(1, collection, 1, &spec, &[v], &pk, None).0),
            ));
            keys.push((
                collection,
                with(format!("s{n}"), |v| {
                    KeyBuilder::index(1, collection, 2, &spec, &[v], &pk, None).0
                }),
            ));
        }
    }
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    // Sorted, the collections come one after another without mixing.
    let order: Vec<u32> = keys.iter().map(|(c, _)| *c).collect();
    assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
    let (start, end) = (KeyBuilder::collection_start(1, 5), KeyBuilder::collection_end(1, 5));
    for (c, k) in &keys {
        assert_eq!(*c == 5, *k >= start && *k < end, "{k:?}");
        // Documents and index entries share the collection's prefix (004 FR-022).
        let prefix = space::collection_of(k.as_bytes()).expect("a data key");
        assert_eq!(prefix, &KeyBuilder::collection_start(1, *c).as_bytes()[..9], "{k:?}");
    }
    assert_eq!(space::collection_of(&[space::DATA, 0, 0]), None, "too short");
    assert_eq!(space::collection_of(&[space::SYSTEM, 0, 0, 0, 1, 0, 0, 0, 5, 0]), None);
}

#[test]
fn an_index_entry_ends_with_its_primary_key() {
    let spec = IndexKeySpec {
        fields: vec![
            FieldSpec { direction: Direction::Ascending },
            FieldSpec { direction: Direction::Descending },
        ],
        buckets: 0,
    };
    let pk = with("abc", PrimaryKey::id);
    let (key, bits) =
        with(1, |a| with("x", |b| KeyBuilder::index(2, 3, 4, &spec, &[a, b], &pk, None)));
    assert!(key.as_bytes().ends_with(pk.as_bytes()));
    let KeyParts::Data { index, rest, .. } = KeyParts::of(key.as_bytes()).unwrap() else {
        panic!()
    };
    assert_eq!(index, 4);
    let values = teseladb_keys::decode(
        &rest[..rest.len() - pk.as_bytes().len()],
        &bits,
        &[Direction::Ascending, Direction::Descending],
    )
    .unwrap();
    assert_eq!(values, vec![1.into(), "x".into()]);
    // The primary key of every kind of collection.
    let sharded = with(10, |s| with(1, |id| PrimaryKey::sharded(&[s], id)));
    assert!(sharded.as_bytes().len() > with(1, PrimaryKey::id).as_bytes().len());
    assert_eq!(PrimaryKey::seq(5).as_bytes(), 5u64.to_be_bytes());
    // An index with buckets puts one after the index number.
    let bucketed = IndexKeySpec { buckets: 4, ..ascending(1) };
    let (k, _) = with(9, |v| KeyBuilder::index(2, 3, 4, &bucketed, &[v], &pk, None));
    let KeyParts::Data { rest, .. } = KeyParts::of(k.as_bytes()).unwrap() else { panic!() };
    assert!(rest[0] < 4);
    // Sharded and clustered documents have no bucket: their order is the shard key's or the _id's.
    let a = with(1, |s| with(9, |id| KeyBuilder::document_sharded(2, 3, &[s], id)));
    let b = with(2, |s| with(0, |id| KeyBuilder::document_sharded(2, 3, &[s], id)));
    assert!(a < b);
    let (c1, c2) = (
        with(1, |id| KeyBuilder::document_clustered(2, 3, id)),
        with(2, |id| KeyBuilder::document_clustered(2, 3, id)),
    );
    assert!(c1 < c2);
}

#[test]
fn versions_go_from_newest_to_oldest() {
    let key = with(1, |id| KeyBuilder::document(1, 1, id));
    let stamps = [
        Hlc { physical: 5, logical: 0 },
        Hlc { physical: 5, logical: 2 },
        Hlc { physical: 9, logical: 0 },
    ];
    let mut versions: Vec<VersionedKey> = stamps.iter().map(|t| key.with_version(*t)).collect();
    versions.sort();
    let order: Vec<Hlc> =
        versions.iter().map(|v| VersionedKey::split(v.as_bytes()).unwrap().1).collect();
    assert_eq!(order, [stamps[2], stamps[1], stamps[0]]);
    for v in &versions {
        let (logical, _) = VersionedKey::split(v.as_bytes()).unwrap();
        assert_eq!(logical, key.as_bytes());
        assert_eq!(v.as_bytes().len(), key.as_bytes().len() + 12);
    }
    assert!(VersionedKey::split(&[1, 2, 3]).is_err());
}

#[test]
fn a_unique_entry_leaves_the_primary_key_to_its_value() {
    // 009 FR-004: two documents with the same values share the key of a unique index.
    let spec = ascending(1);
    let pk = |id: i32| with(id, PrimaryKey::id);
    let key = |id: i32| {
        with(5, |v| {
            let (k, bits) = KeyBuilder::unique_index(1, 2, 3, &spec, &[v], None);
            let (plain, _) = KeyBuilder::index(1, 2, 3, &spec, &[v], &pk(id), None);
            assert!(plain.as_bytes().starts_with(k.as_bytes()), "the same prefix and values");
            assert_eq!(plain.as_bytes().len(), k.as_bytes().len() + pk(id).as_bytes().len());
            (k, bits)
        })
    };
    let (a, bits) = key(1);
    assert_eq!(a, key(2).0, "the key has no primary key");
    let value = space::unique_value(&pk(1), &bits);
    let (got_pk, got_bits) = space::split_unique_value(&value).unwrap();
    assert_eq!((got_pk, got_bits), (pk(1).as_bytes(), bits));
    for bad in [&value[..3], &value[..5], &[0, 0, 0, 9, 1][..]] {
        assert!(space::split_unique_value(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_split_key_never_cuts_a_collection_kept_whole() {
    let capped = |t: u32, c: u32| (t, c) == (0, 7);
    let span = |c: u32| [&[1u8][..], &0u32.to_be_bytes(), &c.to_be_bytes()].concat();
    let inside_capped = KeyBuilder::document_capped(0, 7, 40).as_bytes().to_vec();
    let whole = (&[][..], &[][..]);
    // Inside the capped collection: its start.
    assert_eq!(space::cut_outside(&inside_capped, whole, capped), Some(span(7)));
    // At its start already: kept.
    assert_eq!(space::cut_outside(&span(7), whole, capped), Some(span(7)));
    // A range that starts at the collection: its end.
    let from_it = span(7);
    assert_eq!(space::cut_outside(&inside_capped, (&from_it, &[][..]), capped), Some(span(8)));
    // A range that holds only the collection: no cut.
    let (s, e) = (span(7), span(8));
    assert_eq!(space::cut_outside(&inside_capped, (&s, &e), capped), None);
    // Another collection, a non-data key, and a key outside the range.
    let other = KeyBuilder::document_capped(0, 9, 1).as_bytes().to_vec();
    assert_eq!(space::cut_outside(&other, whole, capped), Some(other.clone()));
    assert_eq!(space::cut_outside(&[2, 5], whole, capped), Some(vec![2, 5]));
    assert_eq!(space::cut_outside(&other, (&other, &[][..]), capped), None);
    // The last collection of the last tenant ends at the next key space.
    let last = |t: u32, c: u32| (t, c) == (u32::MAX, u32::MAX);
    let k = KeyBuilder::document_capped(u32::MAX, u32::MAX, 3).as_bytes().to_vec();
    let from = [&[1u8][..], &[0xff; 8]].concat();
    assert_eq!(space::cut_outside(&k, (&from, &[][..]), last), Some(vec![2]));
}

/// The document of an index entry's primary key is the document of its `_id` (010 FR-001): the
/// planner fetches it from the entry alone.
#[test]
fn the_document_of_a_primary_key_is_that_of_its_id() {
    for id in [RawValue::Int32(7), RawValue::String("x"), RawValue::Double(2.5)] {
        let pk = PrimaryKey::id(id);
        assert_eq!(KeyBuilder::document_of_pk(3, 9, pk.as_bytes()), KeyBuilder::document(3, 9, id));
    }
}

/// A document's change metas (014) are its key followed by `00` and a position, right after it: a
/// cut there would leave a meta in another range than its document, so it moves to the document.
#[test]
fn a_split_key_never_parts_a_document_from_its_meta() {
    let none = |_: u32, _: u32| false;
    let doc = KeyBuilder::document(0, 3, RawValue::Int32(5)).as_bytes().to_vec();
    let meta = [&doc[..], &[0, 0, 0, 0, 3][..]].concat();
    let whole = (&[][..], &[][..]);
    assert_eq!(space::cut_outside(&meta, whole, none), Some(doc.clone()));
    // At the start of the range the document is already there: no cut.
    assert_eq!(space::cut_outside(&meta, (&doc, &[][..]), none), None);
    // An index entry ending in `00` is no meta.
    let entry = [KeyBuilder::index_prefix(0, 3, 1).as_bytes(), &[4, 0, 0, 0, 0, 1]].concat();
    assert_eq!(space::cut_outside(&entry, whole, none), Some(entry.clone()));
}
