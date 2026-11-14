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
/// The format of the range commands this binary reads and writes: 1 adds
/// [`KIND_WRITE_COUNTED`] (007 FR-039), 2 [`KIND_RESOLVE_INTENTS_COUNTED`], 3
/// [`KIND_WRITE_REACHING`], [`KIND_SPLIT_REACHING`] and [`KIND_MERGE_REACHING`], 4 the writes
/// that number their new documents ([`FORMAT_LOCAL_IDS`]), 5 [`KIND_WRITE_SIZED`],
/// [`KIND_RESOLVE_INTENTS_SIZED`] and [`KIND_ADJUST_SIZED`]. A node records it in Group 0 (FR-027),
/// and a leader writes a command of a format only once every node that is not removed records it.
pub const FORMAT: u8 = 5;
/// The format whose writes put the range's next local identifier (009 FR-023,
/// [`crate::sm::next_local_key`]), a key a node of the version before refuses as outside the
/// range.
pub const FORMAT_LOCAL_IDS: u8 = 4;
/// The format that brings [`KIND_WRITE_COUNTED`].
pub const FORMAT_COUNTED: u8 = 1;
/// `kind` of a [`Command::Write`] that carries the change of each collection's live documents
/// (007 FR-039): a format a node of the version before does not read, so a leader writes it only
/// once every node says it reads it.
pub const KIND_WRITE_COUNTED: u16 = 17;
/// `kind` of [`Command::AdjustCounts`] (007 FR-039), of the same format as
/// [`KIND_WRITE_COUNTED`].
pub const KIND_ADJUST_COUNTS: u16 = 18;
/// The format that brings [`KIND_RESOLVE_INTENTS_COUNTED`].
pub const FORMAT_RESOLVE_COUNTED: u8 = 2;
/// `kind` of a [`Command::ResolveIntents`] that carries the change of each collection's live
/// documents (007 FR-039), as [`KIND_WRITE_COUNTED`] does for a write: without it a transaction's
/// commit leaves the counts estimated, and its range counts all its documents again.
pub const KIND_RESOLVE_INTENTS_COUNTED: u16 = 19;
/// The format that brings [`KIND_WRITE_REACHING`].
pub const FORMAT_REACHING: u8 = 3;
/// `kind` of a counted [`Command::Write`] whose idempotence record keeps the greatest data key the
/// write wrote (FR-022): a split copies to its new range only the records that reach it.
pub const KIND_WRITE_REACHING: u16 = 20;
/// `kind` of a [`Command::Split`] whose copied records carry their greatest key.
pub const KIND_SPLIT_REACHING: u16 = 21;
/// `kind` of a [`Command::Merge`] whose copied records carry their greatest key.
pub const KIND_MERGE_REACHING: u16 = 22;
/// The format that brings [`KIND_WRITE_SIZED`].
pub const FORMAT_SIZED: u8 = 5;
/// `kind` of a counted [`Command::Write`] whose change of each collection's documents carries
/// their bytes of BSON and of keys too (007 FR-039, `collStats`), with whether its idempotence
/// record reaches as in [`KIND_WRITE_REACHING`].
pub const KIND_WRITE_SIZED: u16 = 23;
/// `kind` of a counted [`Command::ResolveIntents`] whose change carries the bytes too.
pub const KIND_RESOLVE_INTENTS_SIZED: u16 = 24;
/// `kind` of an [`Command::AdjustCounts`] that carries the bytes the leader counted.
pub const KIND_ADJUST_SIZED: u16 = 25;

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
    /// Whether the record keeps the greatest data key the write wrote (FR-022,
    /// [`KIND_WRITE_REACHING`]): a split gives it to its new range only if that key falls there.
    pub reaching: bool,
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
    /// The greatest data key the statement wrote, if its record keeps it (FR-022): a split gives
    /// the record to its new range only if this key falls in it.
    pub reach: Option<Vec<u8>>,
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
        /// Change of each collection's live documents (007 FR-039), computed as `live_delta`;
        /// `None`: not counted, and a write of documents without it leaves the counts estimated.
        docs: Option<Vec<(crate::doc_count::Coll, crate::doc_count::Change)>>,
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
        /// Change of each collection's live documents (007 FR-039), computed as `live_delta`;
        /// `None`: not counted, and a commit of documents without it leaves the counts estimated.
        docs: Option<Vec<(crate::doc_count::Coll, crate::doc_count::Change)>>,
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
    /// The live documents of each collection that the leader counted at entry `base` (007
    /// FR-039): each replica adds the changes its counted writes made after `base`, and its
    /// counts are exact again if no entry left them estimated after it.
    AdjustCounts {
        /// The entry the leader counted at.
        base: u64,
        /// Live documents of each collection at `base`.
        counts: Vec<(crate::doc_count::Coll, u64)>,
        /// Their bytes of BSON and of keys at `base`, from [`FORMAT_SIZED`] on.
        sizes: Option<Vec<(crate::doc_count::Coll, crate::doc_count::Size)>>,
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
        reaching: false,
    })
}

/// Writes the change of each collection's live documents of a counted command.
/// The change of each collection's documents of a command; with `sized`, its bytes too.
type Docs = [(crate::doc_count::Coll, crate::doc_count::Change)];

/// Whether a change of documents says the bytes of every collection it changes, so that its
/// command goes in a kind of [`FORMAT_SIZED`]. An empty one changes nothing.
fn sized(docs: &Docs) -> bool {
    !docs.is_empty() && docs.iter().all(|(_, c)| c.size.is_some())
}

fn put_docs(e: &mut Encoder, docs: &Docs) {
    let sized = sized(docs);
    e.var(docs.len() as u64);
    for ((tenant, collection), c) in docs {
        e.u32(*tenant).u32(*collection).var(zigzag(c.docs));
        if let (true, Some((bytes, keys))) = (sized, c.size) {
            e.var(zigzag(bytes)).var(zigzag(keys));
        }
    }
}

/// Reads what [`put_docs`] wrote; `sized` for a kind of [`FORMAT_SIZED`].
fn get_docs(
    d: &mut Decoder<'_>,
    sized: bool,
) -> Result<Vec<(crate::doc_count::Coll, crate::doc_count::Change)>, DecodeError> {
    let n = d.var()?;
    let mut docs = Vec::new();
    for _ in 0..n {
        let coll = (d.u32()?, d.u32()?);
        let n = unzigzag(d.var()?);
        let change = match sized {
            true => crate::doc_count::Change::sized(n, unzigzag(d.var()?), unzigzag(d.var()?)),
            false => crate::doc_count::Change::docs(n),
        };
        docs.push((coll, change));
    }
    Ok(docs)
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
        records.push(CopiedRecord { key, mark, result: d.bytes()?.to_vec(), reach: None });
    }
    Ok(records)
}

/// The greatest key of each copied record, after everything else the entry carries (FR-022).
fn put_reaches(e: &mut Encoder, records: &[CopiedRecord]) {
    e.var(records.len() as u64);
    for r in records {
        e.bool(r.reach.is_some());
        if let Some(k) = &r.reach {
            e.bytes(k);
        }
    }
}

fn get_reaches(d: &mut Decoder<'_>, records: &mut [CopiedRecord]) -> Result<(), DecodeError> {
    let offset = d.position();
    if usize::try_from(d.var()?).ok() != Some(records.len()) {
        return Err(DecodeError::InvalidValue { offset });
    }
    for r in records {
        r.reach = if d.bool()? { Some(d.bytes()?.to_vec()) } else { None };
    }
    Ok(())
}

impl Command {
    /// Its `kind`.
    pub fn kind(&self) -> u16 {
        match self {
            Command::Noop => KIND_NOOP,
            Command::Write { docs: Some(d), .. } if sized(d) => KIND_WRITE_SIZED,
            Command::Write { docs: Some(_), dedup: Some(r), .. } if r.reaching => {
                KIND_WRITE_REACHING
            }
            Command::Write { docs: Some(_), .. } => KIND_WRITE_COUNTED,
            Command::Write { docs: None, .. } => KIND_WRITE,
            Command::SetGcThreshold { .. } => KIND_SET_GC_THRESHOLD,
            Command::SetClosedTs { .. } => KIND_SET_CLOSED_TS,
            Command::ComputeChecksum { .. } => KIND_COMPUTE_CHECKSUM,
            Command::PurgeDedup { .. } => KIND_PURGE_DEDUP,
            Command::ForgetRecord { .. } => KIND_FORGET_RECORD,
            Command::Relocate { .. } => KIND_RELOCATE,
            Command::Split { records, .. } if records.iter().any(|r| r.reach.is_some()) => {
                KIND_SPLIT_REACHING
            }
            Command::Split { .. } => KIND_SPLIT,
            Command::SplitGenesis { .. } => KIND_SPLIT_GENESIS,
            Command::MergeFreeze { .. } => KIND_MERGE_FREEZE,
            Command::Merge { absorb: Some(a), .. }
                if a.records.iter().any(|r| r.reach.is_some()) =>
            {
                KIND_MERGE_REACHING
            }
            Command::Merge { .. } => KIND_MERGE,
            Command::PutIntents { .. } => KIND_PUT_INTENTS,
            Command::EndTxn { .. } => KIND_END_TXN,
            Command::ResolveIntents { docs: Some(d), .. } if sized(d) => KIND_RESOLVE_INTENTS_SIZED,
            Command::ResolveIntents { docs: Some(_), .. } => KIND_RESOLVE_INTENTS_COUNTED,
            Command::ResolveIntents { docs: None, .. } => KIND_RESOLVE_INTENTS,
            Command::PushTxn { .. } => KIND_PUSH_TXN,
            Command::Fence { .. } => KIND_FENCE,
            Command::AdjustCounts { sizes: Some(_), .. } => KIND_ADJUST_SIZED,
            Command::AdjustCounts { sizes: None, .. } => KIND_ADJUST_COUNTS,
        }
    }

    /// Encodes the command as an envelope payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u16(self.kind());
        match self {
            Command::Noop => {}
            Command::Write { puts, deletes, dedup, live_delta, docs } => {
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
                if let Some(docs) = docs {
                    put_docs(&mut e, docs);
                    if sized(docs) {
                        e.bool(dedup.as_ref().is_some_and(|r| r.reaching));
                    }
                }
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
                if self.kind() == KIND_SPLIT_REACHING {
                    put_reaches(&mut e, records);
                }
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
                    if self.kind() == KIND_MERGE_REACHING {
                        put_reaches(&mut e, &a.records);
                    }
                }
            }
            Command::Fence { start, end, on } => {
                e.bytes(start).bytes(end).bool(*on);
            }
            Command::AdjustCounts { base, counts, sizes } => {
                e.u64(*base).var(counts.len() as u64);
                for ((tenant, collection), n) in counts {
                    e.u32(*tenant).u32(*collection).u64(*n);
                }
                if let Some(sizes) = sizes {
                    e.var(sizes.len() as u64);
                    for ((tenant, collection), (bytes, keys)) in sizes {
                        e.u32(*tenant).u32(*collection).u64(*bytes).u64(*keys);
                    }
                }
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
            Command::ResolveIntents { txn, status, commit_ts, resolutions, live_delta, docs } => {
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
                if let Some(docs) = docs {
                    put_docs(&mut e, docs);
                }
            }
        }
        e.finish()
    }

    /// Decodes a payload written by [`Command::encode`]; an unknown `kind` or trailing bytes are
    /// an error.
    pub fn decode(payload: &[u8]) -> Result<Command, DecodeError> {
        let mut d = Decoder::new(payload);
        let at = d.position();
        let kind = d.u16()?;
        let command = match kind {
            KIND_NOOP => Command::Noop,
            KIND_WRITE | KIND_WRITE_COUNTED | KIND_WRITE_REACHING | KIND_WRITE_SIZED => {
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
                let offset = d.position();
                let mut dedup = if d.bool()? { Some(get_record(&mut d)?) } else { None };
                if kind == KIND_WRITE_REACHING {
                    let r = dedup.as_mut().ok_or(DecodeError::InvalidValue { offset })?;
                    r.reaching = true;
                }
                let live_delta = unzigzag(d.var()?);
                let docs = match kind != KIND_WRITE {
                    true => Some(get_docs(&mut d, kind == KIND_WRITE_SIZED)?),
                    false => None,
                };
                if kind == KIND_WRITE_SIZED && d.bool()? {
                    let r = dedup.as_mut().ok_or(DecodeError::InvalidValue { offset })?;
                    r.reaching = true;
                }
                Command::Write { puts, deletes, dedup, live_delta, docs }
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
            KIND_SPLIT | KIND_SPLIT_REACHING => {
                let at = d.bytes()?.to_vec();
                let new_range = GroupId(d.u64()?);
                let mut records = get_copied(&mut d)?;
                let right_live = d.u64()?;
                if kind == KIND_SPLIT_REACHING {
                    get_reaches(&mut d, &mut records)?;
                }
                Command::Split { at, new_range, records, right_live }
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
            KIND_ADJUST_COUNTS | KIND_ADJUST_SIZED => {
                let (base, n) = (d.u64()?, d.var()?);
                let mut counts = Vec::new();
                for _ in 0..n {
                    counts.push(((d.u32()?, d.u32()?), d.u64()?));
                }
                let sizes = match kind == KIND_ADJUST_SIZED {
                    true => {
                        let m = d.var()?;
                        let mut sizes = Vec::new();
                        for _ in 0..m {
                            sizes.push(((d.u32()?, d.u32()?), (d.u64()?, d.u64()?)));
                        }
                        Some(sizes)
                    }
                    false => None,
                };
                Command::AdjustCounts { base, counts, sizes }
            }
            KIND_MERGE | KIND_MERGE_REACHING => {
                let (right, merge) = (GroupId(d.u64()?), d.u64()?);
                let absorb = if d.bool()? {
                    let end = d.bytes()?.to_vec();
                    let (gc, mark, live) = (get_hlc(&mut d)?, get_hlc(&mut d)?, d.u64()?);
                    let mut records = get_copied(&mut d)?;
                    let fences = crate::fence::get(&mut d)?;
                    if kind == KIND_MERGE_REACHING {
                        get_reaches(&mut d, &mut records)?;
                    }
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
            KIND_RESOLVE_INTENTS | KIND_RESOLVE_INTENTS_COUNTED | KIND_RESOLVE_INTENTS_SIZED => {
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
                let docs = match kind {
                    KIND_RESOLVE_INTENTS => None,
                    _ => Some(get_docs(&mut d, kind == KIND_RESOLVE_INTENTS_SIZED)?),
                };
                Command::ResolveIntents { txn, status, commit_ts, resolutions, live_delta, docs }
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
