//! Unique indexes (FR-004, research R3). The key of a unique entry leaves out the primary key,
//! which goes in its value, so two documents with the same values write the same key and the
//! conflict shows as a key that holds another document's primary key. The leader reads the
//! current version of each unique key a write puts (it joins the write's keys and latch) and
//! [`UniqueGuard`] decides, also between the statements of one write. A `DuplicateKey` carries the
//! index's `keyPattern` and the conflicting `keyValue`, with the message of a `refrouter` 9.0.4
//! (measured): `E11000 duplicate key error collection: msgs.c index: b_u dup key: { b: 0 }`.

use std::collections::BTreeMap;

use teseladb_bson::{BsonError, Collator, Document, RawDoc, Value};
use teseladb_keys::space::split_unique_value;
use teseladb_keys::{KeyBuilder, KeyError};

use crate::delta::IndexSet;
use crate::keygen::{KeyValue, KeygenError, key_values};
use crate::shell::shell_doc;

/// Why a write cannot put its unique entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UniqueError {
    /// Another document holds this key, or an earlier statement of the write took it.
    Duplicate {
        /// The key.
        key: Vec<u8>,
    },
    /// The stored value of a unique key is not a unique entry's.
    Malformed(KeyError),
}

/// The unique keys a write has put or removed so far, with the primary key that holds each one
/// now (`None`: removed).
#[derive(Clone, Debug, Default)]
pub struct UniqueGuard {
    written: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl UniqueGuard {
    /// Admits one statement: it removes the keys `removed` and puts the unique entries `added`
    /// (key and the document's primary key). `stored` gives the stored value of a key the leader
    /// read. A key held by another primary key refuses the statement, which then records nothing.
    pub fn admit<'s>(
        &mut self,
        removed: &[Vec<u8>],
        added: &[(Vec<u8>, Vec<u8>)],
        stored: &dyn Fn(&[u8]) -> Option<&'s [u8]>,
    ) -> Result<(), UniqueError> {
        for (key, pk) in added {
            let holder = match self.written.get(key) {
                Some(now) => now.as_deref(),
                None => match stored(key) {
                    Some(value) => {
                        Some(split_unique_value(value).map_err(UniqueError::Malformed)?.0)
                    }
                    None => None,
                },
            };
            // The statement's own removals free a key before it puts it again.
            let freed = removed.contains(key);
            if !freed && holder.is_some_and(|h| h != pk.as_slice()) {
                return Err(UniqueError::Duplicate { key: key.clone() });
            }
        }
        for key in removed {
            self.written.insert(key.clone(), None);
        }
        for (key, pk) in added {
            self.written.insert(key.clone(), Some(pk.clone()));
        }
        Ok(())
    }
}

/// The values of the entry of `doc` whose key is `key` in the unique index `id` of `set` (empty if
/// it has none), in the order of the index's fields.
pub fn conflicting_values(
    set: &IndexSet,
    id: u32,
    doc: RawDoc<'_>,
    key: &[u8],
    collation: Option<&dyn Collator>,
) -> Result<Vec<Value>, KeygenError> {
    let Some((_, def)) = set.indexes.iter().find(|(i, _)| *i == id) else { return Ok(Vec::new()) };
    let spec = def.key_spec();
    for tuple in key_values(def, doc)?.tuples {
        let raw: Vec<_> = tuple.iter().map(KeyValue::raw).collect();
        let (k, _) =
            KeyBuilder::unique_index(set.tenant, set.collection, id, &spec, &raw, collation);
        if k.as_bytes() == key {
            return Ok(raw.into_iter().map(Value::from).collect());
        }
    }
    Ok(Vec::new())
}

/// What a `DuplicateKey` (11000) of a unique index says.
#[derive(Clone, Debug, PartialEq)]
pub struct DuplicateKey {
    /// `E11000 duplicate key error collection: <ns> index: <name> dup key: { … }`.
    pub message: String,
    /// The index's key pattern.
    pub key_pattern: Document,
    /// The conflicting values under the pattern's field names.
    pub key_value: Document,
}

/// The `DuplicateKey` of index `name` with pattern `pattern` in namespace `ns` for `values`.
pub fn duplicate_key(
    ns: &str,
    name: &str,
    pattern: &Document,
    values: &[Value],
) -> Result<DuplicateKey, BsonError> {
    let key_value = pattern
        .iter()
        .zip(values)
        .fold(Document::new(), |d, ((field, _), v)| d.with(field, v.clone()));
    let bytes = key_value.to_bytes()?;
    let shown = shell_doc(RawDoc::from_bytes(&bytes)?);
    Ok(DuplicateKey {
        message: format!(
            "E11000 duplicate key error collection: {ns} index: {name} dup key: {shown}"
        ),
        key_pattern: pattern.clone(),
        key_value,
    })
}
