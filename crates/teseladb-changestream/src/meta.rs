//! What a document's version keeps for its event (014 FR-002, FR-005, FR-009): the description of
//! an update by operators, the transaction that wrote it (`lsid`, `txnNumber`) and its position in
//! the write. It is stored next to the document, at key `document · 00` and at the same mark, as a
//! version that only serves the history (`teseladb_lsm::HISTORY_VALUE`): written in the same entry
//! or the same intents as the document, it goes wherever the document goes, and the GC drops it
//! once it leaves the retention window.
//!
//! Its bytes: format (1), flags (bit 0 transaction, bit 1 description), position (4 bytes,
//! big-endian); with a transaction its identifier (16), `lsid.id` (16), `lsid.uid` (32) and
//! `txnNumber` (8, big-endian); with a description, the description's BSON to the end.

use teseladb_bson::{BsonError, Document, RawDoc, Value};

use crate::update_desc::{DESCRIPTION_LIMITS, DescError, UpdateDescription};

/// Format of the bytes this module writes.
pub const META_FORMAT: u8 = 1;
/// Flag of a meta with a transaction.
const HAS_TXN: u8 = 1;
/// Flag of a meta with a description.
const HAS_UPDATE: u8 = 2;
/// Flag of the meta of a replacement.
const REPLACE: u8 = 4;
/// Byte that follows a document's key in the key of its meta.
pub const META_SUFFIX: u8 = 0;
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
            + u8::from(self.replace) * REPLACE;
        let mut out = vec![META_FORMAT, flags];
        out.extend_from_slice(&self.op.to_be_bytes());
        if let Some(t) = &self.txn {
            out.extend_from_slice(&t.id.to_be_bytes());
            out.extend_from_slice(&t.lsid.to_be_bytes());
            out.extend_from_slice(&t.uid);
            out.extend_from_slice(&t.txn_number.to_be_bytes());
        }
        if let Some(d) = &self.update {
            out.extend(d.to_document(true).to_bytes_with(DESCRIPTION_LIMITS)?);
        }
        Ok(out)
    }

    /// Reads the bytes [`ChangeMeta::encode`] wrote.
    pub fn decode(mut b: &[u8]) -> Result<ChangeMeta, MetaError> {
        let [format, flags] = take::<2>(&mut b)?;
        if format != META_FORMAT {
            return Err(MetaError::Format(format));
        }
        if flags & !(HAS_TXN | HAS_UPDATE | REPLACE) != 0 {
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
            let raw = RawDoc::from_bytes_with(b, DESCRIPTION_LIMITS)
                .map_err(|e| MetaError::Description(DescError::Bson(e)))?;
            let d = UpdateDescription::from_document(&Document::from(raw))
                .map_err(MetaError::Description)?;
            Some(d)
        } else if b.is_empty() {
            None
        } else {
            return Err(MetaError::Trailing);
        };
        Ok(ChangeMeta { txn, op, update, replace: flags & REPLACE != 0 })
    }
}

/// The key of the meta of the document at `doc`: right after it, before any other key, because
/// the encoding of a document's key is self-delimiting.
pub fn meta_key(doc: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(doc.len() + 1);
    k.extend_from_slice(doc);
    k.push(META_SUFFIX);
    k
}

/// The document's key of the meta at `key`.
pub fn doc_of_meta(key: &[u8]) -> Option<&[u8]> {
    key.split_last().map(|(_, doc)| doc)
}

/// The metas of a write of documents `docs` (in the write's order) with descriptions `updates`,
/// replacing the documents `replaced`, in a transaction with its next position (`txn`): one for
/// each document it says something of, a transaction, a position above 0, a description or a
/// replacement.
pub fn history(
    docs: &[Vec<u8>],
    (updates, replaced): (&[(Vec<u8>, UpdateDescription)], &[Vec<u8>]),
    txn: Option<(&TxnInfo, u32)>,
) -> Vec<(Vec<u8>, ChangeMeta)> {
    let first = txn.map_or(0, |(_, n)| n);
    let mut out = Vec::new();
    for (op, doc) in (first..).zip(docs) {
        let update = updates.iter().find(|(k, _)| k == doc).map(|(_, d)| d.clone());
        let replace = replaced.contains(doc);
        if txn.is_none() && op == 0 && update.is_none() && !replace {
            continue;
        }
        let meta = ChangeMeta { txn: txn.map(|(t, _)| t.clone()), op, update, replace };
        out.push((meta_key(doc), meta));
    }
    out
}

/// The `lsid` of an event (measured on a `refrouter` 9.0.4 on 01-11): `{id: UUID, uid: BinData(0)}`.
pub fn lsid_document(lsid: u128, uid: &[u8; 32]) -> Document {
    Document::new()
        .with("id", Value::Binary { subtype: UUID_SUBTYPE, bytes: lsid.to_be_bytes().to_vec() })
        .with("uid", Value::Binary { subtype: GENERIC_SUBTYPE, bytes: uid.to_vec() })
}
