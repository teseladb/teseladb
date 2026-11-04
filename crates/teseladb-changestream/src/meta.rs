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
//! Its bytes: format (2, or 3 with a retryable write), flags (bit 0 transaction, bit 1 description, bit 2 replacement, bit 3
//! deletion, bit 4 document, bit 5 moved here, bit 6 moved away, bits 5 and 6 keyed again by
//! `shardCollection`), position (4 bytes,
//! big-endian); with a transaction its identifier (16), `lsid.id` (16), `lsid.uid` (32) and
//! `txnNumber` (8, big-endian); with a retryable write (flag bit 7, format 3 only) its
//! [`RetryInfo`] in [`RetryInfo::BYTES`] bytes; with a description its length (4, big-endian) and its BSON; for a
//! document that moved here, the length (4, big-endian) and the bytes of the key it left; with a
//! document, the document's bytes to the end.
//!
//! A document moves when an update changes its shard key (018 FR-001): its key is the shard key
//! and its `_id`, so the write deletes it at the old key and puts it at the new one. A `refrouter`
//! 9.0.4 gives one `update` (or `replace`) event for it, keyed by the document before the change
//! (measured on 02-11-2026): the meta at the new key says where it came from, and the meta of the
//! deletion at the old key that it gives no event.

use teseladb_bson::{BsonError, Document, RawDoc, Value};
use teseladb_hlc::Hlc;

use crate::update_desc::{DESCRIPTION_LIMITS, DescError, UpdateDescription};

/// Format of the bytes this module writes for a meta without a retryable write.
pub const META_FORMAT: u8 = 2;
/// Format of the bytes of a meta with a retryable write ([`RetryInfo`], 018 FR-009).
pub const META_FORMAT_RETRY: u8 = 3;
/// Flag of a meta with a retryable write.
const HAS_RETRY: u8 = 128;
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
/// Flag of the meta of a document that moved here from another key.
const MOVED_FROM: u8 = 32;
/// Flag of the meta of the deletion of a document that moved to another key.
const MOVED_AWAY: u8 = 64;
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

/// The retryable write that wrote a document outside a transaction (018 FR-009): what its oplog
/// entry carries (`lsid`, `txnNumber`, `stmtId`, `prevOpTime`, `needsRetryImage`), as a replica
/// set member of RefDb 9.0.4 writes it (measured on 02-11-2026).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryInfo {
    /// `lsid.id`.
    pub lsid: u128,
    /// `lsid.uid`: SHA-256 of the authenticated user (`user@db`), or of the empty name.
    pub uid: [u8; 32],
    /// `txnNumber`.
    pub txn_number: i64,
    /// `stmtId` of the write's first document; the next ones follow it by position.
    pub stmt_id: u32,
    /// The mark of the previous statement of the same `txnNumber` (`prevOpTime`), `None` for the
    /// first.
    pub prev: Option<Hlc>,
    /// `needsRetryImage` of a `findAndModify`: 0 none, 1 `preImage`, 2 `postImage`.
    pub image: u8,
}

impl RetryInfo {
    /// Bytes of its encoding.
    pub const BYTES: usize = 16 + 32 + 8 + 4 + 8 + 4 + 1;

    /// Its bytes; a previous mark of `Hlc(0, 0)` stands for none.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::BYTES);
        out.extend_from_slice(&self.lsid.to_be_bytes());
        out.extend_from_slice(&self.uid);
        out.extend_from_slice(&self.txn_number.to_be_bytes());
        out.extend_from_slice(&self.stmt_id.to_be_bytes());
        let prev = self.prev.unwrap_or(Hlc { physical: 0, logical: 0 });
        out.extend_from_slice(&prev.physical.to_be_bytes());
        out.extend_from_slice(&prev.logical.to_be_bytes());
        out.push(self.image);
        out
    }

    /// Reads what [`RetryInfo::to_bytes`] wrote from the front of `b`.
    pub fn take_from(b: &mut &[u8]) -> Result<RetryInfo, MetaError> {
        let lsid = u128::from_be_bytes(take::<16>(b)?);
        let uid = take::<32>(b)?;
        let txn_number = i64::from_be_bytes(take::<8>(b)?);
        let stmt_id = u32::from_be_bytes(take::<4>(b)?);
        let physical = u64::from_be_bytes(take::<8>(b)?);
        let logical = u32::from_be_bytes(take::<4>(b)?);
        let [image] = take::<1>(b)?;
        if image > 2 {
            return Err(MetaError::Flags(image));
        }
        let prev = (physical != 0 || logical != 0).then_some(Hlc { physical, logical });
        Ok(RetryInfo { lsid, uid, txn_number, stmt_id, prev, image })
    }

    /// `needsRetryImage` of its oplog entry.
    pub fn image_name(&self) -> Option<&'static str> {
        match self.image {
            1 => Some("preImage"),
            2 => Some("postImage"),
            _ => None,
        }
    }
}

/// What a document's version keeps for its event.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeMeta {
    /// The transaction that wrote it.
    pub txn: Option<TxnInfo>,
    /// The retryable write that wrote it outside a transaction.
    pub retry: Option<RetryInfo>,
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
    /// The document moved: here from another key, or away from this one.
    pub moved: Option<Moved>,
}

/// How a document's key changed with its shard key (018 FR-001).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Moved {
    /// It came from this key: its event is that of the write, keyed by the version there.
    From(Vec<u8>),
    /// It left this key: the deletion gives no event.
    Away,
    /// `shardCollection` keyed it again by the new shard key: the deletion at the old key and the
    /// insert at the new one are changes of the document's key for the indexes that follow the
    /// versions (011), and no change stream or oplog entry, as a `refrouter` 9.0.4 gives only the
    /// `shardCollection` event (measured on 02-11-2026).
    Internal,
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
            + u8::from(self.doc.is_some()) * HAS_DOC
            + match self.moved {
                Some(Moved::From(_)) => MOVED_FROM,
                Some(Moved::Away) => MOVED_AWAY,
                Some(Moved::Internal) => MOVED_FROM | MOVED_AWAY,
                None => 0,
            }
            + u8::from(self.retry.is_some()) * HAS_RETRY;
        let format = if self.retry.is_some() { META_FORMAT_RETRY } else { META_FORMAT };
        let mut out = vec![format, flags];
        out.extend_from_slice(&self.op.to_be_bytes());
        if let Some(t) = &self.txn {
            out.extend_from_slice(&t.id.to_be_bytes());
            out.extend_from_slice(&t.lsid.to_be_bytes());
            out.extend_from_slice(&t.uid);
            out.extend_from_slice(&t.txn_number.to_be_bytes());
        }
        if let Some(r) = &self.retry {
            out.extend(r.to_bytes());
        }
        if let Some(d) = &self.update {
            let bytes = d.stored().to_bytes_with(DESCRIPTION_LIMITS)?;
            // A description is at most `DESCRIPTION_LIMITS`, far below 4 GiB.
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend(bytes);
        }
        if let Some(Moved::From(key)) = &self.moved {
            // A key is far below 4 GiB.
            out.extend_from_slice(&(key.len() as u32).to_be_bytes());
            out.extend_from_slice(key);
        }
        if let Some(doc) = &self.doc {
            out.extend_from_slice(doc);
        }
        Ok(out)
    }

    /// Reads the bytes [`ChangeMeta::encode`] wrote.
    pub fn decode(mut b: &[u8]) -> Result<ChangeMeta, MetaError> {
        let [format, flags] = take::<2>(&mut b)?;
        if format != META_FORMAT && format != META_FORMAT_RETRY {
            return Err(MetaError::Format(format));
        }
        let known = HAS_TXN | HAS_UPDATE | REPLACE | DELETE | HAS_DOC | MOVED_FROM | MOVED_AWAY;
        let retry_flag = if format == META_FORMAT_RETRY { HAS_RETRY } else { 0 };
        if flags & !(known | retry_flag) != 0
            || (format == META_FORMAT_RETRY && flags & HAS_RETRY == 0)
        {
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
        let retry = if flags & HAS_RETRY != 0 { Some(RetryInfo::take_from(&mut b)?) } else { None };
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
        let moved = if flags & (MOVED_FROM | MOVED_AWAY) == MOVED_FROM | MOVED_AWAY {
            Some(Moved::Internal)
        } else if flags & MOVED_FROM != 0 {
            let len = u32::from_be_bytes(take::<4>(&mut b)?) as usize;
            let (key, rest) = b.split_at_checked(len).ok_or(MetaError::Truncated)?;
            b = rest;
            Some(Moved::From(key.to_vec()))
        } else {
            (flags & MOVED_AWAY != 0).then_some(Moved::Away)
        };
        let doc = if flags & HAS_DOC != 0 {
            Some(b.to_vec())
        } else if b.is_empty() {
            None
        } else {
            return Err(MetaError::Trailing);
        };
        let (replace, delete) = (flags & REPLACE != 0, flags & DELETE != 0);
        Ok(ChangeMeta { txn, retry, op, update, replace, delete, doc, moved })
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
    /// The document moved with its shard key ([`Moved`]).
    pub moved: Option<Moved>,
}

/// The metas of the writes `writes` (in the write's order), in a transaction with its next
/// position (`txn`): one for each write it says something of, a transaction, a position above 0, a
/// description or a replacement. The key and the meta of each.
pub fn history(writes: &[DocWrite], txn: Option<(&TxnInfo, u32)>) -> Vec<(Vec<u8>, ChangeMeta)> {
    history_with(writes, txn, None)
}

/// [`history`] of a retryable write outside a transaction (`retry`): every write has a meta, as
/// its oplog entry carries the session (018 FR-009).
pub fn history_with(
    writes: &[DocWrite],
    txn: Option<(&TxnInfo, u32)>,
    retry: Option<&RetryInfo>,
) -> Vec<(Vec<u8>, ChangeMeta)> {
    let first = txn.map_or(0, |(_, n)| n);
    let retry = if txn.is_some() { None } else { retry };
    let mut out = Vec::new();
    for (op, w) in (first..).zip(writes) {
        let bare = w.update.is_none() && !w.replace && w.moved.is_none();
        if txn.is_none() && retry.is_none() && op == 0 && bare {
            continue;
        }
        let meta = ChangeMeta {
            txn: txn.map(|(t, _)| t.clone()),
            retry: retry.copied(),
            op,
            update: w.update.clone(),
            replace: w.replace,
            delete: w.delete,
            doc: w.doc.clone(),
            moved: w.moved.clone(),
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
