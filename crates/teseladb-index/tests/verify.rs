//! Index-document verification for `validate` (009 T027, FR-016): the case built on refnode 9.0.4
//! with the fail points `skipIndexNewRecords` and `skipUnindexingDocumentWhenDeleted` (a document
//! inserted without its entries, another deleted without removing its entry from `a_1`) gives
//! the same counts: `a_1` holds 1 entry and misses 2, the wildcard index holds none, misses 3 and
//! misses the multikey path `a`.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::keygen::wildcard::{WILDCARD_FIELD, WildcardProjection};
use teseladb_index::keygen::{IndexDef, IndexField, KeygenError, entry_value, index_entries};
use teseladb_index::verify::verify_range;
use teseladb_keys::{Direction, Key, KeyBuilder, PrimaryKey};
use teseladb_bson::{RawDoc, RawValue, extjson};

const A: u32 = 1;
const WILD: u32 = 2;

fn set() -> IndexSet {
    let a = IndexDef {
        fields: vec![IndexField::ordered("a", Direction::Ascending)],
        buckets: 0,
        sparse: false,
        unique: false,
    };
    let wild = IndexDef {
        fields: vec![IndexField::wildcard(
            WILDCARD_FIELD,
            Direction::Ascending,
            WildcardProjection::all(),
        )],
        buckets: 0,
        sparse: false,
        unique: false,
    };
    IndexSet { tenant: 0, collection: 7, indexes: vec![(A, a), (WILD, wild)] }
}

fn bytes(text: &str) -> Vec<u8> {
    extjson::parse(text).unwrap().to_bytes().unwrap()
}

fn entries(set: &IndexSet, id: u32, doc: &[u8], pk: &PrimaryKey) -> Vec<(Key, Vec<u8>)> {
    let def = &set.indexes.iter().find(|(i, _)| *i == id).unwrap().1;
    index_entries(set.tenant, set.collection, id, def, RawDoc::from_bytes(doc).unwrap(), pk, None)
        .unwrap()
        .into_iter()
        .map(|(k, bits)| (k, entry_value(def, pk, &bits)))
        .collect()
}

#[test]
fn missing_and_extra_entries_as_refdb_counts_them() {
    let set = set();
    let one = bytes("{\"_id\": 1, \"a\": [1, 2], \"b\": {\"c\": \"x\"}}");
    let gone = bytes("{\"_id\": 2, \"a\": 5}");
    let pk1 = PrimaryKey::id(RawValue::Int32(1));
    let pk2 = PrimaryKey::id(RawValue::Int32(2));
    let held = entries(&set, A, &gone, &pk2);
    let docs = [Version { doc: RawDoc::from_bytes(&one).unwrap(), pk: &pk1 }];
    let report = verify_range(&set, docs, held.clone(), &BTreeMap::new(), None).unwrap();
    assert_eq!(report.records, 1);
    assert!(!report.valid());
    let a = &report.indexes[&A];
    assert_eq!((a.entries, a.missing.len(), &a.extra), (1, 2, &held));
    assert!(a.missing.iter().all(|m| m.pk == pk1));
    let w = &report.indexes[&WILD];
    assert_eq!((w.entries, w.missing.len(), w.extra.len()), (0, 3, 0));
    assert_eq!(w.missing_multikey_paths, vec!["a".to_string()]);
}

#[test]
fn a_consistent_range_is_valid_and_counts_multikey_paths() {
    let set = set();
    let one = bytes("{\"_id\": 1, \"a\": [1, 2], \"b\": {\"c\": \"x\"}}");
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let mut held = entries(&set, A, &one, &pk);
    held.extend(entries(&set, WILD, &one, &pk));
    held.sort_by(|x, y| x.0.cmp(&y.0));
    // An entry of another collection is not this verification's business.
    held.push((KeyBuilder::document(0, 8, RawValue::Int32(1)), Vec::new()));
    let paths = BTreeMap::from([(WILD, BTreeSet::from(["a".to_string()]))]);
    let docs = [Version { doc: RawDoc::from_bytes(&one).unwrap(), pk: &pk }];
    let report = verify_range(&set, docs, held, &paths, None).unwrap();
    assert!(report.valid(), "{report:?}");
    assert_eq!(report.indexes[&A].entries, 2);
    assert_eq!(report.indexes[&WILD].entries, 4, "3 entries and the multikey path `a`");
}

#[test]
fn other_type_bits_unknown_indexes_and_refused_documents() {
    let set = set();
    let doc = bytes("{\"_id\": 1, \"a\": 1}");
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let mut held = entries(&set, A, &doc, &pk);
    held[0].1 = entries(&set, A, &bytes("{\"_id\": 1, \"a\": 1.0}"), &pk)[0].1.clone();
    let stray = KeyBuilder::index_prefix(0, 7, 9);
    held.push((stray.clone(), Vec::new()));
    let docs = [Version { doc: RawDoc::from_bytes(&doc).unwrap(), pk: &pk }];
    let report = verify_range(&set, docs, held, &BTreeMap::new(), None).unwrap();
    let a = &report.indexes[&A];
    assert_eq!((a.missing.len(), a.extra.len(), a.entries), (1, 1, 1), "1 against 1.0");
    assert_eq!(report.unknown, vec![stray]);
    assert!(!report.valid());

    let parallel = IndexSet {
        tenant: 0,
        collection: 7,
        indexes: vec![(
            3,
            IndexDef {
                fields: vec![
                    IndexField::ordered("a", Direction::Ascending),
                    IndexField::ordered("b", Direction::Ascending),
                ],
                buckets: 0,
                sparse: false,
                unique: false,
            },
        )],
    };
    let bad = bytes("{\"_id\": 1, \"a\": [1], \"b\": [2]}");
    let docs = [Version { doc: RawDoc::from_bytes(&bad).unwrap(), pk: &pk }];
    let err = verify_range(&parallel, docs, Vec::new(), &BTreeMap::new(), None).unwrap_err();
    assert!(matches!(err, KeygenError::ParallelArrays { .. }));
}
