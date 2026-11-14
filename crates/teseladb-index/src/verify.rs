//! Index-document verification of one range for `validate` (FR-016, `index-maintenance.md`): the
//! entries every index must hold for the documents the range has at a time, compared with the
//! entries it holds at that time. The caller reads both with one snapshot and passes them in key
//! order; this module only compares, so it is the same check the simulator runs (FR-017).
//!
//! What it reports is what `validate` shows, measured on the reference server 9.0.4 with the fail
//! points that skip index writes: per index, the entries it holds (`keysPerIndex`, which in the
//! reference server also counts one entry per multikey path of a wildcard index), the entries a
//! document needs and the index lacks (`missingIndexEntries`), the entries no document needs
//! (`extraIndexEntries`), and for a wildcard index the multikey paths the catalog lacks
//! (`Index '<name>' has one or more missing multikey metadata index keys`). An entry whose value
//! differs from the one the document gives (other type bits, or in a unique index another
//! document's primary key) is both missing and extra.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_bson::Collator;
use teseladb_keys::{Key, KeyParts, PrimaryKey};

use crate::delta::{IndexSet, Version};
use crate::keygen::{FieldKind, KeygenError, entry_value, index_entries, key_values};

/// An entry a document needs and the index lacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingEntry {
    /// The entry.
    pub key: Key,
    /// Its value ([`entry_value`]).
    pub value: Vec<u8>,
    /// The document's primary key (the reference server's `idKey`).
    pub pk: PrimaryKey,
}

/// What the verification found in one index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// Entries held, plus the multikey paths the catalog records for a wildcard index.
    pub entries: u64,
    /// Entries needed and not held.
    pub missing: Vec<MissingEntry>,
    /// Entries held and not needed, with their values.
    pub extra: Vec<(Key, Vec<u8>)>,
    /// Multikey paths of a wildcard index that some document walks and the catalog lacks.
    pub missing_multikey_paths: Vec<String>,
}

impl IndexReport {
    /// Whether the index agrees with the documents.
    pub fn valid(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.missing_multikey_paths.is_empty()
    }
}

/// What the verification of a range found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Documents seen (`nrecords`).
    pub records: u64,
    /// Per index number, in order.
    pub indexes: BTreeMap<u32, IndexReport>,
    /// Entries of the collection under an index number the set does not have.
    pub unknown: Vec<Key>,
}

impl VerifyReport {
    /// Whether every index agrees with the documents (`valid`).
    pub fn valid(&self) -> bool {
        self.unknown.is_empty() && self.indexes.values().all(IndexReport::valid)
    }
}

/// The index an entry of the set's collection belongs to.
fn index_of(set: &IndexSet, key: &Key) -> Option<u32> {
    match KeyParts::of(key.as_bytes()) {
        Ok(KeyParts::Data { tenant, collection, index, .. })
            if tenant == set.tenant && collection == set.collection && index != 0 =>
        {
            Some(index)
        }
        _ => None,
    }
}

/// Verifies the indexes of `set` against `docs`, the range's documents at the time read, and
/// `held`, the index entries of the collection in the range at that time with their values. `multikey_paths` are
/// the multikey paths the catalog records per wildcard index. A document an index refuses (it
/// was accepted when written, so the definition changed) fails the verification.
pub fn verify_range<'a>(
    set: &IndexSet,
    docs: impl IntoIterator<Item = Version<'a>>,
    held: impl IntoIterator<Item = (Key, Vec<u8>)>,
    multikey_paths: &BTreeMap<u32, BTreeSet<String>>,
    collation: Option<&dyn Collator>,
) -> Result<VerifyReport, KeygenError> {
    let mut report = VerifyReport::default();
    let mut needed: BTreeMap<u32, BTreeMap<Key, (Vec<u8>, PrimaryKey)>> = BTreeMap::new();
    let mut walked: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for (id, _) in &set.indexes {
        report.indexes.insert(*id, IndexReport::default());
        needed.insert(*id, BTreeMap::new());
    }
    for v in docs {
        report.records += 1;
        for (id, def) in &set.indexes {
            let entries =
                index_entries(set.tenant, set.collection, *id, def, v.doc, v.pk, collation)?;
            let slot = needed.entry(*id).or_default();
            for (key, bits) in entries {
                slot.insert(key, (entry_value(def, v.pk, &bits), v.pk.clone()));
            }
            if def.fields.iter().any(|f| matches!(f.kind, FieldKind::Wildcard(_))) {
                let paths = key_values(def, v.doc)?.multikey_paths;
                walked.entry(*id).or_default().extend(paths);
            }
        }
    }
    for (key, value) in held {
        let Some(id) = index_of(set, &key) else { continue };
        let Some(index) = report.indexes.get_mut(&id) else {
            report.unknown.push(key);
            continue;
        };
        index.entries += 1;
        let wanted = needed.get_mut(&id).and_then(|n| n.remove(&key));
        match wanted {
            // The local identifier an entry carries (009 FR-023) is no part of what its document
            // gives it.
            Some((want, _)) if want == teseladb_keys::local_id::entry_rest(&value) => {}
            Some((want, pk)) => {
                index.missing.push(MissingEntry { key: key.clone(), value: want, pk });
                index.extra.push((key, value));
            }
            None => index.extra.push((key, value)),
        }
    }
    for (id, index) in report.indexes.iter_mut() {
        let recorded = multikey_paths.get(id);
        index.entries += recorded.map_or(0, |p| p.len() as u64);
        if let Some(rest) = needed.remove(id) {
            index.missing.extend(rest.into_iter().map(|(key, (value, pk))| MissingEntry {
                key,
                value,
                pk,
            }));
        }
        index.missing.sort_by(|a, b| a.key.cmp(&b.key));
        if let Some(paths) = walked.get(id) {
            index.missing_multikey_paths = paths
                .iter()
                .filter(|p| !recorded.is_some_and(|r| r.contains(*p)))
                .cloned()
                .collect();
        }
    }
    Ok(report)
}
