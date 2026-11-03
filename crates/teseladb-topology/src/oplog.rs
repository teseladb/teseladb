//! The entries of `local.oplog.rs` (018 FR-009, contract `oplog-format.md`): their encoding with
//! RefDb's field order, their decoding for `applyOps` and `refrestore --oplogReplay`, and the
//! `applyOps` entries of a transaction.
//!
//! Field order, measured on RefDb 9.0.4: `lsid`, `txnNumber`, `op`, `ns`, `ui`, `o`, `o2`,
//! `needsRetryImage`, `stmtId`, the fields this module does not interpret (such as
//! `versionContext`), `ts`, `t`, `v`, `wall`, `prevOpTime` and `multiOpType` (the `applyOps`
//! entry of a retryable insert of several documents). A transaction's operations go inside `applyOps` entries on
//! `admin.$cmd`; RefDb fills each one while its `o` stays within 16 MiB, marks every entry but
//! the last with `partialTxn: true`, gives the last the total `count`, and chains them with
//! `prevOpTime`. One operation larger than that goes alone.

use crate::replset::{OpTime, Timestamp};
use teseladb_bson::{Document, Value};

/// The oplog's namespace.
pub const OPLOG_NS: &str = "local.oplog.rs";
/// Namespace of the `applyOps` entries of transactions.
pub const APPLY_OPS_NS: &str = "admin.$cmd";
/// Largest `o` of a transaction's `applyOps` entry before it splits.
pub const MAX_APPLY_OPS_BYTES: usize = 16 * 1024 * 1024;
/// `prevOpTime` of the first entry of a transaction: `{ts: Timestamp(0, 0), t: -1}`.
pub const NULL_OP_TIME: OpTime = OpTime { ts: Timestamp { time: 0, increment: 0 }, term: -1 };
/// `v` of every entry RefDb 9.0 writes.
pub const ENTRY_VERSION: i64 = 2;
/// BSON binary subtype of a UUID (`ui`, `lsid.id`).
const UUID_SUBTYPE: u8 = 4;
/// Bytes of a UUID.
const UUID_LEN: usize = 16;
/// Message of an insert, update, delete or index build on the oplog (code 73,
/// `InvalidNamespace`, on RefDb 9.0.4).
pub const WRITE_REFUSED: &str = "cannot write to local.oplog.rs";
/// Code of `drop` on the oplog (`Location5255000` on RefDb 9.0.4).
pub const DROP_REFUSED_CODE: i32 = 5_255_000;
/// Message of `drop` on the oplog.
pub const DROP_REFUSED: &str = "can't drop live oplog while replicating";

/// `op` of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpType {
    /// `i`: insert.
    Insert,
    /// `u`: update or replacement.
    Update,
    /// `d`: delete.
    Delete,
    /// `c`: command, `applyOps` of transactions among them.
    Command,
    /// `n`: no-op (`appendOplogNote` and periodic no-ops).
    Noop,
}

impl OpType {
    const ALL: [OpType; 5] =
        [OpType::Insert, OpType::Update, OpType::Delete, OpType::Command, OpType::Noop];

    /// The one-letter code of `op`.
    pub fn code(self) -> &'static str {
        match self {
            OpType::Insert => "i",
            OpType::Update => "u",
            OpType::Delete => "d",
            OpType::Command => "c",
            OpType::Noop => "n",
        }
    }

    /// The type with this code.
    pub fn from_code(code: &str) -> Option<OpType> {
        OpType::ALL.into_iter().find(|t| t.code() == code)
    }
}

/// Why a document is not an oplog entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OplogError {
    /// A required field is absent.
    Missing(&'static str),
    /// A field has the wrong type.
    WrongType(&'static str),
    /// `op` is not one of `i`, `u`, `d`, `c` and `n`.
    UnknownOp(String),
}

/// One operation: an entry without its times and session, as it also goes inside `applyOps`.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// `op`.
    pub op: OpType,
    /// `ns`: `db.collection`, `db.$cmd` for commands, empty for no-ops.
    pub ns: String,
    /// `ui`: the collection's UUID.
    pub ui: Option<[u8; UUID_LEN]>,
    /// `o`: the document, the update, the delete's key or the command.
    pub o: Document,
    /// `o2`: the document key of inserts and updates, or a command's extra information.
    pub o2: Option<Document>,
}

impl Operation {
    /// An operation.
    pub fn new(
        op: OpType,
        ns: impl Into<String>,
        ui: Option<[u8; UUID_LEN]>,
        o: Document,
        o2: Option<Document>,
    ) -> Operation {
        Operation { op, ns: ns.into(), ui, o, o2 }
    }

    fn push_into(&self, d: &mut Document) {
        d.push("op", self.op.code());
        d.push("ns", self.ns.as_str());
        if let Some(ui) = self.ui {
            d.push("ui", Value::Binary { subtype: UUID_SUBTYPE, bytes: ui.to_vec() });
        }
        d.push("o", self.o.clone());
        if let Some(o2) = &self.o2 {
            d.push("o2", o2.clone());
        }
    }

    /// `{op, ns, ui, o, o2}`, the form inside `applyOps`.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new();
        self.push_into(&mut d);
        d
    }

    /// Reads `op`, `ns`, `ui`, `o` and `o2` of an entry.
    pub fn from_document(d: &Document) -> Result<Operation, OplogError> {
        let op = match d.get("op") {
            None => return Err(OplogError::Missing("op")),
            Some(Value::String(s)) => {
                OpType::from_code(s).ok_or_else(|| OplogError::UnknownOp(s.clone()))?
            }
            Some(_) => return Err(OplogError::WrongType("op")),
        };
        let ns = match d.get("ns") {
            None => return Err(OplogError::Missing("ns")),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(OplogError::WrongType("ns")),
        };
        let ui = match d.get("ui") {
            None => None,
            Some(Value::Binary { subtype: UUID_SUBTYPE, bytes }) => Some(
                <[u8; UUID_LEN]>::try_from(bytes.as_slice())
                    .map_err(|_| OplogError::WrongType("ui"))?,
            ),
            Some(_) => return Err(OplogError::WrongType("ui")),
        };
        let o = document(d, "o")?.ok_or(OplogError::Missing("o"))?;
        let o2 = document(d, "o2")?;
        Ok(Operation { op, ns, ui, o, o2 })
    }
}

/// The session and transaction number of a retryable write or a transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct TxnInfo {
    /// `lsid`.
    pub lsid: Document,
    /// `txnNumber`.
    pub txn_number: i64,
}

/// An entry of `local.oplog.rs`.
#[derive(Clone, Debug, PartialEq)]
pub struct OplogEntry {
    /// `lsid` and `txnNumber`.
    pub txn: Option<TxnInfo>,
    /// `op`, `ns`, `ui`, `o` and `o2`.
    pub operation: Operation,
    /// `needsRetryImage` of a retryable `findAndModify` (`preImage` or `postImage`).
    pub retry_image: Option<String>,
    /// `stmtId` of a retryable write.
    pub stmt_id: Option<i32>,
    /// Fields this module does not interpret, kept in their order (`versionContext`,
    /// `fromMigrate`…).
    pub extra: Document,
    /// `ts` and `t`.
    pub op_time: Option<OpTime>,
    /// `v`.
    pub version: i64,
    /// `wall`: milliseconds since 1970.
    pub wall_ms: Option<i64>,
    /// `prevOpTime`: the previous entry of the same session.
    pub prev_op_time: Option<OpTime>,
    /// `multiOpType`: 1 on the `applyOps` entry of a retryable write of several documents.
    pub multi_op_type: Option<i32>,
}

/// Names this module reads; anything else goes to [`OplogEntry::extra`].
const KNOWN: [&str; 15] = [
    "lsid",
    "txnNumber",
    "op",
    "ns",
    "ui",
    "o",
    "o2",
    "needsRetryImage",
    "stmtId",
    "ts",
    "t",
    "v",
    "wall",
    "prevOpTime",
    "multiOpType",
];

fn document(d: &Document, field: &'static str) -> Result<Option<Document>, OplogError> {
    match d.get(field) {
        None => Ok(None),
        Some(Value::Document(x)) => Ok(Some(x.clone())),
        Some(_) => Err(OplogError::WrongType(field)),
    }
}

/// An integer field written in 32 or 64 bits.
fn integer(d: &Document, field: &'static str) -> Result<Option<i64>, OplogError> {
    match d.get(field) {
        None => Ok(None),
        Some(Value::Int64(n)) => Ok(Some(*n)),
        Some(Value::Int32(n)) => Ok(Some(i64::from(*n))),
        Some(_) => Err(OplogError::WrongType(field)),
    }
}

fn timestamp(d: &Document) -> Result<Option<Timestamp>, OplogError> {
    match d.get("ts") {
        None => Ok(None),
        Some(Value::Timestamp { time, increment }) => {
            Ok(Some(Timestamp { time: *time, increment: *increment }))
        }
        Some(_) => Err(OplogError::WrongType("ts")),
    }
}

/// `ts` and `t` of `d`: both or neither.
fn op_time(d: &Document) -> Result<Option<OpTime>, OplogError> {
    let Some(ts) = timestamp(d)? else { return Ok(None) };
    let term = integer(d, "t")?.ok_or(OplogError::Missing("t"))?;
    Ok(Some(OpTime { ts, term }))
}

impl OplogEntry {
    /// The entry as RefDb writes it.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new();
        if let Some(t) = &self.txn {
            d.push("lsid", t.lsid.clone());
            d.push("txnNumber", t.txn_number);
        }
        self.operation.push_into(&mut d);
        if let Some(i) = &self.retry_image {
            d.push("needsRetryImage", i.as_str());
        }
        if let Some(s) = self.stmt_id {
            d.push("stmtId", s);
        }
        for (k, v) in self.extra.iter() {
            d.push(k, v.clone());
        }
        if let Some(at) = self.op_time {
            d.push("ts", Value::Timestamp { time: at.ts.time, increment: at.ts.increment });
            d.push("t", at.term);
        }
        d.push("v", self.version);
        if let Some(w) = self.wall_ms {
            d.push("wall", Value::DateTime(w));
        }
        if let Some(p) = self.prev_op_time {
            d.push("prevOpTime", p.to_document());
        }
        if let Some(m) = self.multi_op_type {
            d.push("multiOpType", m);
        }
        d
    }

    /// Reads an entry, as `applyOps` and `refrestore --oplogReplay` send them: `ts`, `t`,
    /// `v` and `wall` may be absent.
    pub fn from_document(d: &Document) -> Result<OplogEntry, OplogError> {
        let operation = Operation::from_document(d)?;
        let lsid = document(d, "lsid")?;
        let txn_number = integer(d, "txnNumber")?;
        let txn = match (lsid, txn_number) {
            (None, None) => None,
            (Some(lsid), Some(txn_number)) => Some(TxnInfo { lsid, txn_number }),
            (None, Some(_)) => return Err(OplogError::Missing("lsid")),
            (Some(_), None) => return Err(OplogError::Missing("txnNumber")),
        };
        let stmt_id = match d.get("stmtId") {
            None => None,
            Some(Value::Int32(n)) => Some(*n),
            Some(_) => return Err(OplogError::WrongType("stmtId")),
        };
        let retry_image = match d.get("needsRetryImage") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(OplogError::WrongType("needsRetryImage")),
        };
        let multi_op_type = match d.get("multiOpType") {
            None => None,
            Some(Value::Int32(n)) => Some(*n),
            Some(_) => return Err(OplogError::WrongType("multiOpType")),
        };
        let wall_ms = match d.get("wall") {
            None => None,
            Some(Value::DateTime(ms)) => Some(*ms),
            Some(_) => return Err(OplogError::WrongType("wall")),
        };
        let prev_op_time = match d.get("prevOpTime") {
            None => None,
            Some(Value::Document(p)) => Some(op_time(p)?.ok_or(OplogError::Missing("ts"))?),
            Some(_) => return Err(OplogError::WrongType("prevOpTime")),
        };
        let extra = d
            .iter()
            .filter(|(k, _)| !KNOWN.contains(k))
            .fold(Document::new(), |e, (k, v)| e.with(k, v.clone()));
        Ok(OplogEntry {
            txn,
            operation,
            retry_image,
            stmt_id,
            extra,
            op_time: op_time(d)?,
            version: integer(d, "v")?.unwrap_or(ENTRY_VERSION),
            wall_ms,
            prev_op_time,
            multi_op_type,
        })
    }
}

/// Encoded size of a document (a document that does not encode counts as too large).
fn size(d: &Document) -> usize {
    d.to_bytes().map_or(usize::MAX, |b| b.len())
}

/// Bytes of `{applyOps: []}`: the document's length and terminator, the array's type byte and
/// name, and the empty array's length and terminator.
const EMPTY_APPLY_OPS: usize = 4 + 1 + "applyOps".len() + 1 + 4 + 1 + 1;

/// Bytes an operation adds as element `index` of the array: its type byte, its decimal index
/// and terminator, and the document.
fn element_size(index: usize, doc: usize) -> usize {
    1 + index.to_string().len() + 1 + doc
}

/// Groups the operations of a transaction as RefDb does: each group's `o` within
/// [`MAX_APPLY_OPS_BYTES`], one operation alone if it does not fit with another. The size of
/// `{applyOps: [...]}` grows by each element's bytes, so nothing is encoded twice.
fn groups(ops: &[Operation]) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut bytes = EMPTY_APPLY_OPS;
    for op in ops {
        let doc = op.to_document();
        let grown = bytes.saturating_add(element_size(current.len(), size(&doc)));
        if !current.is_empty() && grown > MAX_APPLY_OPS_BYTES {
            out.push(std::mem::take(&mut current));
            bytes = EMPTY_APPLY_OPS.saturating_add(element_size(0, size(&doc)));
        } else {
            bytes = grown;
        }
        current.push(Value::Document(doc));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The `applyOps` entries of a committed transaction. `next_op_time` gives each entry its
/// optime in commit order (the first one is the commit's, contract `oplog-format.md`).
pub fn transaction_entries(
    txn: TxnInfo,
    ops: Vec<Operation>,
    wall_ms: i64,
    next_op_time: &mut dyn FnMut() -> OpTime,
) -> Vec<OplogEntry> {
    let count = ops.len() as i64;
    let groups = groups(&ops);
    let last = groups.len().saturating_sub(1);
    let mut prev = NULL_OP_TIME;
    let mut out = Vec::with_capacity(groups.len());
    for (i, group) in groups.into_iter().enumerate() {
        let mut o = Document::new().with("applyOps", group);
        match (i == last, last) {
            (_, 0) => {}
            (false, _) => o.push("partialTxn", true),
            (true, _) => o.push("count", count),
        }
        let at = next_op_time();
        out.push(OplogEntry {
            txn: Some(txn.clone()),
            operation: Operation::new(OpType::Command, APPLY_OPS_NS, None, o, None),
            retry_image: None,
            stmt_id: None,
            extra: Document::new(),
            op_time: Some(at),
            version: ENTRY_VERSION,
            wall_ms: Some(wall_ms),
            prev_op_time: Some(prev),
            multi_op_type: None,
        });
        prev = at;
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// The incremental size is the encoded size of `{applyOps: [...]}`, also past ten and a
    /// hundred elements (indexes of two and three digits).
    #[test]
    fn incremental_size_matches_the_encoding() {
        let mut items = Vec::new();
        let mut bytes = EMPTY_APPLY_OPS;
        assert_eq!(size(&Document::new().with("applyOps", items.clone())), bytes);
        for i in 0..150 {
            let doc = Document::new().with("_id", i).with("s", "x".repeat(i as usize % 7));
            bytes += element_size(items.len(), size(&doc));
            items.push(Value::Document(doc));
            assert_eq!(size(&Document::new().with("applyOps", items.clone())), bytes);
        }
    }
}
