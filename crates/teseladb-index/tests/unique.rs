//! Unique indexes (009 FR-004, T010, research R3): the entry's key leaves out the primary key, which
//! goes in its value, so two documents with the same values write the same key; a write that puts
//! a key another document holds, or that an earlier statement of the same write took, is a
//! `DuplicateKey` with the index's `keyPattern` and the conflicting `keyValue` (message measured on
//! a `refrouter` 9.0.4: `E11000 duplicate key error collection: msgs.c index: b_u dup key: { b: 0 }`).

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::keygen::{IndexDef, IndexField};
use teseladb_index::unique::{UniqueError, UniqueGuard, conflicting_values, duplicate_key};
use teseladb_keys::{Direction, PrimaryKey, space};

const ID: u32 = 4;

fn def(paths: &[&str], unique: bool) -> IndexDef {
    let fields = paths.iter().map(|p| IndexField::ordered(p, Direction::Ascending)).collect();
    IndexDef { fields, buckets: 0, sparse: false, unique }
}

fn bytes(text: &str) -> Vec<u8> {
    extjson::parse(text).unwrap().to_bytes().unwrap()
}

fn pk(id: i32) -> PrimaryKey {
    PrimaryKey::id(teseladb_bson::RawValue::Int32(id))
}

/// The (key, value) entries of `doc` in index `def`.
fn entries(def: &IndexDef, doc: &[u8], id: i32) -> Vec<(Vec<u8>, Vec<u8>)> {
    let set = IndexSet { tenant: 0, collection: 9, indexes: vec![(ID, def.clone())] };
    let raw = RawDoc::from_bytes(doc).unwrap();
    let pk = pk(id);
    let d = set.delta(None, Some(Version { doc: raw, pk: &pk }), None).unwrap();
    d.add.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect()
}

#[test]
fn documents_with_the_same_values_share_the_key_and_keep_their_primary_key_in_the_value() {
    let unique = def(&["a"], true);
    let one = entries(&unique, &bytes(r#"{"_id": 1, "a": 5}"#), 1);
    let two = entries(&unique, &bytes(r#"{"_id": 2, "a": 5}"#), 2);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].0, two[0].0, "the key has no primary key");
    let (holder, _) = space::split_unique_value(&one[0].1).unwrap();
    assert_eq!(holder, pk(1).as_bytes());
    // A plain index ends its key with the primary key and keeps only the type bits.
    let plain = entries(&def(&["a"], false), &bytes(r#"{"_id": 1, "a": 5}"#), 1);
    assert!(plain[0].0.ends_with(pk(1).as_bytes()));
    assert_eq!(plain[0].0.len(), one[0].0.len() + pk(1).as_bytes().len());
}

#[test]
fn null_and_a_missing_field_are_the_same_key() {
    // Measured on refnode 9.0.4: `{_id: 2}` after `{_id: 1, a: null}` is a duplicate of `{ a: null }`.
    let unique = def(&["a"], true);
    let null = entries(&unique, &bytes(r#"{"_id": 1, "a": null}"#), 1);
    let missing = entries(&unique, &bytes(r#"{"_id": 2}"#), 2);
    assert_eq!(null[0].0, missing[0].0);
}

#[test]
fn the_guard_refuses_a_key_another_document_holds_or_took_in_the_same_write() {
    let unique = def(&["a"], true);
    let (key, held) = entries(&unique, &bytes(r#"{"_id": 1, "a": 5}"#), 1).remove(0);
    let stored = |k: &[u8]| (k == key.as_slice()).then_some(held.as_slice());
    let nothing = |_: &[u8]| -> Option<&[u8]> { None };
    let put = |id: i32| vec![(key.clone(), pk(id).as_bytes().to_vec())];

    // The same document may write its own key again; another one may not.
    assert_eq!(UniqueGuard::default().admit(&[], &put(1), &stored), Ok(()));
    let dup = UniqueGuard::default().admit(&[], &put(2), &stored);
    assert_eq!(dup, Err(UniqueError::Duplicate { key: key.clone() }));

    // Two statements of one write: the second finds the key the first took.
    let mut g = UniqueGuard::default();
    assert_eq!(g.admit(&[], &put(1), &nothing), Ok(()));
    assert_eq!(g.admit(&[], &put(2), &nothing), Err(UniqueError::Duplicate { key: key.clone() }));

    // A statement that removes the key frees it for a later one.
    let mut g = UniqueGuard::default();
    assert_eq!(g.admit(std::slice::from_ref(&key), &[], &stored), Ok(()));
    assert_eq!(g.admit(&[], &put(2), &stored), Ok(()));
    // So does the statement's own removal of the key it puts.
    let removed = std::slice::from_ref(&key);
    assert_eq!(UniqueGuard::default().admit(removed, &put(2), &stored), Ok(()));

    // A refused statement leaves nothing behind.
    let mut g = UniqueGuard::default();
    let other = entries(&unique, &bytes(r#"{"_id": 3, "a": 6}"#), 3).remove(0).0;
    let both = vec![(other.clone(), pk(3).as_bytes().to_vec()), put(3).remove(0)];
    assert!(g.admit(&[], &both, &stored).is_err());
    assert_eq!(g.admit(&[], &[(other, pk(4).as_bytes().to_vec())], &nothing), Ok(()));

    // A stored value that is not a unique entry's is an error, not a free key.
    let broken = |_: &[u8]| -> Option<&[u8]> { Some(&[0u8, 0, 0, 9][..]) };
    assert!(matches!(
        UniqueGuard::default().admit(&[], &put(1), &broken),
        Err(UniqueError::Malformed(_))
    ));
}

#[test]
fn the_error_names_the_index_its_pattern_and_the_conflicting_values() {
    let unique = def(&["a", "b"], true);
    let doc = bytes(r#"{"_id": 7, "a": [1, 2], "b": "x"}"#);
    let raw = RawDoc::from_bytes(&doc).unwrap();
    let keys = entries(&unique, &doc, 7);
    let set = IndexSet { tenant: 0, collection: 9, indexes: vec![(ID, unique.clone())] };
    let values = conflicting_values(&set, ID, raw, &keys[1].0, None).unwrap();
    assert_eq!(values, vec![Value::Int32(2), Value::from("x")], "the second element's entry");
    assert!(conflicting_values(&set, ID, raw, b"nope", None).unwrap().is_empty());
    assert!(conflicting_values(&set, ID + 1, raw, &keys[1].0, None).unwrap().is_empty());

    let pattern = Document::new().with("a", 1).with("b", -1);
    let e = duplicate_key("msgs.c", "a_b", &pattern, &values).unwrap();
    assert_eq!(
        e.message,
        "E11000 duplicate key error collection: msgs.c index: a_b dup key: { a: 2, b: \"x\" }"
    );
    assert_eq!(e.key_pattern, pattern);
    assert_eq!(e.key_value, Document::new().with("a", 2).with("b", "x"));
}
