//! Index changes between document versions (009 T005, FR-003): inserting writes every entry,
//! deleting removes them, an update touches only the entries that change, a value of another
//! numeric type with the same key rewrites the entry, and a refused new version fails.

use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, KeygenError};
use teseladb_keys::{Direction, PrimaryKey};
use teseladb_bson::{RawDoc, RawValue, extjson};

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
    assert!(!delete.is_empty() && !insert.is_empty());
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

/// The delta as it is defined, with every index computed for both versions: what the delta that
/// leaves alike indexes out has to equal (FR-022).
fn whole(s: &IndexSet, old: Version<'_>, new: Version<'_>) -> (usize, usize) {
    let (before, after) = (s.keys(old, None).unwrap(), s.keys(new, None).unwrap());
    let remove = before.keys().filter(|k| !after.contains_key(*k)).count();
    let add = after.iter().filter(|(k, v)| before.get(*k) != Some(*v)).count();
    (remove, add)
}

#[test]
fn an_index_whose_fields_stay_is_left_out_and_the_delta_is_the_same() {
    // 009 FR-022. Fields `a` (also under a dotted path), `tags` and `h` in every combination
    // of staying and changing, a field no index has, and a document that grows.
    let nested = |fields| IndexDef { fields, buckets: 0, sparse: false, unique: false };
    let mut s = set();
    s.indexes.push((4, nested(vec![IndexField::ordered("o.x", Direction::Ascending)])));
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let docs: Vec<Vec<u8>> = [
        "{\"_id\": 1, \"a\": 1, \"tags\": [\"x\"], \"h\": 5, \"o\": {\"x\": 1, \"y\": 1}, \"n\": 0}",
        "{\"_id\": 1, \"a\": 1, \"tags\": [\"x\"], \"h\": 5, \"o\": {\"x\": 1, \"y\": 1}, \"n\": 9}",
        "{\"_id\": 1, \"a\": 2, \"tags\": [\"x\"], \"h\": 5, \"o\": {\"x\": 1, \"y\": 1}, \"n\": 0}",
        "{\"_id\": 1, \"a\": 1, \"tags\": [\"x\", \"y\"], \"h\": 5, \"o\": {\"x\": 1, \"y\": 2}, \"n\": 0}",
        "{\"_id\": 1, \"a\": 1, \"tags\": [\"x\"], \"h\": 6, \"o\": {\"x\": 2, \"y\": 1}, \"n\": 0}",
        "{\"_id\": 1, \"tags\": [], \"h\": 5, \"pad\": \"xxxxxxxx\", \"a\": 1}",
        "{\"_id\": 1}",
    ]
    .iter()
    .map(|t| bytes(t))
    .collect();
    for old in &docs {
        for new in &docs {
            let (o, n) = (RawDoc::from_bytes(old).unwrap(), RawDoc::from_bytes(new).unwrap());
            let (old, new) = (Version { doc: o, pk: &pk }, Version { doc: n, pk: &pk });
            let d = s.delta(Some(old), Some(new), None).unwrap();
            assert_eq!((d.remove.len(), d.add.len()), whole(&s, old, new), "{o:?} to {n:?}");
        }
    }
    // The same version twice changes nothing, whatever its fields.
    let doc = RawDoc::from_bytes(&docs[3]).unwrap();
    let same = Version { doc, pk: &pk };
    assert!(s.delta(Some(same), Some(same), None).unwrap().is_empty());
}

#[test]
fn the_bytes_of_a_field_decide_whether_its_index_is_left_out() {
    // 009 FR-022: the same number as another type, or zero with the other sign, has other type
    // bits and its entry is written again; a NaN is the same value as itself.
    let s = set();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let pairs = [
        ("{\"_id\": 1, \"a\": 1}", "{\"_id\": 1, \"a\": {\"$numberLong\": \"1\"}}"),
        (
            "{\"_id\": 1, \"a\": {\"$numberDouble\": \"0.0\"}}",
            "{\"_id\": 1, \"a\": {\"$numberDouble\": \"-0.0\"}}",
        ),
        (
            "{\"_id\": 1, \"a\": {\"$numberDouble\": \"NaN\"}}",
            "{\"_id\": 1, \"a\": {\"$numberDouble\": \"NaN\"}}",
        ),
    ];
    for (old, new) in pairs {
        let (old, new) = (bytes(old), bytes(new));
        let (o, n) = (RawDoc::from_bytes(&old).unwrap(), RawDoc::from_bytes(&new).unwrap());
        let (old, new) = (Version { doc: o, pk: &pk }, Version { doc: n, pk: &pk });
        let d = s.delta(Some(old), Some(new), None).unwrap();
        assert_eq!((d.remove.len(), d.add.len()), whole(&s, old, new), "{o:?} to {n:?}");
    }
    // Another primary key moves every entry, whatever the fields: `a`, `h` and the entry of
    // the missing `tags`.
    let doc = bytes("{\"_id\": 1, \"a\": 1, \"h\": 5}");
    let doc = RawDoc::from_bytes(&doc).unwrap();
    let other = PrimaryKey::id(RawValue::Int32(2));
    let (old, new) = (Version { doc, pk: &pk }, Version { doc, pk: &other });
    let moved = s.delta(Some(old), Some(new), None).unwrap();
    assert_eq!((moved.remove.len(), moved.add.len()), (3, 3));
    assert_eq!((moved.remove.len(), moved.add.len()), whole(&s, old, new));
}

#[test]
fn an_index_left_out_does_not_look_at_its_fields_and_a_wildcard_one_is_always_computed() {
    // 009 FR-022: a version an index would refuse, with the fields of that index as they were,
    // changes nothing in it: the version in force was accepted with the same fields. The
    // hashed index of an array is the case at hand.
    let s = set();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let (old, new) = (
        bytes("{\"_id\": 1, \"h\": [1, 2], \"a\": 1}"),
        bytes("{\"_id\": 1, \"h\": [1, 2], \"a\": 2}"),
    );
    let (o, n) = (RawDoc::from_bytes(&old).unwrap(), RawDoc::from_bytes(&new).unwrap());
    let d = s.delta(Some(Version { doc: o, pk: &pk }), Some(Version { doc: n, pk: &pk }), None);
    let d = d.unwrap();
    assert_eq!((d.remove.len(), d.add.len()), (1, 1), "a: 1 becomes a: 2");
    // Its fields changed: the index is computed and refuses the new version.
    let worse = bytes("{\"_id\": 1, \"h\": [1, 2, 3], \"a\": 1}");
    let w = RawDoc::from_bytes(&worse).unwrap();
    let refused =
        s.delta(Some(Version { doc: o, pk: &pk }), Some(Version { doc: w, pk: &pk }), None);
    assert_eq!(refused, Err(KeygenError::HashedArray("h".to_string())));
    // A wildcard field reaches every field of the document: a field its path does not name
    // changes its entries.
    let projection = teseladb_index::keygen::wildcard::WildcardProjection::all();
    let wild = IndexField::wildcard("$**", Direction::Ascending, projection);
    let def = IndexDef { fields: vec![wild], buckets: 0, sparse: false, unique: false };
    let s = IndexSet { tenant: 0, collection: 3, indexes: vec![(9, def)] };
    let (old, new) = (bytes("{\"_id\": 1, \"q\": 1}"), bytes("{\"_id\": 1, \"q\": 2}"));
    let (o, n) = (RawDoc::from_bytes(&old).unwrap(), RawDoc::from_bytes(&new).unwrap());
    let (old, new) = (Version { doc: o, pk: &pk }, Version { doc: n, pk: &pk });
    let d = s.delta(Some(old), Some(new), None).unwrap();
    assert_eq!((d.remove.len(), d.add.len()), whole(&s, old, new));
    assert!(!d.is_empty());
}
