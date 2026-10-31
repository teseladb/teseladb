//! Transactions on the leader of a range (008 FR-002 to FR-005, contract `txn-protocol.md`): a
//! request takes the latches of its keys like a write, reads the newest versions of each one
//! (deletions and intents included), and is evaluated inside the proposal at its own mark with the
//! rules of [`crate::intents`]. `PutIntents` and `ResolveIntents` answer with the entry's mark,
//! `EndTxn` and `PushTxn` with the record they leave. A conflict goes out only once the core
//! confirms the leadership, as a refused mutation does.

use super::*;
use teseladb_lsm::Item;
use teseladb_multiraft::ReadMode;

use crate::command::{IntentPut, Resolution};
use crate::intents::{
    Conflict, Intent, IntentOp, Latest, Push, RecordError, RecordOp, TxnId, TxnRecord, TxnStatus,
    check_put, end_txn, push_txn, record_key,
};
use crate::mutate::live_size;

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
}

impl TxnOp {
    /// The keys it latches and reads.
    fn keys(&self) -> Vec<Vec<u8>> {
        match self {
            TxnOp::PutIntents { writes, .. } | TxnOp::OnePhase { writes, .. } => {
                writes.iter().map(|(k, _)| k.clone()).collect()
            }
            TxnOp::EndTxn { txn, .. } | TxnOp::PushTxn { txn, .. } => vec![record_key(*txn)],
            TxnOp::ResolveIntents { keys, .. } => keys.clone(),
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
        .map_or(0, |(_, value)| live_size(key, value))
}

/// The resolutions of `txn`'s intents among `versions` and the change of live bytes they make.
fn resolutions(
    txn: TxnId,
    status: TxnStatus,
    keys: &[Vec<u8>],
    versions: &[Vec<Item>],
) -> Result<(Vec<Resolution>, i64), RangeError> {
    if status == TxnStatus::Pending {
        let e = "a pending transaction's intents are not resolved";
        return Err(RangeError::Mutation(MutationError(e.into())));
    }
    let mut out = Vec::new();
    let mut delta = 0;
    for (key, v) in keys.iter().zip(versions) {
        let Some(newest) = v.first().filter(|i| i.kind == Kind::Intent) else { continue };
        let intent = Intent::decode(&newest.value).map_err(|_| RangeError::Unavailable)?;
        if intent.txn != txn {
            continue;
        }
        let commit = (status == TxnStatus::Committed).then_some(intent.op);
        if let Some(op) = &commit {
            let new = match op {
                IntentOp::Put { value, .. } => live_size(key, value),
                IntentOp::Delete => 0,
            };
            delta += new - stored_size(key, v.get(1));
        }
        out.push(Resolution { key: key.clone(), intent_ts: newest.ts, commit });
    }
    Ok((out, delta))
}

/// The command a transaction's request proposes at mark `ts`, with the record it leaves.
fn evaluate_txn(
    op: &TxnOp,
    keys: &[Vec<u8>],
    versions: &[Vec<Item>],
    ts: Hlc,
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
        TxnOp::OnePhase { txn, read_ts, writes } => {
            let newest: Vec<Option<Latest>> =
                versions.iter().map(|v| v.first().map(latest_of)).collect();
            let command =
                crate::onepc::one_phase(*txn, *read_ts, writes, &newest).map_err(conflict_error)?;
            Ok((command, None))
        }
        TxnOp::EndTxn { txn, op } => {
            let stored = versions.first().and_then(|v| stored_record(v));
            let record = end_txn(stored, *txn, op.clone(), ts).map_err(record_error)?;
            Ok((Command::EndTxn { record: record.clone() }, Some(record)))
        }
        TxnOp::PushTxn { txn, push } => {
            let stored = versions.first().and_then(|v| stored_record(v));
            let record = push_txn(stored, *txn, *push, ts).map_err(record_error)?;
            Ok((Command::PushTxn { record: record.clone() }, Some(record)))
        }
        TxnOp::ResolveIntents { txn, status, commit_ts, .. } => {
            let (resolutions, live_delta) = resolutions(*txn, *status, keys, versions)?;
            let command = Command::ResolveIntents {
                txn: *txn,
                status: *status,
                commit_ts: *commit_ts,
                resolutions,
                live_delta,
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
        let keys = op.keys();
        self.submit(core, (g, keys, Work::Txn(Box::new(op)), None, wc), None, (now, wall_ms))
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
        let keys = op.keys();
        let work = (g, keys, Work::Txn(Box::new(op)), None, wc);
        self.submit(core, work, Some(deadline), (now, wall_ms))
    }

    /// Reads the newest versions of a transaction's keys, then proposes its command.
    pub(super) fn step_txn<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        m: MutReq,
        (latch, moved): (LatchId, bool),
        now: Nanos,
    ) -> bool {
        let depth = match &m.work {
            Work::Txn(op) if matches!(**op, TxnOp::ResolveIntents { .. }) => RESOLVE_DEPTH,
            Work::Txn(_) | Work::Mutation(_) => 1,
        };
        let mut versions = Vec::with_capacity(m.keys.len());
        for k in &m.keys {
            match newest_versions(&self.lsm, k, depth) {
                Some(Ok(v)) => versions.push(v),
                Some(Err(())) => {
                    self.release(m.g, m.term, latch);
                    self.reply(id, Err(RangeError::Unavailable));
                    return true;
                }
                None => {
                    self.reqs.insert(id, Req::Mut(m));
                    return moved;
                }
            }
        }
        // A split cut the range while the request read its keys (007): the caller retries there.
        if !m.keys.iter().all(|k| self.covers(m.g, k)) {
            self.release(m.g, m.term, latch);
            self.reply(id, Err(RangeError::OutOfRange));
            return true;
        }
        self.propose_txn(core, id, m, latch, &versions, now)
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
        let Work::Txn(op) = &m.work else { unreachable!("a transaction's request") };
        let l = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()).expect("latched");
        let mut floor = l.cache.floor_of(&m.keys);
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
            let (command, after) = evaluate_txn(op, keys, versions, ts)?;
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
                let reply = Some(Box::new(record.map_or(Reply::Intents { ts }, Reply::Record)));
                let p = Proposal {
                    latch,
                    ts,
                    change: Vec::new(),
                    result: Vec::new(),
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
}
