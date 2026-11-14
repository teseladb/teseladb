//! The index changes of a document mutation (FR-003): the entries of each index for the version
//! in force and for the new one, and the difference. An entry whose key stays but whose type bits
//! change (1 becomes 1.0) is rewritten. The leader proposes these changes with the document, in one
//! phase when every key falls in the document's range and in an internal transaction otherwise.
//! An index whose fields the two versions have alike is left out of the computation (FR-022).

use std::collections::BTreeMap;

use teseladb_bson::{Collator, RawDoc, RawValue};
use teseladb_keys::{Key, PrimaryKey};

use crate::keygen::{FieldKind, IndexDef, KeygenError, entry_value, index_entries};

/// The indexes of a collection, by number.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexSet {
    /// Tenant.
    pub tenant: u32,
    /// Collection.
    pub collection: u32,
    /// Index number and definition.
    pub indexes: Vec<(u32, IndexDef)>,
}

/// One version of a document: its bytes and its primary key.
#[derive(Clone, Copy, Debug)]
pub struct Version<'a> {
    /// The document.
    pub doc: RawDoc<'a>,
    /// Its primary key (003 FR-012).
    pub pk: &'a PrimaryKey,
}

/// The entries to delete and to write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    /// Keys to delete.
    pub remove: Vec<Key>,
    /// Keys to write, with their values ([`entry_value`]: the type bits, and in a unique index the
    /// document's primary key before them).
    pub add: Vec<(Key, Vec<u8>)>,
}

impl Delta {
    /// Whether nothing changes.
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.add.is_empty()
    }
}

impl IndexSet {
    /// Every entry of `v` in every index with its value, in key order.
    pub fn keys(
        &self,
        v: Version<'_>,
        collation: Option<&dyn Collator>,
    ) -> Result<BTreeMap<Key, Vec<u8>>, KeygenError> {
        self.keys_of(self.indexes.iter(), v, collation)
    }

    /// Every entry of `v` in the indexes `which` with its value, in key order.
    fn keys_of<'i>(
        &self,
        which: impl Iterator<Item = &'i (u32, IndexDef)>,
        v: Version<'_>,
        collation: Option<&dyn Collator>,
    ) -> Result<BTreeMap<Key, Vec<u8>>, KeygenError> {
        let mut out = BTreeMap::new();
        for (id, def) in which {
            let entries =
                index_entries(self.tenant, self.collection, *id, def, v.doc, v.pk, collation)?;
            out.extend(entries.into_iter().map(|(k, bits)| (k, entry_value(def, v.pk, &bits))));
        }
        Ok(out)
    }

    /// The changes from version `old` to version `new` (`None`: the document does not exist).
    /// A new version an index refuses fails the mutation; the old one was accepted when written.
    /// An index whose fields the two versions have alike has the same entries in both and is
    /// not computed (FR-022): most updates leave the fields of most indexes as they were.
    pub fn delta(
        &self,
        old: Option<Version<'_>>,
        new: Option<Version<'_>>,
        collation: Option<&dyn Collator>,
    ) -> Result<Delta, KeygenError> {
        let touched: Vec<&(u32, IndexDef)> = self
            .indexes
            .iter()
            .filter(|(_, def)| match (old, new) {
                (Some(o), Some(n)) => !alike(def, o, n),
                _ => true,
            })
            .collect();
        let keys = |v| self.keys_of(touched.iter().copied(), v, collation);
        let before = old.map(keys).transpose()?.unwrap_or_default();
        let after = new.map(keys).transpose()?.unwrap_or_default();
        let remove = before.keys().filter(|k| !after.contains_key(*k)).cloned().collect();
        let add = after.into_iter().filter(|(k, value)| before.get(k) != Some(value)).collect();
        Ok(Delta { remove, add })
    }
}

/// Whether versions `old` and `new` have the same entries in an index of definition `def`,
/// without computing them (FR-022): the same primary key and, for every field of the index, the
/// same top-level field of the document, byte for byte. Only fields that store the value at
/// their path or its hash are looked at this way: a wildcard field reaches fields its path does
/// not name, and the geographic kinds are computed as before.
fn alike(def: &IndexDef, old: Version<'_>, new: Version<'_>) -> bool {
    old.pk == new.pk
        && def.fields.iter().all(|f| {
            matches!(f.kind, FieldKind::Ordered | FieldKind::Hashed)
                && same(top_level(old.doc, &f.path), top_level(new.doc, &f.path))
        })
}

/// The top-level field of `doc` that the dotted `path` goes through.
fn top_level<'a>(doc: RawDoc<'a>, path: &str) -> Option<RawValue<'a>> {
    let name = path.split('.').next().unwrap_or(path);
    doc.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// Whether two values are the same bytes: `0.0` and `-0.0` differ in an entry's type bits, and
/// a NaN is the same value as itself.
fn same(a: Option<RawValue<'_>>, b: Option<RawValue<'_>>) -> bool {
    match (a, b) {
        (Some(RawValue::Double(x)), Some(RawValue::Double(y))) => x.to_bits() == y.to_bits(),
        (a, b) => a == b,
    }
}
