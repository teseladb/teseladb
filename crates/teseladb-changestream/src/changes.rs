//! The changes a range's versions give (014 FR-002, FR-003, FR-009): each version of a document,
//! with the meta written next to it at the same mark ([`crate::meta`]), becomes a change with its
//! place in the merge order and the type of its event. The versions come from a time scan of the
//! range (`scan_time`), in key order and, within a key, from the newest; the changes leave in the
//! merge order.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;

use crate::meta::{ChangeMeta, MetaError, doc_of_meta};
use crate::order::EventKey;
use crate::token::EventKind;

/// A version a time scan gave: its key, its mark and its `(value type, value)`, or `None` for a
/// deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versioned {
    /// Logical key.
    pub key: Vec<u8>,
    /// Mark.
    pub ts: Hlc,
    /// Value type and value, or `None` for a deletion.
    pub value: Option<(u8, Vec<u8>)>,
}

/// The change of one document at one mark.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Its place in the merge.
    pub event: EventKey,
    /// The type of its event.
    pub kind: EventKind,
    /// The document after the change (`None` for a deletion).
    pub doc: Option<Vec<u8>>,
    /// What the version kept for its event.
    pub meta: Option<ChangeMeta>,
}

/// Why the versions do not give their changes.
#[derive(Clone, Debug, PartialEq)]
pub enum ChangeError {
    /// A stored meta does not read back.
    Meta(MetaError),
}

/// Bytes of a data key before the rest of a document: space, tenant, collection, index.
const DOC_PREFIX: usize = 13;

/// Whether `key` is in the documents of a collection (index 0 of the data space).
pub fn is_document_key(key: &[u8]) -> bool {
    key.len() > DOC_PREFIX
        && key[0] == teseladb_keys::space::DATA
        && key[9..DOC_PREFIX] == teseladb_keys::DOCUMENTS.to_be_bytes()
}

/// The changes of `versions`, in the merge order.
pub fn changes(versions: impl IntoIterator<Item = Versioned>) -> Result<Vec<Change>, ChangeError> {
    let mut docs: BTreeMap<(Vec<u8>, Hlc), Option<Vec<u8>>> = BTreeMap::new();
    let mut metas: BTreeMap<(Vec<u8>, Hlc), ChangeMeta> = BTreeMap::new();
    for v in versions {
        if !is_document_key(&v.key) {
            continue;
        }
        match v.value {
            Some((crate::HISTORY_VALUE, bytes)) => {
                let meta = ChangeMeta::decode(&bytes).map_err(ChangeError::Meta)?;
                if let Some(doc) = doc_of_meta(&v.key) {
                    metas.insert((doc.to_vec(), v.ts), meta);
                }
            }
            Some((crate::DOCUMENT_VALUE, bytes)) => {
                docs.insert((v.key, v.ts), Some(bytes));
            }
            Some(_) => {}
            None => {
                docs.insert((v.key, v.ts), None);
            }
        }
    }
    let mut out: Vec<Change> = docs
        .into_iter()
        .map(|((key, ts), doc)| {
            let meta = metas.remove(&(key.clone(), ts));
            let kind = match (&doc, &meta) {
                (None, _) => EventKind::Delete,
                (Some(_), Some(m)) if m.update.is_some() => EventKind::Update,
                (Some(_), Some(m)) if m.replace => EventKind::Replace,
                (Some(_), _) => EventKind::Insert,
            };
            let (txn, op) =
                meta.as_ref().map_or((0, 0), |m| (m.txn.as_ref().map_or(0, |t| t.id), m.op));
            Change { event: EventKey { ts, txn, op, key }, kind, doc, meta }
        })
        .collect();
    out.sort_by(|a, b| a.event.cmp(&b.event));
    Ok(out)
}
