//! The encoding of the data range's requests and answers that travel to the range's leader on
//! another node as forwarded requests (006 FR-014, D-048's `Forward`): a write with its mutation
//! and a scan, and their results with the range's errors; and the pieces of a backup and a
//! restore (007 FR-021).

use teseladb_hlc::Hlc;
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::changes::Versioned;
use teseladb_range::command::DedupKey;
use teseladb_range::host::{MutationError, RangeError, TxnOp};
use teseladb_range::intents::{IntentOwner, TxnId, TxnRecord};
use teseladb_range::push::{Known, TxnView};
use teseladb_range::read::ReadConcern;
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

/// The evaluation of a write prepared on the connection's thread (005 FR-035). It travels with
/// the request inside the node and is no part of it: a request forwarded to another node goes
/// without, and two requests are the same whatever was prepared for them.
#[derive(Clone, Debug, Default)]
pub struct Ahead(pub Option<Box<teseladb_wire::commands::mutation::Prepared>>);

impl PartialEq for Ahead {
    fn eq(&self, _: &Ahead) -> bool {
        true
    }
}

impl Eq for Ahead {}

/// A request for the leader of the data range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteReq {
    /// A write: its keys, the encoded mutation, its statement, its write concern and its
    /// deadline.
    Write {
        /// Keys it latches.
        keys: Vec<Vec<u8>>,
        /// The encoded mutation (`teseladb_wire::commands::mutation::Bound`).
        mutation: Vec<u8>,
        /// Statement of a retryable write.
        dedup: Option<DedupKey>,
        /// Write concern.
        wc: WriteConcern,
        /// The mark's physical part past which the leader no longer evaluates it (009 FR-006).
        deadline: Option<u64>,
        /// Its evaluation, when the connection's thread prepared it.
        prepared: Ahead,
    },
    /// A scan of `[start, end)` of at most `limit` documents.
    Scan {
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
        /// Most documents.
        limit: u32,
        /// Read concern.
        concern: ReadConcern,
        /// From the last key down, the documents nearest `end` first (H-003).
        backward: bool,
    },
    /// A scan with a read preference (007 FR-013): on the first of `nodes` (each with whether the
    /// leader may serve it, should that replica lead the range), a follower at its closed mark no
    /// older than `max_stale_ms`, the leader as [`RemoteReq::Scan`]; a replica that cannot serve
    /// it, or the leader of a `secondary` read, sends it to the next one.
    FollowerScan {
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
        /// Most documents.
        limit: u32,
        /// How old the data may be (`maxStalenessSeconds`), in milliseconds.
        max_stale_ms: Option<u64>,
        /// The replicas to try, in order, and whether the leader may serve the read on each (false
        /// for `secondary`, which never reads the primary).
        nodes: Vec<(NodeId, bool)>,
        /// From the last key down (H-003).
        backward: bool,
    },
    /// A read of `key` at mark `at`, or at a mark the leader takes under the clock rule, with its
    /// value type (009 FR-003).
    Read {
        /// Key.
        key: Vec<u8>,
        /// Mark (`None`: the leader's).
        at: Option<Hlc>,
    },
    /// A read with what its reader knows about transactions (008 FR-003): its own, whose intents
    /// it reads, and those whose intents it met and pushed; of `key`, or a scan from `key` to
    /// `scan.0` of at most `scan.1` documents, at mark `at` (`None`: the leader's).
    ReadWith {
        /// Key, or the start of the scan.
        key: Vec<u8>,
        /// The scan's end and most documents (`None`: a read of `key`).
        scan: Option<(Vec<u8>, u32)>,
        /// A scan from the last key down (H-003).
        backward: bool,
        /// Mark.
        at: Option<Hlc>,
        /// Read concern.
        concern: ReadConcern,
        /// What the reader knows about transactions.
        view: TxnView,
        /// The end of its uncertainty window (008 FR-008; `None`: none).
        uncertain_to: Option<Hlc>,
    },
    /// A transaction's request (008, 009 FR-003).
    Txn {
        /// What it asks.
        op: TxnOp,
        /// The mark's physical part past which the leader no longer evaluates it (009 FR-006).
        deadline: Option<u64>,
    },
    /// A piece of the range's backup at mark `at` (007 FR-021): file `file` from byte `offset`.
    BackupFetch {
        /// The backup's mark.
        at: Hlc,
        /// File of the export.
        file: u32,
        /// Where in the file.
        offset: u64,
    },
    /// A piece of a restore's file `file` from byte `offset` (007 FR-021), which the leader
    /// keeps until the files are ingested.
    RestorePiece {
        /// File.
        file: u32,
        /// Where in the file.
        offset: u64,
        /// The bytes.
        data: Vec<u8>,
    },
    /// Ingests the `files` files of the restore into the range (007 FR-021).
    RestoreIngest {
        /// How many files the restore has.
        files: u32,
    },
    /// A change stream's read of `[start, end)` after mark `from` (014 FR-003), on the leader.
    Changes {
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
        /// The stream read the range up to this mark.
        from: Hlc,
        /// The mark the reader waits for (a search's, 011 FR-010): past the closed marks, the
        /// leader reads up to it as a read at that mark.
        at: Option<Hlc>,
    },
    /// Raises (`on`) or lifts the write fence of `[start, end)` (018 FR-002), on the leader.
    Fence {
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
        /// Whether the fence goes up.
        on: bool,
    },
    /// The range's live bytes (007 FR-005, D-068), which its state machine keeps: what the
    /// commands that weigh the data read without scanning it (018 FR-017).
    Live,
    /// The live documents of a collection the range holds, from its leader's counts (007
    /// FR-039).
    DocCount {
        /// Tenant.
        tenant: u32,
        /// Collection.
        collection: u32,
    },
    /// A scan of `[start, end)` of at most `limit` documents whose rows the leader runs `work`
    /// over, answering what the work gives instead of the rows (010 FR-034).
    Pushed {
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
        /// Most documents.
        limit: u32,
        /// Read concern.
        concern: ReadConcern,
        /// What the leader runs over the rows.
        work: crate::pushed::Work,
    },
}

fn put_mark(e: &mut Encoder, ts: Hlc) {
    e.u64(ts.physical).u32(ts.logical);
}

fn get_mark(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

/// A [`RemoteReq`] for range `group`, as it travels to the range's leader (007 FR-003).
pub fn encode_for(group: GroupId, req: &RemoteReq) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u64(group.0);
    [e.finish(), req.encode()].concat()
}

/// The range and the request of [`encode_for`].
pub fn decode_for(b: &[u8]) -> Result<(GroupId, RemoteReq), DecodeError> {
    let mut d = Decoder::new(b);
    let group = GroupId(d.u64()?);
    let at = d.position();
    Ok((group, RemoteReq::decode(&b[at..])?))
}

/// What the leader answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteReply {
    /// A write at mark `ts` with the mutation's result.
    Written {
        /// Mark.
        ts: Hlc,
        /// Encoded result.
        result: Vec<u8>,
    },
    /// The rows of a scan.
    Rows(Vec<(Vec<u8>, Vec<u8>)>),
    /// A read at mark `ts`: the visible version, with its value type.
    Value {
        /// Mark.
        ts: Hlc,
        /// Value type and value.
        value: Option<(u8, Vec<u8>)>,
    },
    /// A transaction's intents written or resolved at mark `ts`.
    Intents {
        /// Mark.
        ts: Hlc,
    },
    /// The transaction record a record change or a push left.
    Record(TxnRecord),
    /// A piece of a backup: not ready yet, or the sizes of the export's files and the bytes
    /// asked for.
    Piece {
        /// The export is ready.
        ready: bool,
        /// Sizes of the files of the export, in order.
        sizes: Vec<u64>,
        /// The bytes.
        data: Vec<u8>,
    },
    /// A restore's piece kept, or its files ingested (`done`); `false` while the ingestion goes
    /// on.
    Restored {
        /// The files are ingested.
        done: bool,
    },
    /// What a change stream read of the range (014 FR-003): the versions of `(from, to]` and the
    /// oldest intent pending there.
    Changes {
        /// The versions and deletions.
        rows: Vec<Versioned>,
        /// The end of the interval read.
        to: Hlc,
        /// The mark of the oldest pending intent.
        oldest_intent: Option<Hlc>,
    },
    /// A fence applied at mark `ts` (018 FR-002).
    Fenced {
        /// Mark of its entry.
        ts: Hlc,
    },
    /// The range's live bytes.
    Live {
        /// The bytes.
        bytes: u64,
    },
    /// The live documents of a collection in the range; `None` while its counts are estimated.
    DocCount {
        /// Documents.
        n: Option<u64>,
    },
    /// What the work of a [`RemoteReq::Pushed`] gave.
    Pushed(crate::pushed::Worked),
}

const WRITE: u8 = 0;
const SCAN: u8 = 1;
const READ: u8 = 2;
const TXN: u8 = 3;
const READ_WITH: u8 = 4;
const FOLLOWER_SCAN: u8 = 5;
const BACKUP_FETCH: u8 = 6;
const RESTORE_PIECE: u8 = 7;
const RESTORE_INGEST: u8 = 8;
const CHANGES: u8 = 9;
const FENCE: u8 = 10;
const LIVE: u8 = 11;
// A backward scan goes under a tag of its own (H-003): a forward one is encoded as before.
const SCAN_BACK: u8 = 12;
const FOLLOWER_SCAN_BACK: u8 = 13;
const READ_WITH_BACK: u8 = 14;
const DOC_COUNT: u8 = 15;
const PUSHED: u8 = 16;

fn put_hlc(e: &mut Encoder, ts: Hlc) {
    e.u64(ts.physical).u32(ts.logical);
}

fn get_hlc(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

fn put_view(e: &mut Encoder, v: &TxnView) {
    e.bool(v.own.is_some()).u128(v.own.map_or(0, |t| t.0)).var(v.known.len() as u64);
    for (txn, k) in &v.known {
        e.u128(txn.0);
        match k {
            Known::Pushed => {
                e.u8(0);
            }
            Known::Committed(ts) => {
                e.u8(1);
                put_hlc(e, *ts);
            }
            Known::Aborted => {
                e.u8(2);
            }
        }
    }
}

fn get_view(d: &mut Decoder<'_>) -> Result<TxnView, DecodeError> {
    let (some, own) = (d.bool()?, TxnId(d.u128()?));
    let at = d.position();
    let n = d.var()?;
    if n > d.remaining() as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    let mut known = std::collections::BTreeMap::new();
    for _ in 0..n {
        let txn = TxnId(d.u128()?);
        let at = d.position();
        let k = match d.u8()? {
            0 => Known::Pushed,
            1 => Known::Committed(get_hlc(d)?),
            2 => Known::Aborted,
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        known.insert(txn, k);
    }
    Ok(TxnView { own: some.then_some(own), known })
}

fn put_wc(e: &mut Encoder, wc: WriteConcern) {
    let (tag, n) = match wc.w {
        W::Zero => (0u8, 0u32),
        W::Nodes(n) => (1, n),
        W::Majority => (2, 0),
    };
    e.u8(tag).u32(n).bool(wc.j);
}

fn get_wc(d: &mut Decoder<'_>) -> Result<WriteConcern, DecodeError> {
    let at = d.position();
    let (tag, n, j) = (d.u8()?, d.u32()?, d.bool()?);
    let w = match tag {
        0 => W::Zero,
        1 => W::Nodes(n),
        2 => W::Majority,
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    };
    Ok(WriteConcern { w, j })
}

fn put_deadline(e: &mut Encoder, deadline: Option<u64>) {
    e.bool(deadline.is_some()).u64(deadline.unwrap_or_default());
}

fn get_deadline(d: &mut Decoder<'_>) -> Result<Option<u64>, DecodeError> {
    let (some, at) = (d.bool()?, d.u64()?);
    Ok(some.then_some(at))
}

fn concern_code(c: ReadConcern) -> u8 {
    match c {
        ReadConcern::Majority => 0,
        ReadConcern::Linearizable => 1,
        ReadConcern::Local => 2,
        ReadConcern::Available => 3,
    }
}

fn get_concern(d: &mut Decoder<'_>) -> Result<ReadConcern, DecodeError> {
    let at = d.position();
    match d.u8()? {
        0 => Ok(ReadConcern::Majority),
        1 => Ok(ReadConcern::Linearizable),
        2 => Ok(ReadConcern::Local),
        3 => Ok(ReadConcern::Available),
        _ => Err(DecodeError::InvalidValue { offset: at }),
    }
}

impl RemoteReq {
    /// Its encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            RemoteReq::Write { keys, mutation, dedup, wc, deadline, prepared: _ } => {
                e.u8(WRITE).var(keys.len() as u64);
                for k in keys {
                    e.bytes(k);
                }
                e.bytes(mutation).bool(dedup.is_some());
                if let Some(k) = dedup {
                    e.u128(k.lsid).i64(k.txn_number).u32(k.stmt_id);
                }
                put_wc(&mut e, *wc);
                put_deadline(&mut e, *deadline);
            }
            RemoteReq::Scan { start, end, limit, concern, backward } => {
                let tag = if *backward { SCAN_BACK } else { SCAN };
                e.u8(tag).bytes(start).bytes(end).u32(*limit).u8(concern_code(*concern));
            }
            RemoteReq::FollowerScan { start, end, limit, max_stale_ms, nodes, backward } => {
                let tag = if *backward { FOLLOWER_SCAN_BACK } else { FOLLOWER_SCAN };
                e.u8(tag).bytes(start).bytes(end).u32(*limit);
                e.bool(max_stale_ms.is_some()).u64(max_stale_ms.unwrap_or(0));
                e.var(nodes.len() as u64);
                for (node, leads) in nodes {
                    e.u64(node.0).bool(*leads);
                }
            }
            RemoteReq::Read { key, at } => {
                let mark = at.unwrap_or(Hlc { physical: 0, logical: 0 });
                e.u8(READ).bytes(key).bool(at.is_some()).u64(mark.physical).u32(mark.logical);
            }
            RemoteReq::Txn { op, deadline } => {
                e.u8(TXN);
                crate::txn_wire::put_op(&mut e, op);
                put_deadline(&mut e, *deadline);
            }
            RemoteReq::ReadWith { key, scan, backward, at, concern, view, uncertain_to } => {
                let tag = if *backward { READ_WITH_BACK } else { READ_WITH };
                e.u8(tag).bytes(key).bool(scan.is_some());
                if let Some((end, limit)) = scan {
                    e.bytes(end).u32(*limit);
                }
                e.bool(at.is_some());
                put_hlc(&mut e, at.unwrap_or(Hlc { physical: 0, logical: 0 }));
                e.u8(concern_code(*concern));
                put_view(&mut e, view);
                e.bool(uncertain_to.is_some());
                put_hlc(&mut e, uncertain_to.unwrap_or(Hlc { physical: 0, logical: 0 }));
            }
            RemoteReq::BackupFetch { at, file, offset } => {
                e.u8(BACKUP_FETCH);
                put_hlc(&mut e, *at);
                e.u32(*file).u64(*offset);
            }
            RemoteReq::RestorePiece { file, offset, data } => {
                e.u8(RESTORE_PIECE).u32(*file).u64(*offset).bytes(data);
            }
            RemoteReq::RestoreIngest { files } => {
                e.u8(RESTORE_INGEST).u32(*files);
            }
            RemoteReq::Changes { start, end, from, at } => {
                e.u8(CHANGES).bytes(start).bytes(end);
                put_mark(&mut e, *from);
                e.bool(at.is_some());
                put_mark(&mut e, at.unwrap_or(Hlc { physical: 0, logical: 0 }));
            }
            RemoteReq::Fence { start, end, on } => {
                e.u8(FENCE).bytes(start).bytes(end).bool(*on);
            }
            RemoteReq::Live => {
                e.u8(LIVE);
            }
            RemoteReq::DocCount { tenant, collection } => {
                e.u8(DOC_COUNT).u32(*tenant).u32(*collection);
            }
            RemoteReq::Pushed { start, end, limit, concern, work } => {
                e.u8(PUSHED).bytes(start).bytes(end).u32(*limit).u8(concern_code(*concern));
                work.put(&mut e);
            }
        }
        e.finish()
    }

    /// Reads what [`RemoteReq::encode`] wrote.
    pub fn decode(b: &[u8]) -> Result<RemoteReq, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let tag = d.u8()?;
        let r = match tag {
            WRITE => {
                let n = d.var()?;
                if n > d.remaining() as u64 {
                    return Err(DecodeError::InvalidValue { offset: at });
                }
                let keys =
                    (0..n).map(|_| d.bytes().map(<[u8]>::to_vec)).collect::<Result<Vec<_>, _>>()?;
                let mutation = d.bytes()?.to_vec();
                let dedup = if d.bool()? {
                    Some(DedupKey { lsid: d.u128()?, txn_number: d.i64()?, stmt_id: d.u32()? })
                } else {
                    None
                };
                let wc = get_wc(&mut d)?;
                let deadline = get_deadline(&mut d)?;
                RemoteReq::Write { keys, mutation, dedup, wc, deadline, prepared: Ahead::default() }
            }
            SCAN | SCAN_BACK => RemoteReq::Scan {
                start: d.bytes()?.to_vec(),
                end: d.bytes()?.to_vec(),
                limit: d.u32()?,
                concern: get_concern(&mut d)?,
                backward: tag == SCAN_BACK,
            },
            FOLLOWER_SCAN | FOLLOWER_SCAN_BACK => {
                let (start, end, limit) = (d.bytes()?.to_vec(), d.bytes()?.to_vec(), d.u32()?);
                let (some, stale) = (d.bool()?, d.u64()?);
                let n = d.var()?;
                let mut nodes = Vec::new();
                for _ in 0..n.min(d.remaining() as u64 / 9) {
                    nodes.push((NodeId(d.u64()?), d.bool()?));
                }
                if nodes.len() as u64 != n {
                    return Err(DecodeError::InvalidValue { offset: d.position() });
                }
                RemoteReq::FollowerScan {
                    start,
                    end,
                    limit,
                    max_stale_ms: some.then_some(stale),
                    nodes,
                    backward: tag == FOLLOWER_SCAN_BACK,
                }
            }
            READ => {
                let key = d.bytes()?.to_vec();
                let (some, mark) = (d.bool()?, Hlc { physical: d.u64()?, logical: d.u32()? });
                RemoteReq::Read { key, at: some.then_some(mark) }
            }
            TXN => {
                let op = crate::txn_wire::get_op(&mut d)?;
                RemoteReq::Txn { op, deadline: get_deadline(&mut d)? }
            }
            READ_WITH | READ_WITH_BACK => {
                let key = d.bytes()?.to_vec();
                let scan = match d.bool()? {
                    true => Some((d.bytes()?.to_vec(), d.u32()?)),
                    false => None,
                };
                let (some, mark) = (d.bool()?, get_hlc(&mut d)?);
                let concern = get_concern(&mut d)?;
                let view = get_view(&mut d)?;
                let (window, to) = (d.bool()?, get_hlc(&mut d)?);
                let (at, uncertain_to) = (some.then_some(mark), window.then_some(to));
                let backward = tag == READ_WITH_BACK;
                RemoteReq::ReadWith { key, scan, backward, at, concern, view, uncertain_to }
            }
            BACKUP_FETCH => {
                RemoteReq::BackupFetch { at: get_hlc(&mut d)?, file: d.u32()?, offset: d.u64()? }
            }
            RESTORE_PIECE => RemoteReq::RestorePiece {
                file: d.u32()?,
                offset: d.u64()?,
                data: d.bytes()?.to_vec(),
            },
            RESTORE_INGEST => RemoteReq::RestoreIngest { files: d.u32()? },
            CHANGES => RemoteReq::Changes {
                start: d.bytes()?.to_vec(),
                end: d.bytes()?.to_vec(),
                from: get_mark(&mut d)?,
                at: {
                    let some = d.bool()?;
                    let mark = get_mark(&mut d)?;
                    some.then_some(mark)
                },
            },
            FENCE => RemoteReq::Fence {
                start: d.bytes()?.to_vec(),
                end: d.bytes()?.to_vec(),
                on: d.bool()?,
            },
            LIVE => RemoteReq::Live,
            DOC_COUNT => RemoteReq::DocCount { tenant: d.u32()?, collection: d.u32()? },
            PUSHED => RemoteReq::Pushed {
                start: d.bytes()?.to_vec(),
                end: d.bytes()?.to_vec(),
                limit: d.u32()?,
                concern: get_concern(&mut d)?,
                work: crate::pushed::Work::get(&mut d)?,
            },
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        d.finish()?;
        Ok(r)
    }
}

const OK_WRITTEN: u8 = 0;
const OK_ROWS: u8 = 1;
const ERR: u8 = 2;
const OK_VALUE: u8 = 3;
const OK_INTENTS: u8 = 4;
const OK_RECORD: u8 = 5;
const OK_PIECE: u8 = 6;
const OK_RESTORED: u8 = 7;
const OK_CHANGES: u8 = 8;
const OK_FENCED: u8 = 9;
const OK_LIVE: u8 = 10;
const OK_DOC_COUNT: u8 = 11;
const OK_PUSHED: u8 = 12;

fn put_error(e: &mut Encoder, err: &RangeError) {
    let (tag, hint, text): (u8, u64, &str) = match err {
        RangeError::NotLeader { hint } => (0, hint.0, ""),
        RangeError::Timeout => (1, 0, ""),
        RangeError::Overloaded => (2, 0, ""),
        RangeError::OutOfRange => (3, 0, ""),
        RangeError::Unsatisfiable => (4, 0, ""),
        RangeError::TooLarge => (5, 0, ""),
        RangeError::Unavailable => (6, 0, ""),
        RangeError::SnapshotTooOld => (7, 0, ""),
        RangeError::Mutation(m) => (8, 0, m.0.as_str()),
        RangeError::WriteConflict => (9, 0, ""),
        RangeError::IntentConflict(_) => (10, 0, ""),
        RangeError::NoSuchTransaction => (11, 0, ""),
        RangeError::ReadIntent { .. } => (12, 0, ""),
        RangeError::ReadUncertain { .. } => (13, 0, ""),
        RangeError::Fenced => (14, 0, ""),
        RangeError::KeyLocked => (15, 0, ""),
    };
    e.u8(tag).u64(hint).str(text);
    let put_owner = |e: &mut Encoder, o: &IntentOwner| {
        e.u128(o.txn.0).bytes(&o.record_key).bytes(&o.key).u64(o.ts.physical).u32(o.ts.logical);
    };
    match err {
        RangeError::IntentConflict(o) => put_owner(e, o),
        RangeError::ReadIntent { owner, at } => {
            put_owner(e, owner);
            e.u64(at.physical).u32(at.logical);
        }
        RangeError::ReadUncertain { ts } => {
            e.u64(ts.physical).u32(ts.logical);
        }
        _ => {}
    }
}

fn get_owner(d: &mut Decoder<'_>) -> Result<IntentOwner, DecodeError> {
    Ok(IntentOwner {
        txn: TxnId(d.u128()?),
        record_key: d.bytes()?.to_vec(),
        key: d.bytes()?.to_vec(),
        ts: Hlc { physical: d.u64()?, logical: d.u32()? },
    })
}

fn get_error(d: &mut Decoder<'_>) -> Result<RangeError, DecodeError> {
    let at = d.position();
    let (tag, hint, text) = (d.u8()?, d.u64()?, d.str()?);
    Ok(match tag {
        0 => RangeError::NotLeader { hint: NodeId(hint) },
        1 => RangeError::Timeout,
        2 => RangeError::Overloaded,
        3 => RangeError::OutOfRange,
        4 => RangeError::Unsatisfiable,
        5 => RangeError::TooLarge,
        6 => RangeError::Unavailable,
        7 => RangeError::SnapshotTooOld,
        8 => RangeError::Mutation(MutationError(text.to_owned())),
        9 => RangeError::WriteConflict,
        10 => RangeError::IntentConflict(get_owner(d)?),
        11 => RangeError::NoSuchTransaction,
        12 => RangeError::ReadIntent {
            owner: get_owner(d)?,
            at: Hlc { physical: d.u64()?, logical: d.u32()? },
        },
        13 => RangeError::ReadUncertain { ts: Hlc { physical: d.u64()?, logical: d.u32()? } },
        14 => RangeError::Fenced,
        15 => RangeError::KeyLocked,
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    })
}

/// Encodes a leader's answer.
pub fn encode_reply(r: &Result<RemoteReply, RangeError>) -> Vec<u8> {
    let mut e = Encoder::new();
    match r {
        Ok(RemoteReply::Written { ts, result }) => {
            e.u8(OK_WRITTEN).u64(ts.physical).u32(ts.logical).bytes(result);
        }
        Ok(RemoteReply::Rows(rows)) => {
            e.u8(OK_ROWS).var(rows.len() as u64);
            for (k, v) in rows {
                e.bytes(k).bytes(v);
            }
        }
        Ok(RemoteReply::Value { ts, value }) => {
            e.u8(OK_VALUE).u64(ts.physical).u32(ts.logical).bool(value.is_some());
            if let Some((t, v)) = value {
                e.u8(*t).bytes(v);
            }
        }
        Ok(RemoteReply::Intents { ts }) => {
            e.u8(OK_INTENTS).u64(ts.physical).u32(ts.logical);
        }
        Ok(RemoteReply::Record(record)) => {
            e.u8(OK_RECORD).bytes(&record.encode());
        }
        Ok(RemoteReply::Piece { ready, sizes, data }) => {
            e.u8(OK_PIECE).bool(*ready).var(sizes.len() as u64);
            for s in sizes {
                e.u64(*s);
            }
            e.bytes(data);
        }
        Ok(RemoteReply::Restored { done }) => {
            e.u8(OK_RESTORED).bool(*done);
        }
        Ok(RemoteReply::Changes { rows, to, oldest_intent }) => {
            e.u8(OK_CHANGES).var(rows.len() as u64);
            for r in rows {
                e.bytes(&r.key);
                put_mark(&mut e, r.ts);
                e.bool(r.value.is_some());
                if let Some((t, v)) = &r.value {
                    e.u8(*t).bytes(v);
                }
            }
            put_mark(&mut e, *to);
            e.bool(oldest_intent.is_some());
            put_mark(&mut e, oldest_intent.unwrap_or(Hlc { physical: 0, logical: 0 }));
        }
        Ok(RemoteReply::Fenced { ts }) => {
            e.u8(OK_FENCED);
            put_mark(&mut e, *ts);
        }
        Ok(RemoteReply::Live { bytes }) => {
            e.u8(OK_LIVE).u64(*bytes);
        }
        Ok(RemoteReply::DocCount { n }) => {
            e.u8(OK_DOC_COUNT).bool(n.is_some()).u64(n.unwrap_or(0));
        }
        Ok(RemoteReply::Pushed(worked)) => {
            e.u8(OK_PUSHED);
            worked.put(&mut e);
        }
        Err(err) => {
            e.u8(ERR);
            put_error(&mut e, err);
        }
    }
    e.finish()
}

/// Reads what [`encode_reply`] wrote.
pub fn decode_reply(b: &[u8]) -> Result<Result<RemoteReply, RangeError>, DecodeError> {
    let mut d = Decoder::new(b);
    let at = d.position();
    let r = match d.u8()? {
        OK_WRITTEN => {
            let ts = Hlc { physical: d.u64()?, logical: d.u32()? };
            Ok(RemoteReply::Written { ts, result: d.bytes()?.to_vec() })
        }
        OK_ROWS => {
            let n = d.var()?;
            if n > d.remaining() as u64 {
                return Err(DecodeError::InvalidValue { offset: at });
            }
            let mut rows = Vec::new();
            for _ in 0..n {
                rows.push((d.bytes()?.to_vec(), d.bytes()?.to_vec()));
            }
            Ok(RemoteReply::Rows(rows))
        }
        OK_VALUE => {
            let ts = Hlc { physical: d.u64()?, logical: d.u32()? };
            let value = if d.bool()? { Some((d.u8()?, d.bytes()?.to_vec())) } else { None };
            Ok(RemoteReply::Value { ts, value })
        }
        OK_INTENTS => {
            Ok(RemoteReply::Intents { ts: Hlc { physical: d.u64()?, logical: d.u32()? } })
        }
        OK_RECORD => {
            let at = d.position();
            let record = TxnRecord::decode(d.bytes()?)
                .map_err(|_| DecodeError::InvalidValue { offset: at })?;
            Ok(RemoteReply::Record(record))
        }
        OK_PIECE => {
            let ready = d.bool()?;
            let n = d.var()?;
            let mut sizes = Vec::new();
            for _ in 0..n.min(d.remaining() as u64 / 8) {
                sizes.push(d.u64()?);
            }
            if sizes.len() as u64 != n {
                return Err(DecodeError::InvalidValue { offset: d.position() });
            }
            Ok(RemoteReply::Piece { ready, sizes, data: d.bytes()?.to_vec() })
        }
        OK_RESTORED => Ok(RemoteReply::Restored { done: d.bool()? }),
        OK_CHANGES => {
            let n = d.var()?;
            if n > d.remaining() as u64 {
                return Err(DecodeError::InvalidValue { offset: at });
            }
            let mut rows = Vec::new();
            for _ in 0..n {
                let key = d.bytes()?.to_vec();
                let ts = get_mark(&mut d)?;
                let value = if d.bool()? { Some((d.u8()?, d.bytes()?.to_vec())) } else { None };
                rows.push(Versioned { key, ts, value });
            }
            let to = get_mark(&mut d)?;
            let (some, intent) = (d.bool()?, get_mark(&mut d)?);
            Ok(RemoteReply::Changes { rows, to, oldest_intent: some.then_some(intent) })
        }
        OK_FENCED => Ok(RemoteReply::Fenced { ts: get_mark(&mut d)? }),
        OK_LIVE => Ok(RemoteReply::Live { bytes: d.u64()? }),
        OK_DOC_COUNT => {
            let (some, n) = (d.bool()?, d.u64()?);
            Ok(RemoteReply::DocCount { n: some.then_some(n) })
        }
        OK_PUSHED => Ok(RemoteReply::Pushed(crate::pushed::Worked::get(&mut d)?)),
        ERR => Err(get_error(&mut d)?),
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    };
    d.finish()?;
    Ok(r)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_read_back() {
        let reqs = [
            RemoteReq::Write {
                keys: vec![vec![1, 2], vec![3]],
                mutation: vec![9; 5],
                dedup: Some(DedupKey { lsid: 7, txn_number: 2, stmt_id: 1 }),
                wc: WriteConcern { w: W::Nodes(2), j: true },
                deadline: Some(1_800_000_000_000),
                prepared: Ahead::default(),
            },
            RemoteReq::Write {
                keys: vec![],
                mutation: vec![],
                dedup: None,
                wc: WriteConcern { w: W::Zero, j: false },
                deadline: None,
                prepared: Ahead::default(),
            },
            RemoteReq::Write {
                keys: vec![vec![1]],
                mutation: vec![],
                dedup: None,
                wc: WriteConcern { w: W::Majority, j: true },
                deadline: Some(0),
                prepared: Ahead::default(),
            },
            RemoteReq::Scan {
                start: vec![1],
                end: vec![2],
                limit: 1,
                concern: ReadConcern::Majority,
                backward: true,
            },
            RemoteReq::FollowerScan {
                start: vec![1],
                end: vec![],
                limit: 4,
                max_stale_ms: Some(5),
                nodes: vec![(NodeId(1), false)],
                backward: true,
            },
            RemoteReq::ReadWith {
                key: vec![1],
                scan: Some((vec![3], 2)),
                backward: true,
                at: None,
                concern: ReadConcern::Majority,
                view: TxnView::default(),
                uncertain_to: None,
            },
            RemoteReq::Scan {
                start: vec![1],
                end: vec![2],
                limit: 10,
                concern: ReadConcern::Majority,
                backward: false,
            },
            RemoteReq::Scan {
                start: vec![],
                end: vec![],
                limit: 0,
                concern: ReadConcern::Linearizable,
                backward: false,
            },
            RemoteReq::Scan {
                start: vec![],
                end: vec![],
                limit: 0,
                concern: ReadConcern::Local,
                backward: false,
            },
            RemoteReq::Scan {
                start: vec![],
                end: vec![],
                limit: 0,
                concern: ReadConcern::Available,
                backward: false,
            },
            RemoteReq::Read { key: vec![1, 2], at: Some(Hlc { physical: 4, logical: 1 }) },
            RemoteReq::Read { key: vec![3], at: None },
            RemoteReq::ReadWith {
                key: vec![1],
                scan: Some((vec![2], 9)),
                backward: false,
                at: Some(Hlc { physical: 4, logical: 1 }),
                concern: ReadConcern::Local,
                view: TxnView {
                    own: Some(TxnId(1)),
                    known: [
                        (TxnId(2), Known::Pushed),
                        (TxnId(3), Known::Committed(Hlc { physical: 3, logical: 0 })),
                        (TxnId(4), Known::Aborted),
                    ]
                    .into(),
                },
                uncertain_to: Some(Hlc { physical: 254, logical: u32::MAX }),
            },
            RemoteReq::ReadWith {
                key: vec![1],
                scan: None,
                backward: false,
                at: None,
                concern: ReadConcern::Majority,
                view: TxnView::default(),
                uncertain_to: None,
            },
            RemoteReq::Txn {
                op: TxnOp::PushTxn {
                    txn: TxnId(3),
                    push: teseladb_range::intents::Push::AbortIfAbandoned {
                        intent_ts: Hlc { physical: 2, logical: 0 },
                    },
                },
                deadline: None,
            },
            RemoteReq::Txn {
                op: TxnOp::PushTxn {
                    txn: TxnId(4),
                    push: teseladb_range::intents::Push::To(Hlc { physical: 2, logical: 0 }),
                },
                deadline: Some(7),
            },
            RemoteReq::BackupFetch {
                at: Hlc { physical: 5, logical: 1 },
                file: 2,
                offset: 1 << 20,
            },
            RemoteReq::RestorePiece { file: 1, offset: 9, data: vec![1, 2, 3] },
            RemoteReq::RestoreIngest { files: 3 },
            RemoteReq::Changes {
                start: vec![1],
                end: vec![2],
                from: Hlc { physical: 7, logical: 3 },
                at: None,
            },
            RemoteReq::Changes {
                start: vec![1],
                end: vec![2],
                from: Hlc { physical: 7, logical: 3 },
                at: Some(Hlc { physical: 9, logical: 1 }),
            },
            RemoteReq::Fence { start: vec![1], end: vec![2], on: true },
            RemoteReq::Fence { start: vec![], end: vec![9], on: false },
            RemoteReq::Live,
            RemoteReq::Pushed {
                start: vec![1],
                end: vec![2],
                limit: 1024,
                concern: ReadConcern::Local,
                work: crate::pushed::Work::Fold(vec![5, 6]),
            },
        ];
        for r in reqs {
            assert_eq!(RemoteReq::decode(&r.encode()), Ok(r));
        }
        // A view with more transactions than bytes, or an unknown state.
        let mut bad = Encoder::new();
        bad.u8(READ_WITH).bytes(&[1]).bool(false).bool(false).u64(0).u32(0).u8(2);
        let head = bad.finish();
        let mut many = Encoder::new();
        many.bool(false).u128(0).var(1_000);
        assert!(RemoteReq::decode(&[head.clone(), many.finish()].concat()).is_err());
        let mut state = Encoder::new();
        state.bool(false).u128(0).var(1).u128(5).u8(9);
        assert!(RemoteReq::decode(&[head, state.finish()].concat()).is_err());
        let replies = [
            Ok(RemoteReply::Written { ts: Hlc { physical: 5, logical: 2 }, result: vec![1] }),
            Ok(RemoteReply::Rows(vec![(vec![1], vec![2])])),
            Ok(RemoteReply::Value { ts: Hlc { physical: 5, logical: 2 }, value: None }),
            Ok(RemoteReply::Value {
                ts: Hlc { physical: 5, logical: 2 },
                value: Some((2, vec![3])),
            }),
            Ok(RemoteReply::Intents { ts: Hlc { physical: 6, logical: 0 } }),
            Ok(RemoteReply::Fenced { ts: Hlc { physical: 6, logical: 4 } }),
            Ok(RemoteReply::Live { bytes: 1 << 40 }),
            Ok(RemoteReply::Pushed(crate::pushed::Worked::Declined)),
            Ok(RemoteReply::Pushed(crate::pushed::Worked::Folded {
                groups: vec![3; 9],
                examined: 1024,
                next: Some(vec![4, 0]),
            })),
            Err(RangeError::Fenced),
            Ok(RemoteReply::Changes {
                rows: vec![
                    Versioned { key: vec![1], ts: Hlc { physical: 4, logical: 1 }, value: None },
                    Versioned {
                        key: vec![2],
                        ts: Hlc { physical: 5, logical: 0 },
                        value: Some((1, vec![9])),
                    },
                ],
                to: Hlc { physical: 6, logical: 0 },
                oldest_intent: Some(Hlc { physical: 7, logical: 0 }),
            }),
            Ok(RemoteReply::Changes {
                rows: vec![],
                to: Hlc { physical: 6, logical: 0 },
                oldest_intent: None,
            }),
            Ok(RemoteReply::Record(TxnRecord {
                txn: TxnId(8),
                lsid: 0,
                txn_number: 0,
                status: teseladb_range::intents::TxnStatus::Committed,
                read_ts: Hlc { physical: 1, logical: 0 },
                min_commit_ts: Hlc { physical: 1, logical: 0 },
                commit_ts: Hlc { physical: 2, logical: 0 },
                heartbeat_ts: Hlc { physical: 1, logical: 0 },
                spans: vec![(vec![1], vec![2])],
            })),
            Err(RangeError::NotLeader { hint: NodeId(3) }),
            Err(RangeError::Timeout),
            Err(RangeError::Overloaded),
            Err(RangeError::OutOfRange),
            Err(RangeError::Unsatisfiable),
            Err(RangeError::TooLarge),
            Err(RangeError::Unavailable),
            Err(RangeError::SnapshotTooOld),
            Err(RangeError::ReadUncertain { ts: Hlc { physical: 8, logical: 3 } }),
            Err(RangeError::Mutation(MutationError("x".into()))),
            Err(RangeError::WriteConflict),
            Err(RangeError::IntentConflict(IntentOwner {
                txn: TxnId(5),
                record_key: vec![2, 5],
                key: vec![1, 5],
                ts: Hlc { physical: 9, logical: 1 },
            })),
            Err(RangeError::NoSuchTransaction),
            Err(RangeError::ReadIntent {
                owner: IntentOwner {
                    txn: TxnId(6),
                    record_key: vec![2, 6],
                    key: vec![1, 6],
                    ts: Hlc { physical: 3, logical: 0 },
                },
                at: Hlc { physical: 4, logical: 2 },
            }),
            Ok(RemoteReply::Piece { ready: false, sizes: vec![], data: vec![] }),
            Ok(RemoteReply::Piece { ready: true, sizes: vec![3, 4], data: vec![7; 3] }),
            Ok(RemoteReply::Restored { done: true }),
        ];
        for r in replies {
            assert_eq!(decode_reply(&encode_reply(&r)), Ok(r));
        }
    }

    #[test]
    fn what_is_not_a_request_or_a_reply_is_refused() {
        assert!(RemoteReq::decode(&[]).is_err());
        assert!(RemoteReq::decode(&[9]).is_err());
        assert!(decode_reply(&[OK_RECORD, 1, 9]).is_err(), "a record that does not decode");
        assert!(decode_reply(&[OK_PIECE, 1, 2, 0, 0, 0, 0, 0, 0, 0, 1]).is_err(), "sizes missing");
        assert!(RemoteReq::decode(&[WRITE, 200]).is_err());
        let mut bad_wc = RemoteReq::Write {
            keys: vec![],
            mutation: vec![],
            dedup: None,
            wc: WriteConcern { w: W::Zero, j: false },
            deadline: None,
            prepared: Ahead::default(),
        }
        .encode();
        // The tag of the write concern, before its count, `j` and the deadline.
        let n = bad_wc.len();
        bad_wc[n - 15] = 9;
        assert!(RemoteReq::decode(&bad_wc).is_err());
        let mut bad_rc = RemoteReq::Scan {
            start: vec![],
            end: vec![],
            limit: 0,
            concern: ReadConcern::Local,
            backward: false,
        }
        .encode();
        let n = bad_rc.len();
        bad_rc[n - 1] = 9;
        assert!(RemoteReq::decode(&bad_rc).is_err());
        assert!(decode_reply(&[]).is_err());
        assert!(decode_reply(&[7]).is_err());
        assert!(decode_reply(&[OK_ROWS, 200]).is_err());
        assert!(decode_reply(&[OK_ROWS, 5]).is_err());
        assert!(RemoteReq::decode(&[WRITE, 5]).is_err());
        let mut bad_err = encode_reply(&Err(RangeError::Timeout));
        bad_err[1] = 42;
        assert!(decode_reply(&bad_err).is_err());
        let mut long = encode_reply(&Err(RangeError::Timeout));
        long.push(0);
        assert!(decode_reply(&long).is_err());
    }
}
