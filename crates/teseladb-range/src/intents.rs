//! Transactions in a range (008 FR-001 to FR-005, contract `txn-protocol.md`, data model §1 and
//! §2): the transaction record kept at `02 · txn_id`, the intents a transaction leaves in the LSM
//! as versions of kind `Intent`, and the rules the leader applies before proposing `PutIntents`,
//! `EndTxn` and `PushTxn`. They are pure: the leader reads the current versions under latches and
//! proposes what these rules give, and every replica applies the same result without reading.

use teseladb_hlc::Hlc;
use teseladb_types::{DecodeError, Decoder, Encoder};

/// First byte of the key space of transaction records (003, `teseladb_keys::space::TXN`).
pub const TXN_SPACE: u8 = 0x02;
/// A pending transaction whose record has no heartbeat for this long is abandoned (FR-002).
pub const ABANDONED_AFTER_MS: u64 = 5_000;
/// The lowest mark.
const ZERO: Hlc = Hlc { physical: 0, logical: 0 };
/// Format of an encoded record, so that a later one can be told apart.
const RECORD_FORMAT: u8 = 1;

/// A transaction: 16 random bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId(pub u128);

/// The key of the record of `txn`: `02 · txn_id` (FR-001).
pub fn record_key(txn: TxnId) -> Vec<u8> {
    let mut k = Vec::with_capacity(17);
    k.push(TXN_SPACE);
    k.extend_from_slice(&txn.0.to_be_bytes());
    k
}

/// The state of a transaction record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// Running: its intents may still commit.
    Pending,
    /// Committed at its `commit_ts`; final.
    Committed,
    /// Aborted; final.
    Aborted,
}

/// Code of [`TxnStatus::Pending`].
const PENDING: u8 = 0;
/// Code of [`TxnStatus::Committed`].
const COMMITTED: u8 = 1;
/// Code of [`TxnStatus::Aborted`].
const ABORTED: u8 = 2;

impl TxnStatus {
    /// Its code in a record or a command.
    pub fn code(self) -> u8 {
        match self {
            TxnStatus::Pending => PENDING,
            TxnStatus::Committed => COMMITTED,
            TxnStatus::Aborted => ABORTED,
        }
    }

    /// The status of `code`, if it is one.
    pub fn from_code(code: u8) -> Option<TxnStatus> {
        match code {
            PENDING => Some(TxnStatus::Pending),
            COMMITTED => Some(TxnStatus::Committed),
            ABORTED => Some(TxnStatus::Aborted),
            _ => None,
        }
    }

    /// Whether the status is final.
    pub fn is_final(self) -> bool {
        self != TxnStatus::Pending
    }
}

/// A transaction record (data model §1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnRecord {
    /// The transaction.
    pub txn: TxnId,
    /// RefDb's logical session id (0 for an internal transaction).
    pub lsid: u128,
    /// RefDb's `txnNumber`.
    pub txn_number: i64,
    /// Its state.
    pub status: TxnStatus,
    /// The mark its reads are at.
    pub read_ts: Hlc,
    /// The lowest mark it may commit at, which readers push up (FR-003).
    pub min_commit_ts: Hlc,
    /// The commit mark, once committed.
    pub commit_ts: Hlc,
    /// The mark of the last heartbeat (FR-007).
    pub heartbeat_ts: Hlc,
    /// Intervals of keys with its intents, `[start, end)`.
    pub spans: Vec<(Vec<u8>, Vec<u8>)>,
}

fn put_hlc(e: &mut Encoder, ts: Hlc) {
    e.u64(ts.physical).u32(ts.logical);
}

fn get_hlc(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

impl TxnRecord {
    /// Encodes the record as stored at its key and carried by `EndTxn` and `PushTxn`.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        self.encode_into(&mut e);
        e.finish()
    }

    pub(crate) fn encode_into(&self, e: &mut Encoder) {
        e.u8(RECORD_FORMAT).u128(self.txn.0).u128(self.lsid).i64(self.txn_number);
        e.u8(self.status.code());
        for ts in [self.read_ts, self.min_commit_ts, self.commit_ts, self.heartbeat_ts] {
            put_hlc(e, ts);
        }
        e.var(self.spans.len() as u64);
        for (start, end) in &self.spans {
            e.bytes(start).bytes(end);
        }
    }

    /// Decodes a record from a row of a scan, whose value lacks the first byte of the stored
    /// record: the format, which the row takes for the value's type.
    pub fn decode_row(value: &[u8]) -> Result<TxnRecord, DecodeError> {
        TxnRecord::decode(&[&[RECORD_FORMAT][..], value].concat())
    }

    /// Decodes a record written by [`TxnRecord::encode`].
    pub fn decode(stored: &[u8]) -> Result<TxnRecord, DecodeError> {
        let mut d = Decoder::new(stored);
        let r = TxnRecord::decode_from(&mut d)?;
        d.finish()?;
        Ok(r)
    }

    pub(crate) fn decode_from(d: &mut Decoder<'_>) -> Result<TxnRecord, DecodeError> {
        let at = d.position();
        if d.u8()? != RECORD_FORMAT {
            return Err(DecodeError::InvalidValue { offset: at });
        }
        let (txn, lsid, txn_number) = (TxnId(d.u128()?), d.u128()?, d.i64()?);
        let at = d.position();
        let status =
            TxnStatus::from_code(d.u8()?).ok_or(DecodeError::InvalidValue { offset: at })?;
        let (read_ts, min_commit_ts) = (get_hlc(d)?, get_hlc(d)?);
        let (commit_ts, heartbeat_ts) = (get_hlc(d)?, get_hlc(d)?);
        let n = d.var()?;
        let mut spans = Vec::new();
        for _ in 0..n {
            spans.push((d.bytes()?.to_vec(), d.bytes()?.to_vec()));
        }
        Ok(TxnRecord {
            txn,
            lsid,
            txn_number,
            status,
            read_ts,
            min_commit_ts,
            commit_ts,
            heartbeat_ts,
            spans,
        })
    }

    /// Whether a pending record had no heartbeat for [`ABANDONED_AFTER_MS`] at `now`.
    pub fn abandoned_at(&self, now: Hlc) -> bool {
        self.status == TxnStatus::Pending
            && self.heartbeat_ts.physical.saturating_add(ABANDONED_AFTER_MS) < now.physical
    }
}

/// What an intent will write once its transaction commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentOp {
    /// A version with its value type and value.
    Put {
        /// Type of the value.
        value_type: u8,
        /// The value.
        value: Vec<u8>,
    },
    /// A deletion.
    Delete,
}

/// Code of [`IntentOp::Put`].
const OP_PUT: u8 = 1;
/// Code of [`IntentOp::Delete`].
const OP_DELETE: u8 = 2;

pub(crate) fn put_op(e: &mut Encoder, op: &IntentOp) {
    match op {
        IntentOp::Put { value_type, value } => {
            e.u8(OP_PUT).u8(*value_type).bytes(value);
        }
        IntentOp::Delete => {
            e.u8(OP_DELETE);
        }
    }
}

pub(crate) fn get_op(d: &mut Decoder<'_>) -> Result<IntentOp, DecodeError> {
    let at = d.position();
    match d.u8()? {
        OP_PUT => Ok(IntentOp::Put { value_type: d.u8()?, value: d.bytes()?.to_vec() }),
        OP_DELETE => Ok(IntentOp::Delete),
        _ => Err(DecodeError::InvalidValue { offset: at }),
    }
}

/// An intent as the LSM keeps it (data model §2): `txn_id · record_key · real kind · value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// Its transaction.
    pub txn: TxnId,
    /// The key of the transaction's record, which a reader pushes.
    pub record_key: Vec<u8>,
    /// What it writes.
    pub op: IntentOp,
}

impl Intent {
    /// The payload of the LSM version.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u128(self.txn.0).bytes(&self.record_key);
        put_op(&mut e, &self.op);
        e.finish()
    }

    /// Decodes a payload written by [`Intent::encode`].
    pub fn decode(payload: &[u8]) -> Result<Intent, DecodeError> {
        let mut d = Decoder::new(payload);
        let (txn, record_key) = (TxnId(d.u128()?), d.bytes()?.to_vec());
        let op = get_op(&mut d)?;
        d.finish()?;
        Ok(Intent { txn, record_key, op })
    }
}

/// The transaction that holds an intent another write ran into: its record, to push, and the
/// intent's mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentOwner {
    /// The transaction.
    pub txn: TxnId,
    /// Key of its record.
    pub record_key: Vec<u8>,
    /// The key that holds the intent: the one to resolve once the owner is finished, which is not
    /// the written key when the write read it (a unique index's entry).
    pub key: Vec<u8>,
    /// The intent's mark.
    pub ts: Hlc,
}

/// The newest version of a key as the leader read it: its mark, whether it is an intent, and its
/// stored value (an intent's payload for an intent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Latest {
    /// Its mark.
    pub ts: Hlc,
    /// Whether it is an intent.
    pub intent: bool,
    /// The stored value.
    pub value: Vec<u8>,
}

/// Why `PutIntents` cannot write a key (FR-002).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// A version newer than the transaction's read mark: RefDb's `WriteConflict` (112).
    Newer(Hlc),
    /// An intent of another transaction: the caller gets `WriteConflict`, or pushes it if it is
    /// abandoned and tries again.
    Intent(IntentOwner),
    /// The stored intent does not decode: the range is corrupt.
    Corrupt,
}

/// Checks `key`, which `txn`, reading at `read_ts`, wants to write: `Ok` with the mark of its own
/// intent on the key, which the new one replaces, or the conflict (contract `txn-protocol.md`).
pub fn check_put(
    key: &[u8],
    latest: Option<&Latest>,
    txn: TxnId,
    read_ts: Hlc,
) -> Result<Option<Hlc>, Conflict> {
    let Some(l) = latest else { return Ok(None) };
    if !l.intent {
        return if l.ts > read_ts { Err(Conflict::Newer(l.ts)) } else { Ok(None) };
    }
    let intent = Intent::decode(&l.value).map_err(|_| Conflict::Corrupt)?;
    if intent.txn == txn {
        return Ok(Some(l.ts));
    }
    let (txn, record_key, key) = (intent.txn, intent.record_key, key.to_vec());
    Err(Conflict::Intent(IntentOwner { txn, record_key, key, ts: l.ts }))
}

/// What the coordinator asks of its record with `EndTxn` (FR-001, FR-004).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOp {
    /// Creates the record `Pending`, or adopts the one a push created.
    Begin {
        /// RefDb's session.
        lsid: u128,
        /// RefDb's `txnNumber`.
        txn_number: i64,
        /// The transaction's read mark.
        read_ts: Hlc,
    },
    /// A heartbeat, with the intervals written so far.
    Heartbeat {
        /// Intervals with intents.
        spans: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// Commits at no less than `ts` (the coordinator's mark and the highest mark of its intents).
    Commit {
        /// The proposed commit mark.
        ts: Hlc,
        /// Intervals with intents.
        spans: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// Aborts.
    Abort {
        /// Intervals with intents.
        spans: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// Why `EndTxn` was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The record is aborted (by a push, the reaper or the coordinator), or it never existed when
    /// the coordinator commits: RefDb's `NoSuchTransaction` (251).
    NoSuchTransaction,
    /// The stored record does not decode: the range is corrupt.
    Corrupt,
}

fn decode_current(stored: Option<&[u8]>) -> Result<Option<TxnRecord>, RecordError> {
    stored.map(TxnRecord::decode).transpose().map_err(|_| RecordError::Corrupt)
}

fn fresh(txn: TxnId, status: TxnStatus, at: Hlc) -> TxnRecord {
    TxnRecord {
        txn,
        lsid: 0,
        txn_number: 0,
        status,
        read_ts: ZERO,
        min_commit_ts: ZERO,
        commit_ts: ZERO,
        heartbeat_ts: at,
        spans: Vec::new(),
    }
}

/// Evaluates `EndTxn(op)` on the record of `txn` (`stored`: its current value) at mark `at`: the
/// record to write and to answer with (contract `txn-protocol.md`, «Transiciones del registro»).
pub fn end_txn(
    stored: Option<&[u8]>,
    txn: TxnId,
    op: RecordOp,
    at: Hlc,
) -> Result<TxnRecord, RecordError> {
    let current = decode_current(stored)?;
    match (current, op) {
        (None, RecordOp::Begin { lsid, txn_number, read_ts }) => {
            Ok(TxnRecord { lsid, txn_number, read_ts, ..fresh(txn, TxnStatus::Pending, at) })
        }
        // A push created it first: the coordinator adopts its `min_commit_ts`.
        (Some(r), RecordOp::Begin { lsid, txn_number, read_ts })
            if r.status == TxnStatus::Pending =>
        {
            Ok(TxnRecord { lsid, txn_number, read_ts, heartbeat_ts: at, ..r })
        }
        (None, RecordOp::Heartbeat { .. } | RecordOp::Commit { .. }) => {
            Err(RecordError::NoSuchTransaction)
        }
        (None, RecordOp::Abort { spans }) => {
            Ok(TxnRecord { spans, ..fresh(txn, TxnStatus::Aborted, at) })
        }
        (Some(r), _) if r.status == TxnStatus::Aborted => Err(RecordError::NoSuchTransaction),
        // A retried commit, or an abort that arrives after the commit: the record says what
        // happened.
        (Some(r), _) if r.status == TxnStatus::Committed => Ok(r),
        (Some(r), RecordOp::Begin { .. }) => Ok(r),
        (Some(r), RecordOp::Heartbeat { spans }) => Ok(TxnRecord { heartbeat_ts: at, spans, ..r }),
        (Some(r), RecordOp::Commit { ts, spans }) => {
            #[cfg(not(teseladb_bug = "commit_below_push"))]
            let commit_ts = ts.max(r.min_commit_ts);
            // Faulty variant (008 T024): the commit takes the coordinator's mark and ignores the
            // pushes, so a version appears below a read that was served without it.
            #[cfg(teseladb_bug = "commit_below_push")]
            let commit_ts = ts;
            Ok(TxnRecord { status: TxnStatus::Committed, commit_ts, spans, ..r })
        }
        (Some(r), RecordOp::Abort { spans }) => {
            Ok(TxnRecord { status: TxnStatus::Aborted, spans, ..r })
        }
    }
}

/// What a `PushTxn` asks (FR-002, FR-003).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// A reader at a mark below `to`: the transaction may only commit at `to` or above.
    To(Hlc),
    /// A writer that found its intent, written at `intent_ts`: abort it if it has no heartbeat
    /// for [`ABANDONED_AFTER_MS`] at the push's mark. A record that does not exist yet counts
    /// from the intent: past the threshold it is created aborted, before it it is created pending
    /// with the intent's mark as its heartbeat, for its coordinator to adopt.
    AbortIfAbandoned {
        /// Mark of the intent the writer found.
        intent_ts: Hlc,
    },
}

/// Evaluates `PushTxn(push)` on the record of `txn` at mark `at`: the record to write and to
/// answer with.
pub fn push_txn(
    stored: Option<&[u8]>,
    txn: TxnId,
    push: Push,
    at: Hlc,
) -> Result<TxnRecord, RecordError> {
    let current = decode_current(stored)?;
    Ok(match (current, push) {
        // The coordinator that creates it later adopts the push.
        (None, Push::To(to)) => {
            TxnRecord { min_commit_ts: to, ..fresh(txn, TxnStatus::Pending, at) }
        }
        (None, Push::AbortIfAbandoned { intent_ts }) => {
            let pending = fresh(txn, TxnStatus::Pending, intent_ts);
            if pending.abandoned_at(at) {
                TxnRecord { status: TxnStatus::Aborted, ..pending }
            } else {
                pending
            }
        }
        (Some(r), _) if r.status.is_final() => r,
        (Some(r), Push::To(to)) => TxnRecord { min_commit_ts: to.max(r.min_commit_ts), ..r },
        (Some(r), Push::AbortIfAbandoned { .. }) if r.abandoned_at(at) => {
            TxnRecord { status: TxnStatus::Aborted, ..r }
        }
        (Some(r), Push::AbortIfAbandoned { .. }) => r,
    })
}

/// The mark just above `t`: what a reader at `t` pushes a pending transaction to (005 FR-025).
pub fn succ(t: Hlc) -> Hlc {
    match t.logical.checked_add(1) {
        Some(logical) => Hlc { physical: t.physical, logical },
        None => Hlc { physical: t.physical + 1, logical: 0 },
    }
}

/// The mark just below `t`: a read there sees the versions under an intent at `t`.
pub fn pred(t: Hlc) -> Hlc {
    match t.logical.checked_sub(1) {
        Some(logical) => Hlc { physical: t.physical, logical },
        None => Hlc { physical: t.physical.saturating_sub(1), logical: u32::MAX },
    }
}
