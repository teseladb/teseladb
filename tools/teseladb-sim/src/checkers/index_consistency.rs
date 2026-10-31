//! Index-document consistency (009 FR-017, `index-maintenance.md`): for every index ready since
//! a time R and every read time t ≥ R, the entries of the index visible at t are exactly the keys
//! of the documents visible at t (after sparse and partial filtering). An index still building,
//! or ready after t, is not checked at t. The comparison is `teseladb_index::verify`, the same one
//! `validate` uses.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_bson::RawDoc;
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::verify::verify_range;
use teseladb_keys::{Hlc, Key, PrimaryKey};

use crate::checkers::Violation;

/// Name of the checker.
pub const CHECKER: &str = "index_consistency";

/// What a range holds at one read time.
#[derive(Clone, Debug)]
pub struct RangeState {
    /// The collection's indexes, each with the time it became ready (`None`: still building).
    pub indexes: IndexSet,
    /// Ready times by index number.
    pub ready: BTreeMap<u32, Option<Hlc>>,
    /// The read time.
    pub at: Hlc,
    /// Documents visible at `at`: their bytes and primary keys.
    pub docs: Vec<(Vec<u8>, PrimaryKey)>,
    /// Index entries of the collection visible at `at`, with their values.
    pub entries: Vec<(Key, Vec<u8>)>,
    /// Multikey paths the catalog records per wildcard index.
    pub multikey_paths: BTreeMap<u32, BTreeSet<String>>,
}

/// Checks one state.
pub fn check(state: &RangeState) -> Result<(), Violation> {
    let fail = |what: String| Violation::safety(CHECKER, format!("at {:?}: {what}", state.at));
    let ready = IndexSet {
        tenant: state.indexes.tenant,
        collection: state.indexes.collection,
        indexes: state
            .indexes
            .indexes
            .iter()
            .filter(|(id, _)| state.ready.get(id).copied().flatten().is_some_and(|r| r <= state.at))
            .cloned()
            .collect(),
    };
    let mut docs = Vec::with_capacity(state.docs.len());
    for (bytes, pk) in &state.docs {
        let doc = RawDoc::from_bytes(bytes).map_err(|e| fail(format!("stored document: {e:?}")))?;
        docs.push(Version { doc, pk });
    }
    let checked: BTreeSet<u32> = ready.indexes.iter().map(|(id, _)| *id).collect();
    let held = state.entries.iter().filter(|(k, _)| {
        // Entries of indexes not checked at this time are left out.
        teseladb_keys::KeyParts::of(k.as_bytes()).is_ok_and(|p| match p {
            teseladb_keys::KeyParts::Data { index, .. } => checked.contains(&index),
            _ => false,
        })
    });
    let report = verify_range(&ready, docs, held.cloned(), &state.multikey_paths, None)
        .map_err(|e| fail(format!("a stored document has no keys: {e:?}")))?;
    for (id, index) in &report.indexes {
        if let Some(m) = index.missing.first() {
            return Err(fail(format!(
                "index {id} lacks {} entries, first {:?} of document {:?}",
                index.missing.len(),
                m.key,
                m.pk
            )));
        }
        if let Some((k, _)) = index.extra.first() {
            return Err(fail(format!(
                "index {id} holds {} entries no document needs, first {k:?}",
                index.extra.len()
            )));
        }
        if let Some(p) = index.missing_multikey_paths.first() {
            return Err(fail(format!("wildcard index {id} lacks the multikey path {p}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use teseladb_bson::{RawValue, extjson};
    use teseladb_index::keygen::{IndexDef, IndexField, entry_value, index_entries};
    use teseladb_keys::Direction;

    use super::*;

    const A: u32 = 1;
    const B: u32 = 2;

    fn hlc(physical: u64) -> Hlc {
        Hlc { physical, logical: 0 }
    }

    fn def(path: &str) -> IndexDef {
        IndexDef {
            fields: vec![IndexField::ordered(path, Direction::Ascending)],
            buckets: 0,
            sparse: false,
            unique: false,
        }
    }

    fn state(doc: &str, held_from: &str) -> RangeState {
        let indexes =
            IndexSet { tenant: 0, collection: 3, indexes: vec![(A, def("a")), (B, def("b"))] };
        let pk = PrimaryKey::id(RawValue::Int32(1));
        let bytes = extjson::parse(doc).unwrap().to_bytes().unwrap();
        let held_bytes = extjson::parse(held_from).unwrap().to_bytes().unwrap();
        let held = RawDoc::from_bytes(&held_bytes).unwrap();
        let mut entries = Vec::new();
        for (id, d) in &indexes.indexes {
            let found = index_entries(0, 3, *id, d, held, &pk, None).unwrap();
            entries.extend(found.into_iter().map(|(k, bits)| (k, entry_value(d, &pk, &bits))));
        }
        RangeState {
            indexes,
            ready: BTreeMap::from([(A, Some(hlc(10))), (B, None)]),
            at: hlc(20),
            docs: vec![(bytes, pk)],
            entries,
            multikey_paths: BTreeMap::new(),
        }
    }

    #[test]
    fn entries_matching_the_documents_pass() {
        assert_eq!(check(&state("{\"a\": 1, \"b\": 2}", "{\"a\": 1, \"b\": 2}")), Ok(()));
    }

    #[test]
    fn a_stale_entry_is_both_missing_and_extra() {
        let v = check(&state("{\"a\": 1}", "{\"a\": 2}")).unwrap_err();
        assert_eq!(v.checker, CHECKER);
        assert!(v.detail.contains("lacks 1 entries"), "{}", v.detail);
        let mut s = state("{\"a\": 1}", "{\"a\": 1}");
        s.docs.clear();
        let v = check(&s).unwrap_err();
        assert!(v.detail.contains("holds 1 entries"), "{}", v.detail);
    }

    #[test]
    fn indexes_not_ready_at_the_read_time_are_not_checked() {
        // `b` is still building: its entries may lag.
        assert_eq!(check(&state("{\"a\": 1, \"b\": 2}", "{\"a\": 1, \"b\": 9}")), Ok(()));
        let mut s = state("{\"a\": 1}", "{\"a\": 2}");
        s.ready.insert(A, Some(hlc(30)));
        assert_eq!(check(&s), Ok(()), "ready after the read time");
    }

    #[test]
    fn unreadable_documents_refused_documents_and_multikey_paths_are_violations() {
        let mut s = state("{\"a\": 1}", "{\"a\": 1}");
        s.docs[0].0 = vec![1, 2];
        assert!(check(&s).unwrap_err().detail.contains("stored document"));
        let mut s = state("{\"a\": 1}", "{\"a\": 1}");
        s.indexes.indexes[0].1.fields.push(IndexField::ordered("c", Direction::Ascending));
        s.docs[0].0 = extjson::parse("{\"a\": [1], \"c\": [2]}").unwrap().to_bytes().unwrap();
        assert!(check(&s).unwrap_err().detail.contains("no keys"));
        let mut s = state("{\"a\": 1}", "{\"a\": 1}");
        s.entries.push((teseladb_keys::KeyBuilder::document(0, 3, RawValue::Int32(1)), Vec::new()));
        assert_eq!(check(&s), Ok(()), "the documents' own keys are not index entries");
    }

    #[test]
    fn a_wildcard_index_without_its_multikey_path_is_a_violation() {
        use teseladb_index::keygen::wildcard::{WILDCARD_FIELD, WildcardProjection};
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
        let pk = PrimaryKey::id(RawValue::Int32(1));
        let bytes = extjson::parse("{\"x\": [1]}").unwrap().to_bytes().unwrap();
        let entries =
            index_entries(0, 3, A, &wild, RawDoc::from_bytes(&bytes).unwrap(), &pk, None).unwrap();
        let entries = entries.into_iter().map(|(k, bits)| (k, entry_value(&wild, &pk, &bits)));
        let entries = entries.collect();
        let mut s = RangeState {
            indexes: IndexSet { tenant: 0, collection: 3, indexes: vec![(A, wild)] },
            ready: BTreeMap::from([(A, Some(hlc(1)))]),
            at: hlc(2),
            docs: vec![(bytes, pk)],
            entries,
            multikey_paths: BTreeMap::new(),
        };
        assert!(check(&s).unwrap_err().detail.contains("multikey path x"));
        s.multikey_paths.insert(A, BTreeSet::from(["x".to_string()]));
        assert_eq!(check(&s), Ok(()));
    }
}
