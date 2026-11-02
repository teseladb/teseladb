//! Commands of a range (FR-001, contract `range-commands.md`, data model §2): the payload of a
//! multiraft envelope, which already carries the entry's mark (a `Write`'s `ts`), is
//! `kind(u16) · body` written with
//! the binary codec of `teseladb-types`. Unknown kinds do not decode: the state machine stops the
//! core rather than skip an entry.

use teseladb_hlc::Hlc;
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId};

use crate::intents::{IntentOp, TxnId, TxnRecord, TxnStatus, get_op, put_op};

/// `kind` of [`Command::Noop`].
pub const KIND_NOOP: u16 = 0;
/// `kind` of [`Command::Write`].
pub const KIND_WRITE: u16 = 1;
/// `kind` of [`Command::SetGcThreshold`].
pub const KIND_SET_GC_THRESHOLD: u16 = 2;
/// `kind` of [`Command::ComputeChecksum`].
pub const KIND_COMPUTE_CHECKSUM: u16 = 3;
/// `kind` of [`Command::Split`].
pub const KIND_SPLIT: u16 = 4;
/// `kind` of [`Command::SplitGenesis`].
pub const KIND_SPLIT_GENESIS: u16 = 5;
/// `kind` of [`Command::MergeFreeze`].
pub const KIND_MERGE_FREEZE: u16 = 6;
/// `kind` of [`Command::Merge`].
pub const KIND_MERGE: u16 = 7;
/// `kind` of [`Command::PutIntents`] (008).
pub const KIND_PUT_INTENTS: u16 = 8;
/// `kind` of [`Command::EndTxn`] (008).
pub const KIND_END_TXN: u16 = 9;
/// `kind` of [`Command::ResolveIntents`] (008).
pub const KIND_RESOLVE_INTENTS: u16 = 10;
/// `kind` of [`Command::SetClosedTs`] (007).
pub const KIND_SET_CLOSED_TS: u16 = 11;
/// `kind` of [`Command::PushTxn`] (008).
pub const KIND_PUSH_TXN: u16 = 12;
/// `kind` of [`Command::PurgeDedup`].
pub const KIND_PURGE_DEDUP: u16 = 13;
/// `kind` of [`Command::Relocate`] (007).
pub const KIND_RELOCATE: u16 = 14;
/// `kind` of [`Command::ForgetRecord`] (008).
pub const KIND_FORGET_RECORD: u16 = 15;
/// `kind` of [`Command::Fence`] (018).
pub const KIND_FENCE: u16 = 16;

/// The statement a retryable write belongs to (FR-011): the driver sends it again with the same
/// key after a lost answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DedupKey {
    /// Logical session id.
    pub lsid: u128,
    /// Transaction number within the session.
    pub txn_number: i64,
    /// Statement within the transaction.
    pub stmt_id: u32,
}

/// A version written by a [`Command::Write`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Put {
    /// Logical key.
    pub key: Vec<u8>,
    /// Type of the value (document, index entry).
    pub value_type: u8,
    /// Value.
    pub value: Vec<u8>,
}

/// Idempotence record of a retryable write (FR-011, data model §3): its statement and the result
/// the mutation gave, written at the write's mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupRecord {
    /// Logical session id.
    pub lsid: u128,
    /// Transaction number within the session.
    pub txn_number: i64,
    /// Statement within the transaction.
    pub stmt_id: u32,
    /// Encoded result the driver expects back.
    pub result: Vec<u8>,
}

/// An idempotence record copied by a split (007 FR-022), with the mark of the write that left it,
/// which a retry gets back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopiedRecord {
    /// Its statement.
    pub key: DedupKey,
    /// Mark of the original write.
    pub mark: Hlc,
    /// The mutation's result.
    pub result: Vec<u8>,
}

/// The state of the right range a [`Command::Merge`] takes (007 FR-009, FR-022): its end, GC
/// threshold, last mark, live bytes and idempotence table as it froze them. Its data is already
/// in the core's LSM, where its replica on the same node left it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Absorbed {
    /// First key past the right range.
    pub end: Vec<u8>,
    /// Its GC threshold.
    pub gc: Hlc,
    /// Its last mark.
    pub mark: Hlc,
    /// Its live bytes.
    pub live: u64,
    /// Its idempotence records.
    pub records: Vec<CopiedRecord>,
    /// Its write fences (018, [`crate::fence`]).
    pub fences: Vec<crate::fence::Span>,
}

/// An intent written by a [`Command::PutIntents`] at the entry's mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentPut {
    /// Logical key.
    pub key: Vec<u8>,
    /// What it writes once committed.
    pub op: IntentOp,
    /// The mark of the transaction's own earlier intent on the key, which it replaces.
    pub replaces: Option<Hlc>,
}

/// An intent a [`Command::ResolveIntents`] resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// Logical key.
    pub key: Vec<u8>,
    /// The intent's mark.
    pub intent_ts: Hlc,
    /// What it writes at the commit mark; `None` drops it (an aborted transaction).
    pub commit: Option<IntentOp>,
}

/// A command of a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Nothing; a new leader proposes it.
    Noop,
    /// Versions at the entry's mark, deletions and the idempotence record.
    Write {
        /// Versions written.
        puts: Vec<Put>,
        /// Logical keys deleted.
        deletes: Vec<Vec<u8>>,
        /// Idempotence record, if the write is retryable.
        dedup: Option<DedupRecord>,
        /// Change of the range's live bytes (007 FR-005, D-068): the new versions' sizes minus the
        /// sizes of the versions they replace or delete, which the leader read under latches.
        live_delta: i64,
    },
    /// Raises the GC threshold if it is higher.
    SetGcThreshold {
        /// New threshold.
        ts: Hlc,
    },
    /// The leader's closed mark (007 FR-012): no write at or below it comes after this entry, so a
    /// replica that applied it reads at or below the mark without the leader.
    SetClosedTs {
        /// The closed mark.
        closed: Hlc,
    },
    /// Computes the checksum of the range at `at` and sends it to the leader.
    ComputeChecksum {
        /// Mark the checksum is computed at.
        at: Hlc,
    },
    /// Deletes the record of a finished transaction no one needs any more (008 record GC).
    ForgetRecord {
        /// The transaction.
        txn: crate::intents::TxnId,
    },
    /// Deletes idempotence records the leader found expired (FR-011).
    PurgeDedup {
        /// Local keys of the records.
        keys: Vec<Vec<u8>>,
    },
    /// Moves the range to core `core` of every node (007 FR-011): a replica that applies it stops
    /// on its core and hands the range over to that core of its node. `intent` is the Group 0
    /// index of the `RelocateRange` that asked for it.
    Relocate {
        /// The core the range moves to.
        core: u16,
        /// The intent.
        intent: u64,
    },
    /// Cuts the range at `at` (007 FR-007): from this entry it holds `[start, at)` and
    /// `new_range` starts over `[at, end)` with its GC threshold, last mark and `records`, the
    /// idempotence table the leader read once no write on `[at, end)` was pending.
    Split {
        /// First key of the new range.
        at: Vec<u8>,
        /// The new range.
        new_range: GroupId,
        /// The idempotence records it copies.
        records: Vec<CopiedRecord>,
        /// Live bytes of `[at, end)`, which the leader counted before proposing (D-068).
        right_live: u64,
    },
    /// Freezes the range for merge `merge` (007 FR-009: the Group 0 index of the intent): from
    /// this entry it serves nothing and writes nothing. `merge` 0 thaws it after an aborted merge.
    MergeFreeze {
        /// The merge, or 0.
        merge: u64,
    },
    /// Ends merge `merge` of range `right` into this one: `absorb` carries the right range's
    /// frozen state and from this entry the range holds its keys too; `None` gives the merge up,
    /// and no later `Merge` of it applies (Group 0 then aborts it and the right range thaws).
    Merge {
        /// The right range.
        right: GroupId,
        /// The merge.
        merge: u64,
        /// Its frozen state, or `None` to give the merge up.
        absorb: Option<Absorbed>,
    },
    /// Intents of transaction `txn` (008 FR-002), at the entry's mark: the leader checked them
    /// against the newer versions and the other transactions' intents.
    PutIntents {
        /// The transaction.
        txn: TxnId,
        /// Key of its record.
        record_key: Vec<u8>,
        /// The intents.
        intents: Vec<IntentPut>,
    },
    /// Writes the record of a transaction as the leader evaluated it (008 FR-001, FR-004).
    EndTxn {
        /// The record after the command.
        record: TxnRecord,
    },
    /// Turns intents of a finished transaction into versions at its commit mark or drops them
    /// (008 FR-005).
    ResolveIntents {
        /// The transaction.
        txn: TxnId,
        /// Its final state.
        status: TxnStatus,
        /// Its commit mark.
        commit_ts: Hlc,
        /// The intents.
        resolutions: Vec<Resolution>,
        /// Change of the range's live bytes, which the leader read under latches (D-068).
        live_delta: i64,
    },
    /// Writes the record a push left (008 FR-002, FR-003).
    PushTxn {
        /// The record after the push.
        record: TxnRecord,
    },
    /// Adds (`on`) or lifts the write fence of `[start, end)` (018 FR-002, [`crate::fence`]):
    /// from this entry on, a `Write` or `PutIntents` with a key in it is refused.
    Fence {
        /// First key of the span.
        start: Vec<u8>,
        /// End of the span (excluded).
        end: Vec<u8>,
        /// Whether the fence goes up.
        on: bool,
    },
    /// First entry of a range born from a split: its parent, the entry that cut it and its end,
    /// so that a replica that replays the range's log from its first entry knows its bounds.
    SplitGenesis {
        /// The range it was cut from.
        parent: GroupId,
        /// Index of the parent's `Split`.
        at_index: u64,
        /// First key past the range.
        end: Vec<u8>,
    },
}

fn put_record(e: &mut Encoder, r: &DedupRecord) {
    e.u128(r.lsid).i64(r.txn_number).u32(r.stmt_id).bytes(&r.result);
}

fn get_record(d: &mut Decoder<'_>) -> Result<DedupRecord, DecodeError> {
    Ok(DedupRecord {
        lsid: d.u128()?,
        txn_number: d.i64()?,
        stmt_id: d.u32()?,
        result: d.bytes()?.to_vec(),
    })
}

fn put_hlc(e: &mut Encoder, ts: Hlc) {
    e.u64(ts.physical).u32(ts.logical);
}

fn get_hlc(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

fn put_copied(e: &mut Encoder, records: &[CopiedRecord]) {
    e.var(records.len() as u64);
    for r in records {
        e.u128(r.key.lsid).i64(r.key.txn_number).u32(r.key.stmt_id);
        put_hlc(e, r.mark);
        e.bytes(&r.result);
    }
}

fn get_copied(d: &mut Decoder<'_>) -> Result<Vec<CopiedRecord>, DecodeError> {
    let n = d.var()?;
    let mut records = Vec::new();
    for _ in 0..n {
        let key = DedupKey { lsid: d.u128()?, txn_number: d.i64()?, stmt_id: d.u32()? };
        let mark = get_hlc(d)?;
        records.push(CopiedRecord { key, mark, result: d.bytes()?.to_vec() });
    }
    Ok(records)
}

impl Command {
    /// Its `kind`.
    pub fn kind(&self) -> u16 {
        match self {
            Command::Noop => KIND_NOOP,
            Command::Write { .. } => KIND_WRITE,
            Command::SetGcThreshold { .. } => KIND_SET_GC_THRESHOLD,
            Command::SetClosedTs { .. } => KIND_SET_CLOSED_TS,
            Command::ComputeChecksum { .. } => KIND_COMPUTE_CHECKSUM,
            Command::PurgeDedup { .. } => KIND_PURGE_DEDUP,
            Command::ForgetRecord { .. } => KIND_FORGET_RECORD,
            Command::Relocate { .. } => KIND_RELOCATE,
            Command::Split { .. } => KIND_SPLIT,
            Command::SplitGenesis { .. } => KIND_SPLIT_GENESIS,
            Command::MergeFreeze { .. } => KIND_MERGE_FREEZE,
            Command::Merge { .. } => KIND_MERGE,
            Command::PutIntents { .. } => KIND_PUT_INTENTS,
            Command::EndTxn { .. } => KIND_END_TXN,
            Command::ResolveIntents { .. } => KIND_RESOLVE_INTENTS,
            Command::PushTxn { .. } => KIND_PUSH_TXN,
            Command::Fence { .. } => KIND_FENCE,
        }
    }

    /// Encodes the command as an envelope payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u16(self.kind());
        match self {
            Command::Noop => {}
            Command::Write { puts, deletes, dedup, live_delta } => {
                e.var(puts.len() as u64);
                for p in puts {
                    e.bytes(&p.key).u8(p.value_type).bytes(&p.value);
                }
                e.var(deletes.len() as u64);
                for k in deletes {
                    e.bytes(k);
                }
                e.bool(dedup.is_some());
                if let Some(r) = dedup {
                    put_record(&mut e, r);
                }
                e.var(zigzag(*live_delta));
            }
            Command::SetGcThreshold { ts } => put_hlc(&mut e, *ts),
            Command::SetClosedTs { closed } => put_hlc(&mut e, *closed),
            Command::ComputeChecksum { at } => put_hlc(&mut e, *at),
            Command::PurgeDedup { keys } => {
                e.var(keys.len() as u64);
                for k in keys {
                    e.bytes(k);
                }
            }
            Command::Relocate { core, intent } => {
                e.u16(*core).u64(*intent);
            }
            Command::ForgetRecord { txn } => {
                e.u128(txn.0);
            }
            Command::Split { at, new_range, records, right_live } => {
                e.bytes(at).u64(new_range.0);
                put_copied(&mut e, records);
                e.u64(*right_live);
            }
            Command::MergeFreeze { merge } => {
                e.u64(*merge);
            }
            Command::Merge { right, merge, absorb } => {
                e.u64(right.0).u64(*merge).bool(absorb.is_some());
                if let Some(a) = absorb {
                    e.bytes(&a.end);
                    put_hlc(&mut e, a.gc);
                    put_hlc(&mut e, a.mark);
                    e.u64(a.live);
                    put_copied(&mut e, &a.records);
                    crate::fence::put(&mut e, &a.fences);
                }
            }
            Command::Fence { start, end, on } => {
                e.bytes(start).bytes(end).bool(*on);
            }
            Command::SplitGenesis { parent, at_index, end } => {
                e.u64(parent.0).u64(*at_index).bytes(end);
            }
            Command::PutIntents { txn, record_key, intents } => {
                e.u128(txn.0).bytes(record_key).var(intents.len() as u64);
                for i in intents {
                    e.bytes(&i.key);
                    put_op(&mut e, &i.op);
                    e.bool(i.replaces.is_some());
                    if let Some(ts) = i.replaces {
                        put_hlc(&mut e, ts);
                    }
                }
            }
            Command::EndTxn { record } | Command::PushTxn { record } => record.encode_into(&mut e),
            Command::ResolveIntents { txn, status, commit_ts, resolutions, live_delta } => {
                e.u128(txn.0).u8(status.code());
                put_hlc(&mut e, *commit_ts);
                e.var(resolutions.len() as u64);
                for r in resolutions {
                    e.bytes(&r.key);
                    put_hlc(&mut e, r.intent_ts);
                    e.bool(r.commit.is_some());
                    if let Some(op) = &r.commit {
                        put_op(&mut e, op);
                    }
                }
                e.var(zigzag(*live_delta));
            }
        }
        e.finish()
    }

    /// Decodes a payload written by [`Command::encode`]; an unknown `kind` or trailing bytes are
    /// an error.
    pub fn decode(payload: &[u8]) -> Result<Command, DecodeError> {
        let mut d = Decoder::new(payload);
        let at = d.position();
        let command = match d.u16()? {
            KIND_NOOP => Command::Noop,
            KIND_WRITE => {
                let n = d.var()?;
                let mut puts = Vec::new();
                for _ in 0..n {
                    let key = d.bytes()?.to_vec();
                    let value_type = d.u8()?;
                    let value = d.bytes()?.to_vec();
                    puts.push(Put { key, value_type, value });
                }
                let n = d.var()?;
                let mut deletes = Vec::new();
                for _ in 0..n {
                    deletes.push(d.bytes()?.to_vec());
                }
                let dedup = if d.bool()? { Some(get_record(&mut d)?) } else { None };
                Command::Write { puts, deletes, dedup, live_delta: unzigzag(d.var()?) }
            }
            KIND_SET_GC_THRESHOLD => Command::SetGcThreshold { ts: get_hlc(&mut d)? },
            KIND_SET_CLOSED_TS => Command::SetClosedTs { closed: get_hlc(&mut d)? },
            KIND_COMPUTE_CHECKSUM => Command::ComputeChecksum { at: get_hlc(&mut d)? },
            KIND_PURGE_DEDUP => {
                let n = d.var()?;
                let mut keys = Vec::new();
                for _ in 0..n {
                    keys.push(d.bytes()?.to_vec());
                }
                Command::PurgeDedup { keys }
            }
            KIND_RELOCATE => Command::Relocate { core: d.u16()?, intent: d.u64()? },
            KIND_FORGET_RECORD => Command::ForgetRecord { txn: crate::intents::TxnId(d.u128()?) },
            KIND_SPLIT => {
                let at = d.bytes()?.to_vec();
                let new_range = GroupId(d.u64()?);
                let records = get_copied(&mut d)?;
                Command::Split { at, new_range, records, right_live: d.u64()? }
            }
            KIND_SPLIT_GENESIS => {
                let (parent, at_index) = (GroupId(d.u64()?), d.u64()?);
                Command::SplitGenesis { parent, at_index, end: d.bytes()?.to_vec() }
            }
            KIND_MERGE_FREEZE => Command::MergeFreeze { merge: d.u64()? },
            KIND_FENCE => {
                let (start, end) = (d.bytes()?.to_vec(), d.bytes()?.to_vec());
                Command::Fence { start, end, on: d.bool()? }
            }
            KIND_MERGE => {
                let (right, merge) = (GroupId(d.u64()?), d.u64()?);
                let absorb = if d.bool()? {
                    let end = d.bytes()?.to_vec();
                    let (gc, mark, live) = (get_hlc(&mut d)?, get_hlc(&mut d)?, d.u64()?);
                    let records = get_copied(&mut d)?;
                    let fences = crate::fence::get(&mut d)?;
                    Some(Absorbed { end, gc, mark, live, records, fences })
                } else {
                    None
                };
                Command::Merge { right, merge, absorb }
            }
            KIND_PUT_INTENTS => {
                let (txn, record_key) = (TxnId(d.u128()?), d.bytes()?.to_vec());
                let n = d.var()?;
                let mut intents = Vec::new();
                for _ in 0..n {
                    let key = d.bytes()?.to_vec();
                    let op = get_op(&mut d)?;
                    let replaces = if d.bool()? { Some(get_hlc(&mut d)?) } else { None };
                    intents.push(IntentPut { key, op, replaces });
                }
                Command::PutIntents { txn, record_key, intents }
            }
            KIND_END_TXN => Command::EndTxn { record: TxnRecord::decode_from(&mut d)? },
            KIND_PUSH_TXN => Command::PushTxn { record: TxnRecord::decode_from(&mut d)? },
            KIND_RESOLVE_INTENTS => {
                let txn = TxnId(d.u128()?);
                let offset = d.position();
                let status =
                    TxnStatus::from_code(d.u8()?).ok_or(DecodeError::InvalidValue { offset })?;
                let commit_ts = get_hlc(&mut d)?;
                let n = d.var()?;
                let mut resolutions = Vec::new();
                for _ in 0..n {
                    let (key, intent_ts) = (d.bytes()?.to_vec(), get_hlc(&mut d)?);
                    let commit = if d.bool()? { Some(get_op(&mut d)?) } else { None };
                    resolutions.push(Resolution { key, intent_ts, commit });
                }
                let live_delta = unzigzag(d.var()?);
                Command::ResolveIntents { txn, status, commit_ts, resolutions, live_delta }
            }
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        d.finish()?;
        Ok(command)
    }
}

/// A signed number as the unsigned one LEB128 writes short when it is near zero (zigzag:
/// 0, −1, 1, −2… become 0, 1, 2, 3…).
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)).cast_unsigned()
}

/// The inverse of [`zigzag`].
fn unzigzag(z: u64) -> i64 {
    (z >> 1).cast_signed() ^ -((z & 1).cast_signed())
}
