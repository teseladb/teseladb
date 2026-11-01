//! What a document's version keeps for its event (014 FR-002, FR-005, FR-009, D-114): the
//! description of an update by operators, the transaction that wrote it (`lsid`, `txnNumber`),
//! its position in the write or the transaction and, inside a transaction, the type of each write
//! and the document it left. It is stored next to the document, at key `document · 00 · position`
//! and at the same mark, as a version that only serves the history
//! (`teseladb_lsm::HISTORY_VALUE`): written in the same entry or the same intents as the document,
//! it goes wherever the document goes, and the GC drops it once it leaves the retention window. A
//! transaction that writes a document several times leaves one meta per write, as a `refrouter`
//! 9.0.4 gives one event per write (measured on 01-11).
//!
//! Its bytes: format (2), flags (bit 0 transaction, bit 1 description, bit 2 replacement, bit 3
//! deletion, bit 4 document), position (4 bytes, big-endian); with a transaction its identifier
//! (16), `lsid.id` (16), `lsid.uid` (32) and `txnNumber` (8, big-endian); with a description its
//! length (4, big-endian) and its BSON; with a document, the document's bytes to the end.

use teseladb_bson::{BsonError, Document, RawDoc, Value};

use crate::update_desc::{DESCRIPTION_LIMITS, DescError, UpdateDescription};

/// Format of the bytes this module writes.
pub const META_FORMAT: u8 = 2;
/// Flag of a meta with a transaction.
const HAS_TXN: u8 = 1;
/// Flag of a meta with a description.
const HAS_UPDATE: u8 = 2;
/// Flag of the meta of a replacement.
const REPLACE: u8 = 4;
/// Flag of the meta of a deletion.
const DELETE: u8 = 8;
/// Flag of a meta with the document the write left.
const HAS_DOC: u8 = 16;
/// Byte that follows a document's key in the key of its metas.
pub const META_SUFFIX: u8 = 0;
/// Bytes after a document's key in the key of a meta: the suffix and the position.
pub const META_KEY_TAIL: usize = 5;
/// BSON binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;
/// BSON binary subtype of generic bytes.
const GENERIC_SUBTYPE: u8 = 0;

/// The transaction of a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnInfo {
    /// The transaction's identifier (it orders two transactions with the same mark, FR-005).
    pub id: u128,
    /// `lsid.id`.
    pub lsid: u128,
    /// `lsid.uid`: SHA-256 of the authenticated user (`user@db`), or of the empty name.
    pub uid: [u8; 32],
    /// `txnNumber`.
    pub txn_number: i64,
}

/// What a document's version keeps for its event.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeMeta {
    /// The transaction that wrote it.
    pub txn: Option<TxnInfo>,
    /// Its position in the write or the transaction.
    pub op: u32,
    /// The description of an update by operators.
    pub update: Option<UpdateDescription>,
    /// The write replaced the document whole (a `replace` event; without it a version of a
    /// document is an `insert`, or an `update` with a description).
    pub replace: bool,
    /// The write deleted the document (a `delete` event).
    pub delete: bool,
    /// The document the write left, kept inside a transaction: a later write of the transaction
    /// replaces its version.
    pub doc: Option<Vec<u8>>,
}

/// Why a stored meta does not read back.
#[derive(Clone, Debug, PartialEq)]
pub enum MetaError {
    /// Shorter than its fields.
    Truncated,
    /// A format this node does not know.
    Format(u8),
    /// Flags no meta has.
    Flags(u8),
    /// The description does not read back.
    Description(DescError),
    /// Bytes after a meta without a description.
    Trailing,
}

fn take<const N: usize>(b: &mut &[u8]) -> Result<[u8; N], MetaError> {
    let (head, rest) = b.split_first_chunk::<N>().ok_or(MetaError::Truncated)?;
    *b = rest;
    Ok(*head)
}

impl ChangeMeta {
    /// The bytes stored with the version.
    pub fn encode(&self) -> Result<Vec<u8>, BsonError> {
        let flags = u8::from(self.txn.is_some()) * HAS_TXN
            + u8::from(self.update.is_some()) * HAS_UPDATE
            + u8::from(self.replace) * REPLACE
            + u8::from(self.delete) * DELETE
            + u8::from(self.doc.is_some()) * HAS_DOC;
        let mut out = vec![META_FORMAT, flags];
        out.extend_from_slice(&self.op.to_be_bytes());
        if let Some(t) = &self.txn {
            out.extend_from_slice(&t.id.to_be_bytes());
            out.extend_from_slice(&t.lsid.to_be_bytes());
            out.extend_from_slice(&t.uid);
            out.extend_from_slice(&t.txn_number.to_be_bytes());
        }
        if let Some(d) = &self.update {
            let bytes = d.to_document(true).to_bytes_with(DESCRIPTION_LIMITS)?;
            // A description is at most `DESCRIPTION_LIMITS`, far below 4 GiB.
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend(bytes);
        }
        if let Some(doc) = &self.doc {
            out.extend_from_slice(doc);
        }
        Ok(out)
    }

    /// Reads the bytes [`ChangeMeta::encode`] wrote.
    pub fn decode(mut b: &[u8]) -> Result<ChangeMeta, MetaError> {
        let [format, flags] = take::<2>(&mut b)?;
        if format != META_FORMAT {
            return Err(MetaError::Format(format));
        }
        if flags & !(HAS_TXN | HAS_UPDATE | REPLACE | DELETE | HAS_DOC) != 0 {
            return Err(MetaError::Flags(flags));
        }
        let op = u32::from_be_bytes(take::<4>(&mut b)?);
        let txn = if flags & HAS_TXN != 0 {
            Some(TxnInfo {
                id: u128::from_be_bytes(take::<16>(&mut b)?),
                lsid: u128::from_be_bytes(take::<16>(&mut b)?),
                uid: take::<32>(&mut b)?,
                txn_number: i64::from_be_bytes(take::<8>(&mut b)?),
            })
        } else {
            None
        };
        let update = if flags & HAS_UPDATE != 0 {
            let len = u32::from_be_bytes(take::<4>(&mut b)?) as usize;
            let (desc, rest) = b.split_at_checked(len).ok_or(MetaError::Truncated)?;
            b = rest;
            let raw = RawDoc::from_bytes_with(desc, DESCRIPTION_LIMITS)
                .map_err(|e| MetaError::Description(DescError::Bson(e)))?;
            let d = UpdateDescription::from_document(&Document::from(raw))
                .map_err(MetaError::Description)?;
            Some(d)
        } else {
            None
        };
        let doc = if flags & HAS_DOC != 0 {
            Some(b.to_vec())
        } else if b.is_empty() {
            None
        } else {
            return Err(MetaError::Trailing);
        };
        let (replace, delete) = (flags & REPLACE != 0, flags & DELETE != 0);
        Ok(ChangeMeta { txn, op, update, replace, delete, doc })
    }
}

/// The key of the meta of the write at position `op` of the document at `doc`: right after the
/// document, before any other document, because the encoding of a document's key is
/// self-delimiting.
pub fn meta_key(doc: &[u8], op: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(doc.len() + META_KEY_TAIL);
    k.extend_from_slice(doc);
    k.push(META_SUFFIX);
    k.extend_from_slice(&op.to_be_bytes());
    k
}

/// The document's key of the meta at `key`.
pub fn doc_of_meta(key: &[u8]) -> Option<&[u8]> {
    let doc = key.get(..key.len().checked_sub(META_KEY_TAIL)?)?;
    (key[doc.len()] == META_SUFFIX).then_some(doc)
}

/// A write of a document, for its meta.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocWrite {
    /// The document's key.
    pub key: Vec<u8>,
    /// The description of an update by operators.
    pub update: Option<UpdateDescription>,
    /// A replacement.
    pub replace: bool,
    /// A deletion.
    pub delete: bool,
    /// The document it left, kept inside a transaction.
    pub doc: Option<Vec<u8>>,
}

/// The metas of the writes `writes` (in the write's order), in a transaction with its next
/// position (`txn`): one for each write it says something of, a transaction, a position above 0, a
/// description or a replacement. The key and the meta of each.
pub fn history(writes: &[DocWrite], txn: Option<(&TxnInfo, u32)>) -> Vec<(Vec<u8>, ChangeMeta)> {
    let first = txn.map_or(0, |(_, n)| n);
    let mut out = Vec::new();
    for (op, w) in (first..).zip(writes) {
        if txn.is_none() && op == 0 && w.update.is_none() && !w.replace {
            continue;
        }
        let meta = ChangeMeta {
            txn: txn.map(|(t, _)| t.clone()),
            op,
            update: w.update.clone(),
            replace: w.replace,
            delete: w.delete,
            doc: w.doc.clone(),
        };
        out.push((meta_key(&w.key, op), meta));
    }
    out
}

/// The `lsid` of an event (measured on a `refrouter` 9.0.4 on 01-11): `{id: UUID, uid: BinData(0)}`.
pub fn lsid_document(lsid: u128, uid: &[u8; 32]) -> Document {
    Document::new()
        .with("id", Value::Binary { subtype: UUID_SUBTYPE, bytes: lsid.to_be_bytes().to_vec() })
        .with("uid", Value::Binary { subtype: GENERIC_SUBTYPE, bytes: uid.to_vec() })
}
