//! Transactions on the leader of a range (008 FR-002 to FR-005, contract `txn-protocol.md`): a
//! request takes the latches of its keys like a write, reads the newest versions of each one
//! (deletions and intents included), and is evaluated inside the proposal at its own mark with the
//! rules of [`crate::intents`]. `PutIntents` and `ResolveIntents` answer with the entry's mark,
//! `EndTxn` and `PushTxn` with the record they leave. `PutIntents` answers once the entry is in the
//! leader's log (FR-021, D-160), and `CheckIntents` tells the coordinator, before its commit,
//! whether those intents were applied. A conflict goes out only once the core
//! confirms the leadership, as a refused mutation does.

use super::*;
use teseladb_lsm::Item;
use teseladb_multiraft::ReadMode;

use crate::command::{DedupRecord, IntentPut, Resolution};
use crate::intents::{
    Conflict, Intent, IntentOp, Latest, Push, RecordError, RecordOp, TxnId, TxnRecord, TxnStatus,
    check_put, end_txn_above, push_txn, pushed_by_read, record_key,
};
use crate::mutate::live_size_of;

/// What a transaction asks of one range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOp {
    /// Writes intents of `txn`, which reads at `read_ts` (FR-002).
    PutIntents {
        /// The transaction.
        txn: TxnId,
        /// Its read mark: a newer version is a conflict.
        read_ts: Hlc,
        /// The keys and what each will write, without repeated keys.
        writes: Vec<(Vec<u8>, IntentOp)>,
    },
    /// Changes the record of `txn`, which this range holds (FR-001, FR-004).
    EndTxn {
        /// The transaction.
        txn: TxnId,
        /// What the coordinator asks.
        op: RecordOp,
    },
    /// Pushes the record of `txn`, which this range holds (FR-002, FR-003).
    PushTxn {
        /// The transaction.
        txn: TxnId,
        /// The push.
        push: Push,
    },
    /// Commits a transaction whose keys are all in this range with one `Write` (FR-006), after the
    /// same conflict checks as `PutIntents`: no record and no intents.
    OnePhase {
        /// The transaction.
        txn: TxnId,
        /// Its read mark.
        read_ts: Hlc,
        /// The keys and what each writes, without repeated keys.
        writes: Vec<(Vec<u8>, IntentOp)>,
        /// The session and `txnNumber` of a client transaction (FR-022): the commit leaves their
        /// idempotence record in the same entry, so that the same request sent again answers
        /// with the commit's mark and a recovery from another node finds the decision. A commit
        /// a recovery refused first answers [`RangeError::NoSuchTransaction`].
        session: Option<(u128, i64)>,
    },
    /// The decision on the one-phase commit of client transaction `txn_number` of session `lsid`
    /// in this range (FR-022), for a recovery that found no record of it: [`Reply::Intents`] with
    /// the commit's mark if it committed here; otherwise the range writes the record that refuses
    /// the commit from then on and answers an empty [`Reply::Batch`], or
    /// [`RangeError::NoSuchTransaction`] when it was refused before.
    DecideOnePhase {
        /// The session.
        lsid: u128,
        /// Its transaction.
        txn_number: i64,
        /// A key of the range, which routes the request.
        at: Vec<u8>,
    },
    /// Locks `keys` for `txn`, which reads at `read_ts` and holds its writes until the commit, in
    /// the leader's memory and without an entry (FR-022): refused as [`TxnOp::PutIntents`] is when
    /// a version newer than `read_ts` or another transaction's intent is on a key, and with
    /// [`RangeError::WriteConflict`] when another live transaction holds one. It answers
    /// [`Reply::Intents`] with `read_ts`. Sent again with the same keys, it keeps `txn` active.
    LockKeys {
        /// The transaction.
        txn: TxnId,
        /// Its read mark.
        read_ts: Hlc,
        /// The keys its statement writes in this range.
        keys: Vec<Vec<u8>>,
    },
    /// Lets go of every lock of `txn` in this range (FR-022): it aborted, or its writes are
    /// intents now. It answers [`Reply::Intents`] and writes nothing.
    UnlockKeys {
        /// The transaction.
        txn: TxnId,
        /// The keys it locked in this range, which route and latch the request.
        keys: Vec<Vec<u8>>,
    },
    /// Whether every key of `keys` holds an applied intent of `txn` (FR-021): it waits for their
    /// latches, so for the intents in the leader's log to be applied or dropped, and answers
    /// [`Reply::Intents`] with the highest mark among them, or [`RangeError::NoSuchTransaction`]
    /// when a change of leader lost one. It writes nothing.
    CheckIntents {
        /// The transaction.
        txn: TxnId,
        /// The keys it wrote in this range.
        keys: Vec<Vec<u8>>,
    },
    /// Resolves the intents of a finished `txn` on `keys` (FR-005); a key without its intent is
    /// left as it is.
    ResolveIntents {
        /// The transaction.
        txn: TxnId,
        /// Its final state.
        status: TxnStatus,
        /// Its commit mark.
        commit_ts: Hlc,
        /// Keys with its intents.
        keys: Vec<Vec<u8>>,
    },
    /// Finds the keys of `[start, end)` this range holds with an intent of `txn`, at most
    /// [`FIND_BATCH`] (the reaper, for an interval its record could not list key by key, 008
    /// record GC). It answers with [`Reply::Batch`] of those keys, with empty values, followed by
    /// [`continues`] if the interval goes on past the range's end. It writes nothing.
    FindIntents {
        /// The transaction.
        txn: TxnId,
        /// First key.
        start: Vec<u8>,
        /// End (excluded).
        end: Vec<u8>,
    },
    /// A reader at `at` pushes `txn` above its mark by reading its record (008 T043): the leader
    /// reads it at a mark of its clock at or above `at`, which goes into the read cache of the
    /// record's key, and answers once it confirms it still leads, with the record as
    /// [`pushed_by_read`] leaves it. Nothing is written; `EndTxn` commits above that cache. A mark
    /// ahead of the leader's wall clock is refused with [`RangeError::Overloaded`], and the reader
    /// goes again after a pause.
    PushByRead {
        /// The transaction.
        txn: TxnId,
        /// The reader's mark.
        at: Hlc,
    },
    /// Reads the record of `txn` without writing anything: it answers with the record, or
    /// `NoSuchTransaction` (the reaper looks for a session's next transaction, 008 record GC).
    ReadRecord {
        /// The transaction.
        txn: TxnId,
    },
    /// Removes the record of `txn` if it is final, has no intervals left (every intent resolved,
    /// [`RecordOp::Resolved`]) and its last activity is below `before` (008 record GC); a record
    /// that is still needed is refused with [`RangeError::WriteConflict`].
    ForgetRecord {
        /// The transaction.
        txn: TxnId,
        /// The record's last heartbeat or commit has to be below this mark.
        before: Hlc,
    },
}

/// Most keys one [`TxnOp::FindIntents`] answers.
pub const FIND_BATCH: usize = 1_000;

/// Value of the row that ends a [`TxnOp::FindIntents`] answer when its interval goes on past the
/// range's end (the rows of the keys found have none): the row's key is where it goes on.
pub const CONTINUES: &[u8] = b"continues";

/// The row of a [`TxnOp::FindIntents`] answer that says where its interval goes on.
pub fn continues(at: Vec<u8>) -> Row {
    (at, (0, CONTINUES.to_vec()))
}

/// The last mark at which record `r` showed activity: its last heartbeat or its commit.
pub fn last_activity(r: &TxnRecord) -> Hlc {
    r.heartbeat_ts.max(r.commit_ts)
}

impl TxnOp {
    /// The keys it latches and reads.
    fn keys(&self) -> Vec<Vec<u8>> {
        match self {
            TxnOp::PutIntents { writes, .. } | TxnOp::OnePhase { writes, .. } => {
                writes.iter().map(|(k, _)| k.clone()).collect()
            }
            TxnOp::EndTxn { txn, .. }
            | TxnOp::PushTxn { txn, .. }
            | TxnOp::ForgetRecord { txn, .. }
            | TxnOp::ReadRecord { txn }
            | TxnOp::PushByRead { txn, .. } => vec![record_key(*txn)],
            TxnOp::ResolveIntents { keys, .. }
            | TxnOp::CheckIntents { keys, .. }
            | TxnOp::LockKeys { keys, .. }
            | TxnOp::UnlockKeys { keys, .. } => keys.clone(),
            TxnOp::FindIntents { start, .. } => vec![start.clone()],
            TxnOp::DecideOnePhase { at, .. } => vec![at.clone()],
        }
    }

    /// The idempotence record it reads and leaves (FR-022): a client transaction's one-phase
    /// commit and the decision on it.
    fn dedup(&self) -> Option<DedupKey> {
        match self {
            TxnOp::OnePhase { session: Some((lsid, txn_number)), .. }
            | TxnOp::DecideOnePhase { lsid, txn_number, .. } => Some(DedupKey {
                lsid: *lsid,
                txn_number: *txn_number,
                stmt_id: crate::onepc::COMMIT_STMT,
            }),
            _ => None,
        }
    }
}

/// A transaction's request to a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnReq {
    /// Range.
    pub g: GroupId,
    /// What it asks.
    pub op: TxnOp,
    /// Write concern.
    pub wc: WriteConcern,
}

/// Versions of a key a resolution reads: the intent and the one below it.
const RESOLVE_DEPTH: usize = 2;

/// The newest `n` versions of `key`, newest first, deletions and intents included (a retracted
/// intent does not show): `None` while a block is being read.
fn newest_versions(lsm: &RefCell<Lsm>, key: &[u8], n: usize) -> Option<Result<Vec<Item>, ()>> {
    let view = lsm.borrow().view();
    let end = [key, &[0]].concat();
    let mut scan = view.scan_time(key, &end, ZERO, NEWEST);
    let mut found = Vec::with_capacity(n);
    while found.len() < n {
        let step = scan.next(&mut lsm.borrow_mut());
        match step {
            Step::Item(i) => found.push(i),
            Step::Done => break,
            Step::Miss(_) => return None,
            Step::Failed(_) => return Some(Err(())),
        }
    }
    Some(Ok(found))
}

/// A version as the conflict rules read it; a deletion has no value.
/// What [`TxnOp::CheckIntents`] answers from the newest version of each key: the highest mark
/// of the intents of `txn`, if each key holds one.
fn held_intents(txn: TxnId, versions: &[Vec<Item>]) -> Result<Reply, RangeError> {
    let mut ts = Hlc { physical: 0, logical: 0 };
    for v in versions {
        let held = v.first().filter(|i| i.kind == Kind::Intent).is_some_and(|i| {
            crate::intents::Intent::decode(&i.value).is_ok_and(|intent| intent.txn == txn)
        });
        if !held {
            return Err(RangeError::NoSuchTransaction);
        }
        ts = ts.max(v[0].ts);
    }
    Ok(Reply::Intents { ts })
}

fn latest_of(i: &Item) -> Latest {
    let value = if i.kind == Kind::Delete { Vec::new() } else { i.value.to_vec() };
    Latest { ts: i.ts, intent: i.kind == Kind::Intent, value }
}

fn conflict_error(c: Conflict) -> RangeError {
    match c {
        Conflict::Newer(_) => RangeError::WriteConflict,
        Conflict::Intent(owner) => RangeError::IntentConflict(owner),
        Conflict::Corrupt => RangeError::Unavailable,
    }
}

fn record_error(e: RecordError) -> RangeError {
    match e {
        RecordError::NoSuchTransaction => RangeError::NoSuchTransaction,
        RecordError::Corrupt => RangeError::Unavailable,
    }
}

/// The stored record among a key's versions: its newest version, if it is a value.
fn stored_record(versions: &[Item]) -> Option<&[u8]> {
    versions.first().filter(|i| i.kind == Kind::Put).map(|i| &i.value[..])
}

/// Live bytes of a stored version (type and value), or 0 for a deletion.
fn stored_size(key: &[u8], version: Option<&Item>) -> i64 {
    version
        .filter(|v| v.kind == Kind::Put)
        .and_then(|v| v.value.split_first())
        .map_or(0, |(t, value)| live_size_of(key, *t, value))
}

/// What resolving a transaction's intents changes: the resolutions, the change of live bytes and
/// the change of each collection's live documents (007 FR-039).
type Resolved = (Vec<Resolution>, i64, Vec<(crate::doc_count::Coll, crate::doc_count::Change)>);

/// The resolutions of `txn`'s intents among `versions` and the changes of live bytes and of live
/// documents they make: a committed intent replaces the version below it, which the leader read
/// with it.
fn resolutions(
    txn: TxnId,
    status: TxnStatus,
    keys: &[Vec<u8>],
    versions: &[Vec<Item>],
    sized: bool,
) -> Result<Resolved, RangeError> {
    if status == TxnStatus::Pending {
        let e = "a pending transaction's intents are not resolved";
        return Err(RangeError::Mutation(MutationError(e.into())));
    }
    let mut out = Vec::new();
    let mut delta = 0;
    // What the commit writes over what it reads, for the change of documents and their bytes.
    let (mut read, mut current, mut puts, mut deletes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, v) in keys.iter().zip(versions) {
        let Some(newest) = v.first().filter(|i| i.kind == Kind::Intent) else { continue };
        let intent = Intent::decode(&newest.value).map_err(|_| RangeError::Unavailable)?;
        if intent.txn != txn {
            continue;
        }
        let commit = (status == TxnStatus::Committed).then_some(intent.op);
        if let Some(op) = &commit {
            let new = match op {
                IntentOp::Put { value_type, value } => live_size_of(key, *value_type, value),
                IntentOp::Delete => 0,
            };
            delta += new - stored_size(key, v.get(1));
            let below =
                v.get(1).filter(|i| i.kind == Kind::Put).and_then(|i| i.value.split_first());
            read.push(key.clone());
            current.push(below.map(|(t, rest)| (*t, rest)));
            match op {
                IntentOp::Put { value_type, value } => puts.push(crate::command::Put {
                    key: key.clone(),
                    value_type: *value_type,
                    value: value.clone(),
                }),
                IntentOp::Delete => deletes.push(key.clone()),
            }
        }
        out.push(Resolution { key: key.clone(), intent_ts: newest.ts, commit });
    }
    let docs = crate::doc_count::doc_delta(&read, &current, (&puts, &deletes), sized);
    Ok((out, delta, docs))
}

/// The idempotence record of the one-phase commit of a client transaction, with `result`
/// ([`crate::onepc::COMMITTED`] or [`crate::onepc::REFUSED`]).
fn decision(lsid: u128, txn_number: i64, result: &[u8]) -> DedupRecord {
    let stmt_id = crate::onepc::COMMIT_STMT;
    DedupRecord { lsid, txn_number, stmt_id, result: result.to_vec(), reaching: false }
}

/// The command a transaction's request proposes at mark `ts`, with the record it leaves.
fn evaluate_txn(
    op: &TxnOp,
    keys: &[Vec<u8>],
    versions: &[Vec<Item>],
    (ts, read_floor): (Hlc, Hlc),
    (counted, sized): (bool, bool),
) -> Result<(Command, Option<TxnRecord>), RangeError> {
    match op {
        TxnOp::PutIntents { txn, read_ts, writes } => {
            let mut intents = Vec::with_capacity(writes.len());
            for ((key, op), v) in writes.iter().zip(versions) {
                let latest = v.first().map(latest_of);
                // Faulty variant (009 T028): an index entry held by another transaction's intent
                // is taken as free. Only a unique index's entries are shared by documents, so two
                // documents with one value both get its entry.
                #[cfg(teseladb_bug = "unique_ignores_intents")]
                let latest = latest.filter(|l| {
                    let index_entry = key.len() > 12 && key[0] == 1 && key[9..13] != [0; 4];
                    !(l.intent && index_entry)
                });
                let replaces =
                    check_put(key, latest.as_ref(), *txn, *read_ts).map_err(conflict_error)?;
                intents.push(IntentPut { key: key.clone(), op: op.clone(), replaces });
            }
            let command = Command::PutIntents { txn: *txn, record_key: record_key(*txn), intents };
            Ok((command, None))
        }
        TxnOp::OnePhase { txn, read_ts, writes, session } => {
            let newest: Vec<Option<Latest>> =
                versions.iter().map(|v| v.first().map(latest_of)).collect();
            let command =
                crate::onepc::one_phase(*txn, *read_ts, writes, &newest).map_err(conflict_error)?;
            // A client transaction's commit leaves its decision in the same entry (FR-022).
            let command = match (command, session) {
                (Command::Write { puts, deletes, live_delta, docs, .. }, Some((lsid, number))) => {
                    let dedup = Some(decision(*lsid, *number, crate::onepc::COMMITTED));
                    Command::Write { puts, deletes, dedup, live_delta, docs }
                }
                (command, _) => command,
            };
            Ok((command, None))
        }
        // No commit of the transaction is in the range: none comes after this entry.
        TxnOp::DecideOnePhase { lsid, txn_number, .. } => {
            let command = Command::Write {
                puts: Vec::new(),
                deletes: Vec::new(),
                dedup: Some(decision(*lsid, *txn_number, crate::onepc::REFUSED)),
                live_delta: 0,
                docs: None,
            };
            Ok((command, None))
        }
        TxnOp::EndTxn { txn, op } => {
            let stored = versions.first().and_then(|v| stored_record(v));
            let record =
                end_txn_above(stored, *txn, op.clone(), ts, read_floor).map_err(record_error)?;
            Ok((Command::EndTxn { record: record.clone() }, Some(record)))
        }
        TxnOp::PushTxn { txn, push } => {
            let stored = versions.first().and_then(|v| stored_record(v));
            let record = push_txn(stored, *txn, *push, ts).map_err(record_error)?;
            // Faulty variant (008): a recovery answers «aborted» for a record that does not exist
            // and leaves none, so its slow coordinator may still begin and commit it.
            #[cfg(teseladb_bug = "record_not_fenced")]
            if stored.is_none() && matches!(push, Push::Abort { .. }) {
                let none = Command::Write {
                    puts: Vec::new(),
                    deletes: Vec::new(),
                    dedup: None,
                    live_delta: 0,
                    docs: None,
                };
                return Ok((none, Some(record)));
            }
            Ok((Command::PushTxn { record: record.clone() }, Some(record)))
        }
        TxnOp::ForgetRecord { txn, before } => {
            let stored = versions.first().and_then(|v| stored_record(v));
            let record = TxnRecord::decode(stored.ok_or(RangeError::NoSuchTransaction)?)
                .map_err(|_| RangeError::Unavailable)?;
            let gone = record.status.is_final()
                && record.spans.is_empty()
                && last_activity(&record) < *before;
            match gone {
                true => Ok((Command::ForgetRecord { txn: *txn }, None)),
                false => Err(RangeError::WriteConflict),
            }
        }
        TxnOp::FindIntents { .. }
        | TxnOp::ReadRecord { .. }
        | TxnOp::PushByRead { .. }
        | TxnOp::CheckIntents { .. }
        | TxnOp::LockKeys { .. }
        | TxnOp::UnlockKeys { .. } => {
            unreachable!("answered without a proposal (`step_txn`)")
        }
        TxnOp::ResolveIntents { txn, status, commit_ts, .. } => {
            let (resolutions, live_delta, docs) =
                resolutions(*txn, *status, keys, versions, sized)?;
            let command = Command::ResolveIntents {
                txn: *txn,
                status: *status,
                commit_ts: *commit_ts,
                resolutions,
                live_delta,
                docs: counted.then_some(docs),
            };
            Ok((command, None))
        }
    }
}

impl RangeHost {
    /// A transaction's request to a range (008).
    pub fn txn<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        req: TxnReq,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        let TxnReq { g, op, wc } = req;
        let (keys, dedup) = (op.keys(), op.dedup());
        self.submit(core, (g, keys, Work::Txn(Box::new(op)), dedup, wc), None, (now, wall_ms))
    }

    /// A transaction's request that the leader evaluates only at a mark whose physical part is
    /// at most `deadline`; past it the request answers [`RangeError::Timeout`] and writes nothing
    /// (009 FR-006).
    pub fn txn_until<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        req: TxnReq,
        deadline: u64,
        (now, wall_ms): (Nanos, u64),
    ) -> ReqId {
        let TxnReq { g, op, wc } = req;
        let (keys, dedup) = (op.keys(), op.dedup());
        let work = (g, keys, Work::Txn(Box::new(op)), dedup, wc);
        self.submit(core, work, Some(deadline), (now, wall_ms))
    }

    /// Reads the newest versions of a transaction's keys, then proposes its command.
    pub(super) fn step_txn<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        (latch, moved): (LatchId, bool),
        now: Nanos,
    ) -> bool {
        // The decision a one-phase commit of a client transaction left, or the refusal a recovery
        // left before it (FR-022): answered as it is, without a new entry. A session that has a
        // newer `txnNumber` in the range no longer commits this one.
        if let Some(key) = m.dedup {
            let decided = match dedup::lookup(&self.lsm, m.g, key) {
                None => None,
                Some(Err(())) => Some(Err(RangeError::Unavailable)),
                Some(Ok(Some((ts, result)))) => Some(match result == crate::onepc::COMMITTED {
                    true => Ok(Reply::Intents { ts }),
                    false => Err(RangeError::NoSuchTransaction),
                }),
                Some(Ok(None)) => match dedup::superseded(&self.lsm, m.g, key) {
                    None => None,
                    Some(Err(())) => Some(Err(RangeError::Unavailable)),
                    Some(Ok(Some(_))) => Some(Err(RangeError::NoSuchTransaction)),
                    Some(Ok(None)) => Some(Ok(Reply::Batch(Vec::new()))),
                },
            };
            match decided {
                // A block is being read: the request waits for it.
                None => {
                    self.reqs.insert(id, Req::Mut(m));
                    return moved;
                }
                // Nothing decided yet: the request goes on and leaves its record.
                Some(Ok(Reply::Batch(_))) => {}
                Some(answer) => {
                    self.release(m.g, m.term, latch);
                    self.reply(id, answer);
                    return true;
                }
            }
        }
        let depth = match &m.work {
            Work::Txn(op) if matches!(**op, TxnOp::ResolveIntents { .. }) => RESOLVE_DEPTH,
            Work::Txn(_) | Work::Mutation(_) => 1,
        };
        // Every key is read in one pass: the blocks missing from the cache are all asked for
        // together, and the request waits once for them (H-051 of the native tests: one block at
        // a time, 600 keys waited more than 10 s behind the disk's other work). What was read
        // stays read: the latch keeps it so.
        let mut read = match std::mem::take(&mut m.read) {
            Partial::Versions(l, read) if l == latch && read.len() == m.keys.len() => read,
            _ => vec![None; m.keys.len()],
        };
        let mut missing = false;
        for (k, slot) in m.keys.iter().zip(read.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            match newest_versions(&self.lsm, k, depth) {
                Some(Ok(v)) => *slot = Some(v),
                Some(Err(())) => {
                    self.release(m.g, m.term, latch);
                    self.reply(id, Err(RangeError::Unavailable));
                    return true;
                }
                None => missing = true,
            }
        }
        if missing {
            m.read = Partial::Versions(latch, read);
            self.wait_for_blocks(id, m);
            return moved;
        }
        let versions: Vec<Vec<Item>> = read.into_iter().flatten().collect();
        // A split cut the range while the request read its keys (007): the caller retries there.
        if !m.keys.iter().all(|k| self.covers(m.g, k)) {
            self.release(m.g, m.term, latch);
            self.reply(id, Err(RangeError::OutOfRange));
            return true;
        }
        let Work::Txn(op) = &m.work else { unreachable!("a transaction's request") };
        if let TxnOp::ReadRecord { .. } = &**op {
            let stored = versions.first().and_then(|v| stored_record(v));
            let answer = match stored.map(TxnRecord::decode) {
                None => Err(RangeError::NoSuchTransaction),
                Some(Ok(rec)) => Ok(Reply::Record(rec)),
                Some(Err(_)) => Err(RangeError::Unavailable),
            };
            self.release(m.g, m.term, latch);
            self.reply(id, answer);
            return true;
        }
        if let TxnOp::CheckIntents { txn, .. } = **op {
            let answer = held_intents(txn, &versions);
            self.release(m.g, m.term, latch);
            self.reply(id, answer);
            return true;
        }
        if let TxnOp::LockKeys { txn, read_ts, keys } = &**op {
            let answer = self.lock_keys(m.g, (*txn, *read_ts), keys, &versions);
            self.release(m.g, m.term, latch);
            self.reply(id, answer);
            return true;
        }
        if let TxnOp::UnlockKeys { txn, .. } = **op {
            if let Some(l) = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()) {
                l.locks.release(txn);
            }
            self.release(m.g, m.term, latch);
            self.reply(id, Ok(Reply::Intents { ts: Hlc { physical: 0, logical: 0 } }));
            return true;
        }
        if let TxnOp::PushByRead { txn, at } = **op {
            let stored = versions.first().and_then(|v| stored_record(v)).map(<[u8]>::to_vec);
            return self.push_by_read(core, id, m, latch, (txn, at, stored), now);
        }
        if let TxnOp::FindIntents { txn, start, end } = &**op {
            let range_end = self.ranges.get(&m.g).map(|r| r.end.clone()).unwrap_or_default();
            let past = !range_end.is_empty() && (end.is_empty() || *end > range_end);
            let stop = if past { range_end.clone() } else { end.clone() };
            let found = match crate::push::intent_keys(&self.lsm, (start, &stop), *txn, FIND_BATCH)
            {
                None => {
                    self.reqs.insert(id, Req::Mut(m));
                    return moved;
                }
                Some(Err(_)) => Err(RangeError::Unavailable),
                Some(Ok(keys)) => {
                    let mut rows: Vec<Row> =
                        keys.into_iter().map(|k| (k, (0, Vec::new()))).collect();
                    if rows.is_empty() && past {
                        rows.push(continues(range_end));
                    }
                    Ok(Reply::Batch(rows))
                }
            };
            self.release(m.g, m.term, latch);
            self.reply(id, found);
            return true;
        }
        self.propose_txn(core, id, m, latch, &versions, now)
    }

    /// [`TxnOp::LockKeys`] on the leader of range `g`, whose newest versions of `keys` are
    /// `versions` (FR-022).
    fn lock_keys(
        &mut self,
        g: GroupId,
        (txn, read_ts): (TxnId, Hlc),
        keys: &[Vec<u8>],
        versions: &[Vec<Item>],
    ) -> Result<Reply, RangeError> {
        for (key, v) in keys.iter().zip(versions) {
            let latest = v.first().map(latest_of);
            check_put(key, latest.as_ref(), txn, read_ts).map_err(conflict_error)?;
        }
        let now_ms = self.cluster_ms();
        let Some(l) = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()) else {
            return Err(RangeError::Unavailable);
        };
        if l.locks.holder(keys, Some(txn), now_ms).is_some() {
            return Err(RangeError::WriteConflict);
        }
        l.locks.lock(txn, keys, now_ms);
        Ok(Reply::Intents { ts: read_ts })
    }

    /// Whether another live transaction holds a lock on one of `keys` of range `g` (FR-022):
    /// `txn` is the transaction that asks, `None` for a write outside one.
    pub(super) fn locked_by_other(&self, g: GroupId, keys: &[Vec<u8>], txn: Option<TxnId>) -> bool {
        let now_ms = self.cluster_ms();
        self.ranges
            .get(&g)
            .and_then(|r| r.leader.as_ref())
            .is_some_and(|l| l.locks.holder(keys, txn, now_ms).is_some())
    }

    fn propose_txn<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        latch: LatchId,
        versions: &[Vec<Item>],
        now: Nanos,
    ) -> bool {
        let stopped = core.state_machine(m.g).is_some_and(StateMachine::halted);
        if self.refuse_moving(id, &m, latch, stopped) {
            return true;
        }
        let Work::Txn(op) = &m.work else { unreachable!("a transaction's request") };
        // A key another live transaction locked is a conflict for a transaction's writes, as its
        // intent would be; the writes of a transaction replace its own locks (008 FR-022).
        let writer = match &**op {
            TxnOp::PutIntents { txn, .. } | TxnOp::OnePhase { txn, .. } => Some(*txn),
            _ => None,
        };
        if let Some(txn) = writer {
            if self.locked_by_other(m.g, &m.keys, Some(txn)) {
                self.release(m.g, m.term, latch);
                self.reply(id, Err(RangeError::WriteConflict));
                return true;
            }
            if let Some(l) = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()) {
                l.locks.release(txn);
            }
        }
        let deciding = matches!(**op, TxnOp::DecideOnePhase { .. });
        let counted_resolutions = (self.counted_resolutions, self.sized_counts);
        let l = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()).expect("latched");
        let mut floor = l.cache.floor_of(&m.keys);
        // The reads of the keys, the record's for an `EndTxn`: its commit goes above them (008 T043).
        let read_floor = floor;
        // The commit is written at its mark: the entry goes above it.
        if let TxnOp::ResolveIntents { commit_ts, .. } = **op {
            floor = floor.max(commit_ts);
        }
        let mut record = None;
        let keys = &m.keys;
        let deadline = m.deadline;
        let build = |ts: Hlc| {
            // Past its deadline the request is not evaluated (009 FR-006).
            if deadline.is_some_and(|d| ts.physical > d) {
                return Err(RangeError::Timeout);
            }
            let (command, after) =
                evaluate_txn(op, keys, versions, (ts, read_floor), counted_resolutions)?;
            record = after;
            Ok(command.encode())
        };
        // `propose_at` gives a mark above the floor.
        let outcome = match core.propose_at(m.g, floor, now, build) {
            Ok((op_id, ts)) => {
                l.latches.set_mark(latch, ts);
                Ok((op_id, ts))
            }
            Err(ProposeAt::Build(e)) => {
                l.latches.release(latch);
                Err((e, true))
            }
            Err(ProposeAt::Op(e)) => {
                l.latches.release(latch);
                Err((op_error(e), false))
            }
        };
        match outcome {
            Ok((op_id, ts)) => {
                let index = core.status(m.g).map_or(0, |st| st.last_index);
                let reply = match deciding {
                    // The commit is refused from this entry on.
                    true => Reply::Batch(Vec::new()),
                    false => record.map_or(Reply::Intents { ts }, Reply::Record),
                };
                let reply = Some(Box::new(reply));
                let p = Proposal {
                    latch,
                    ts,
                    change: Vec::new(),
                    result: Vec::new(),
                    term: m.term,
                    index,
                    replied: false,
                    applied: false,
                    proposed_at: self.now,
                    reply,
                };
                m.state = MutState::Proposed(p);
                self.ops.insert(op_id, id);
                self.reqs.insert(id, Req::Mut(m));
            }
            // What the replica holds goes out only while it leads (005 FR-009).
            Err((e, true)) => match core.read(m.g, ReadMode::Lease, now) {
                Ok(op_id) => {
                    m.state = MutState::Confirm(e);
                    self.ops.insert(op_id, id);
                    self.reqs.insert(id, Req::Mut(m));
                }
                Err(oe) => self.reply(id, Err(op_error(oe))),
            },
            Err((e, false)) => self.reply(id, Err(e)),
        }
        true
    }

    /// [`TxnOp::PushByRead`] once the record's latch is held: the read at a mark of the clock at or
    /// above the reader's, which goes into the read cache of the record's key before the latch is
    /// released (an `EndTxn` waiting for it then commits above), and its answer once the core
    /// confirms the leadership, as a read's (005 FR-009).
    fn push_by_read<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        latch: LatchId,
        (txn, at, stored): (TxnId, Hlc, Option<Vec<u8>>),
        now: Nanos,
    ) -> bool {
        // The cache keeps only reads at marks the leader's clock reached, which the next leader's
        // cache starts above (005 FR-005): a mark ahead of the wall clock goes again later.
        let mark = match at.physical <= self.wall_ms {
            true => {
                self.clock.0.update(at);
                core.read_mark(now)
            }
            false => None,
        };
        let Some(mark) = mark.filter(|mark| *mark >= at) else {
            self.release(m.g, m.term, latch);
            self.reply(id, Err(RangeError::Overloaded));
            return true;
        };
        let record = pushed_by_read(stored.as_deref(), txn, mark).map_err(record_error);
        // Faulty variant (008 T043): the read does not go into the cache, so a commit evaluated
        // after it may go at or below the reader's mark.
        #[cfg(not(teseladb_bug = "push_read_not_cached"))]
        if let Some(l) = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()) {
            let key = record_key(txn);
            l.cache.record(&key, &[key.as_slice(), &[0]].concat(), mark);
        }
        self.release(m.g, m.term, latch);
        let record = match record {
            Ok(r) => Reply::Record(r),
            Err(e) => {
                self.reply(id, Err(e));
                return true;
            }
        };
        // The answer waits for the leadership to be confirmed, as a read's does (005 FR-009). It
        // has no faulty variant: a deposed leader gives a mark only while the clocks of the
        // others vouch for its own (005 FR-006), so its mark stays below the read cache of the
        // next leader, which commits above it anyway, and no run can tell the answer apart.
        match core.read(m.g, ReadMode::Lease, now) {
            Ok(op_id) => {
                m.state = MutState::Answer(Box::new(record));
                self.ops.insert(op_id, id);
                self.reqs.insert(id, Req::Mut(m));
            }
            Err(oe) => self.reply(id, Err(op_error(oe))),
        }
        true
    }
}
