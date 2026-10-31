//! The index changes of a document mutation (FR-003): the entries of each index for the version
//! in force and for the new one, and the difference. An entry whose key stays but whose type bits
//! change (1 becomes 1.0) is rewritten. The leader proposes these changes with the document, in one
//! phase when every key falls in the document's range and in an internal transaction otherwise.

use std::collections::BTreeMap;

use teseladb_bson::{Collator, RawDoc};
use teseladb_keys::{Key, PrimaryKey};

use crate::keygen::{IndexDef, KeygenError, entry_value, index_entries};

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
        let mut out = BTreeMap::new();
        for (id, def) in &self.indexes {
            let entries =
                index_entries(self.tenant, self.collection, *id, def, v.doc, v.pk, collation)?;
            out.extend(entries.into_iter().map(|(k, bits)| (k, entry_value(def, v.pk, &bits))));
        }
        Ok(out)
    }

    /// The changes from version `old` to version `new` (`None`: the document does not exist).
    /// A new version an index refuses fails the mutation; the old one was accepted when written.
    pub fn delta(
        &self,
        old: Option<Version<'_>>,
        new: Option<Version<'_>>,
        collation: Option<&dyn Collator>,
    ) -> Result<Delta, KeygenError> {
        let before = old.map(|v| self.keys(v, collation)).transpose()?.unwrap_or_default();
        let after = new.map(|v| self.keys(v, collation)).transpose()?.unwrap_or_default();
        let remove = before.keys().filter(|k| !after.contains_key(*k)).cloned().collect();
        let add = after.into_iter().filter(|(k, value)| before.get(k) != Some(value)).collect();
        Ok(Delta { remove, add })
    }
}
