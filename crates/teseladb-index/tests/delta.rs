//! Index changes between document versions (009 T005, FR-003): inserting writes every entry,
//! deleting removes them, an update touches only the entries that change, a value of another
//! numeric type with the same key rewrites the entry, and a refused new version fails.

use teseladb_bson::{RawDoc, RawValue, extjson};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, KeygenError};
use teseladb_keys::{Direction, PrimaryKey};

fn set() -> IndexSet {
    let def = |fields| IndexDef { fields, buckets: 0, sparse: false, unique: false };
    IndexSet {
        tenant: 0,
        collection: 3,
        indexes: vec![
            (1, def(vec![IndexField::ordered("a", Direction::Ascending)])),
            (2, def(vec![IndexField::ordered("tags", Direction::Descending)])),
            (3, def(vec![IndexField::special("h", FieldKind::Hashed)])),
        ],
    }
}

fn bytes(text: &str) -> Vec<u8> {
    extjson::parse(text).unwrap().to_bytes().unwrap()
}

#[test]
fn insert_update_and_delete() {
    let s = set();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let v1 = bytes("{\"_id\": 1, \"a\": 1, \"tags\": [\"x\", \"y\"], \"h\": 5}");
    let v2 = bytes("{\"_id\": 1, \"a\": 2, \"tags\": [\"y\", \"z\"], \"h\": 5}");
    let old = Version { doc: RawDoc::from_bytes(&v1).unwrap(), pk: &pk };
    let new = Version { doc: RawDoc::from_bytes(&v2).unwrap(), pk: &pk };

    let insert = s.delta(None, Some(old), None).unwrap();
    assert!(insert.remove.is_empty());
    assert_eq!(insert.add.len(), 4, "a, two tags, h");

    let update = s.delta(Some(old), Some(new), None).unwrap();
    assert_eq!(update.remove.len(), 2, "a: 1 and tag x");
    assert_eq!(update.add.len(), 2, "a: 2 and tag z; y and h stay");
    let all_old = s.keys(old, None).unwrap();
    assert!(update.remove.iter().all(|k| all_old.contains_key(k)));

    let delete = s.delta(Some(new), None, None).unwrap();
    assert_eq!((delete.remove.len(), delete.add.len()), (4, 0));
    assert!(s.delta(Some(old), Some(old), None).unwrap().is_empty());
}

#[test]
fn same_key_with_other_type_bits_is_rewritten() {
    let s = set();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let int = bytes("{\"_id\": 1, \"a\": 1}");
    let double = bytes("{\"_id\": 1, \"a\": {\"$numberDouble\": \"1.0\"}}");
    let old = Version { doc: RawDoc::from_bytes(&int).unwrap(), pk: &pk };
    let new = Version { doc: RawDoc::from_bytes(&double).unwrap(), pk: &pk };
    let d = s.delta(Some(old), Some(new), None).unwrap();
    assert!(d.remove.is_empty(), "1 and 1.0 have the same key");
    assert_eq!(d.add.len(), 1, "but other type bits");
}

#[test]
fn a_refused_new_version_fails_the_mutation() {
    let s = set();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let bad = bytes("{\"_id\": 1, \"h\": [1, 2]}");
    let new = Version { doc: RawDoc::from_bytes(&bad).unwrap(), pk: &pk };
    assert_eq!(s.delta(None, Some(new), None), Err(KeygenError::HashedArray("h".to_string())));
}
