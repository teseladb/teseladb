//! The coordinator of one transaction (008 FR-001 to FR-005, plan «Confirmación de una
//! transacción de varios rangos»), a sans-I/O state machine. Each statement writes intents with
//! `PutIntents` in the ranges its keys fall in; the first one also creates the record with
//! `EndTxn(Begin)` in the range of `02 · txn_id`. A heartbeat keeps the record alive. The commit is
//! `EndTxn(Commit)` at the highest of the coordinator's clock and its intents' marks (the record
//! raises it to its `min_commit_ts`); once it is answered the coordinator resolves the intents
//! range by range, as it does after an abort.
//!
//! A range answers `PutIntents` once the intents are in its leader's log, not yet durable (FR-021,
//! D-160): before `EndTxn(Commit)` the coordinator asks each range it wrote in with
//! `CheckIntents` whether they were applied. One that a change of leader lost fails the commit
//! with `NoSuchTransaction` and the coordinator aborts; one without an answer leaves the commit
//! unknown, and the driver asks again.
//!
//! A commit whose mark is above the coordinator's wall clock answers once the clock reaches it
//! (commit wait, FR-008): a new leader's writes go up to 250 ms above every clock (its read cache
//! starts there, 005 FR-005) and a reader's push lifts a commit to that reader's clock. Answering
//! before would let a transaction that begins afterwards, on a node whose clock is behind, read
//! below the commit and past its uncertainty window, and miss a commit acknowledged before it
//! began. Once the clock is at the mark, a later transaction's mark is at most the clock offset
//! below it, inside the window. A read-only transaction commits at its read mark and waits the
//! same way, since what it read may be such a commit.
//!
//! A write that runs into another transaction's intent pushes that transaction as abandoned: if
//! the push aborts it (or finds it finished) the coordinator resolves that intent and writes
//! again; if it is alive the statement fails with `WriteConflict`, as in the reference server.
//!
//! A range that no longer holds a request's keys, because a split or a merge moved them, answers
//! `OutOfRange` without applying it: the coordinator asks its [`Route`] again and sends the
//! request, cut by range, where the keys are now. While the route still names the range that
//! refused it, the request waits for the next [`Coordinator::tick`].
//!
//! The embedder sends each [`Request`] to its range, hands every answer to
//! [`Coordinator::on_reply`], ticks it with the same [`Route`] and reads the [`Event`]s.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{
    IntentOp, IntentOwner, Push, RecordOp, TxnId, TxnRecord, TxnStatus, record_key,
};
use teseladb_types::{GroupId, Nanos};

use crate::record::{Heartbeat, Spans};

/// Rounds of pushing and resolving the intents a write runs into before it gives up with
/// `WriteConflict`.
const MAX_CONFLICT_ROUNDS: u32 = 3;
/// Attempts to resolve the intents in one range; the readers and the reaper resolve what is left.
const MAX_RESOLVE_ATTEMPTS: u32 = 10;
/// What a transaction's state may take in the coordinator's memory (008 FR-010: 1 GiB per core
/// until 012 gives each core its part of `--memory-limit`). There is no limit on its intents
/// otherwise: they live in the LSM and the record keeps intervals.
pub const MEMORY_BUDGET: usize = 1 << 30;
/// Most bytes of keys and values a batch of [`Coordinator::write_and_commit`] puts in one
/// `PutIntents`: half of a Raft entry's 64 MiB (005 FR-023), with room for the entry's own
/// encoding. A batch past it goes in parts, one statement each, and commits them all at once (008
/// FR-006): a document whose index entries do not fit in one entry.
pub const PART_BYTES: usize = 32 << 20;

/// Which range holds a key.
pub trait Route {
    /// The range of `key`, if the coordinator's node knows one.
    fn range_of(&self, key: &[u8]) -> Option<GroupId>;

    /// Every range the node knows, each with a key it holds (008 FR-022: the recovery of a
    /// one-phase commit's decision asks them all). None for a route that only places keys.
    fn ranges(&self) -> Vec<(GroupId, Vec<u8>)> {
        Vec::new()
    }
}

/// Identifies a request of the coordinator.
pub type ReqNo = u64;
/// Identifies a statement.
pub type StmtId = u64;

/// A request for a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Its number, which comes back with the answer.
    pub id: ReqNo,
    /// The range.
    pub g: GroupId,
    /// What it asks.
    pub op: TxnOp,
}

/// Why a statement, a commit or an abort failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnError {
    /// A conflict with another transaction or a newer version: the reference server's
    /// `WriteConflict`.
    WriteConflict,
    /// The record was aborted (by a push or the reaper), or the transaction is finished:
    /// The reference server's `NoSuchTransaction`.
    NoSuchTransaction,
    /// No range is known for a key.
    NoRange,
    /// Its state passed the coordinator's memory budget: the reference server's
    /// `TransactionTooLarge` (257).
    TooLarge,
    /// A range failed.
    Range(RangeError),
}

/// How a transaction ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Committed at this mark.
    Committed(Hlc),
    /// Aborted.
    Aborted,
    /// The commit's answer was lost: it may have committed. A new [`Coordinator::commit`] asks
    /// again (the reference server's `UnknownTransactionCommitResult`).
    Unknown(RangeError),
    /// Refused before it could commit.
    Failed(TxnError),
}

/// What the coordinator tells its embedder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A statement's intents are written, or it failed.
    Written {
        /// The statement.
        stmt: StmtId,
        /// Its result.
        result: Result<(), TxnError>,
    },
    /// The commit or the abort ended.
    Ended(Outcome),
    /// Every intent is resolved: the coordinator has nothing left to do.
    Resolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    /// The commit waits for the record, at this proposed mark.
    CommitWaiting(Hlc),
    Committing,
    /// The commit's answer was lost.
    CommitUnknown,
    /// The answer of a one-phase commit was lost: nothing tells whether it was applied.
    OnePhaseUnknown,
    Aborting,
    /// Committed at this mark, above the coordinator's clock: the answer waits for the clock.
    ClockWait(Hlc),
    Committed(Hlc),
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordState {
    NotSent,
    Sent,
    Created,
}

/// A one-phase commit to send again: its range, its writes and the clock it commits at.
type OnePhaseAgain = (GroupId, Vec<(Vec<u8>, IntentOp)>, Hlc);

/// What a request in flight was.
#[derive(Clone, Debug)]
enum Pending {
    Begin,
    Heartbeat,
    Intents(Write),
    /// The push of the transaction whose intent `write` ran into.
    Push(Write, IntentOwner),
    /// The resolution of that transaction's intent, before `write` goes again.
    ResolveOther(Write),
    /// The commit at this mark.
    Commit(Hlc),
    /// The one `Write` of a one-phase commit, with its keys and the clock it commits at.
    OnePhase(Vec<(Vec<u8>, IntentOp)>, Hlc),
    /// `DecideOnePhase` in a range that holds keys of a one-phase commit whose answer was lost.
    Decide,
    Abort,
    Resolve(GroupId),
    /// `CheckIntents` of the keys written in this range, before the commit.
    Check,
}

/// Where the check of the intents before the commit is (FR-021).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checks {
    /// Not asked.
    NotSent,
    /// Asked; this many ranges have not answered.
    Waiting(usize),
    /// Every range answered that its intents are applied.
    Done,
}

/// The intents of one statement in one range.
#[derive(Clone, Debug)]
struct Write {
    stmt: StmtId,
    g: GroupId,
    writes: Vec<(Vec<u8>, IntentOp)>,
    rounds: u32,
}

/// Which range a request a range refused with `OutOfRange` may go to now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Again {
    /// Only one the route names instead: it was just refused.
    OtherRange,
    /// The one the route names, also the same: it waited a tick.
    SameRange,
}

/// The coordinator of one transaction.
#[derive(Debug)]
pub struct Coordinator {
    txn: TxnId,
    lsid: u128,
    txn_number: i64,
    read_ts: Hlc,
    phase: Phase,
    record: RecordState,
    record_g: Option<GroupId>,
    /// Keys with intents, by range.
    written: BTreeMap<GroupId, BTreeSet<Vec<u8>>>,
    spans: Spans,
    intent_max: Hlc,
    /// The check of the intents before the commit (FR-021).
    checks: Checks,
    heartbeat: Heartbeat,
    /// Requests in flight, with their range.
    pending: BTreeMap<ReqNo, (GroupId, Pending)>,
    /// Requests a range refused with `OutOfRange` while the route still names it: they go again
    /// at the next tick.
    stalled: Vec<(GroupId, Pending)>,
    /// Requests in flight of each statement, and its first error.
    stmts: BTreeMap<StmtId, (usize, Option<TxnError>)>,
    /// A failure that dooms the transaction (its record was aborted).
    doomed: Option<TxnError>,
    /// Ranges still to resolve and the attempts each took.
    resolving: BTreeMap<GroupId, u32>,
    /// Ranges whose resolution failed, to send again at the next tick.
    retry_resolve: BTreeSet<GroupId>,
    /// Ranges whose resolution in flight lacks keys that moved there: sent again once it answers.
    resolve_again: BTreeSet<GroupId>,
    /// Aborts sent that got no answer, and whether one goes again at the next tick: the abort
    /// gives the record the intervals of the intents, which nothing else may have given it.
    abort_attempts: u32,
    retry_abort: bool,
    /// Why the answer of a one-phase commit was lost.
    one_phase_lost: Option<RangeError>,
    /// The one-phase commit of a client transaction whose answer was lost, which a commit asked
    /// again sends again: its idempotence record answers it if it was applied (FR-022).
    one_phase_again: Option<OnePhaseAgain>,
    /// A one-phase commit got no answer, so it may be applied where its keys were.
    one_phase_tried: bool,
    /// Ranges asked with `DecideOnePhase` that have not answered.
    deciding: usize,
    out: Vec<Request>,
    events: Vec<Event>,
    next_req: ReqNo,
    next_stmt: StmtId,
    /// Bytes its keys take in the coordinator (each is kept for its range and its record).
    memory: usize,
    budget: usize,
    /// The highest wall clock (ms) the embedder gave, with the commit or a tick.
    wall_ms: u64,
    /// See [`PART_BYTES`].
    part_bytes: usize,
}

/// `writes` cut, in order, into parts whose keys and values take at most `bytes` each (a write
/// larger than that alone is a part of its own).
fn parts(writes: Vec<(Vec<u8>, IntentOp)>, bytes: usize) -> Vec<Vec<(Vec<u8>, IntentOp)>> {
    let size = |(k, op): &(Vec<u8>, IntentOp)| {
        k.len()
            + match op {
                IntentOp::Put { value, .. } => value.len(),
                IntentOp::Delete => 0,
            }
    };
    let mut out: Vec<Vec<(Vec<u8>, IntentOp)>> = vec![Vec::new()];
    let mut used = 0;
    for w in writes {
        let n = size(&w);
        if used + n > bytes && out.last().is_some_and(|p| !p.is_empty()) {
            out.push(Vec::new());
            used = 0;
        }
        used += n;
        if let Some(last) = out.last_mut() {
            last.push(w);
        }
    }
    out
}

impl Coordinator {
    /// The coordinator of transaction `txn` of session `(lsid, txn_number)`, reading at `read_ts`.
    pub fn new(txn: TxnId, (lsid, txn_number): (u128, i64), read_ts: Hlc, now: Nanos) -> Self {
        Coordinator {
            txn,
            lsid,
            txn_number,
            read_ts,
            phase: Phase::Open,
            record: RecordState::NotSent,
            record_g: None,
            written: BTreeMap::new(),
            spans: Spans::default(),
            intent_max: Hlc { physical: 0, logical: 0 },
            checks: Checks::NotSent,
            heartbeat: Heartbeat::new(now),
            pending: BTreeMap::new(),
            stalled: Vec::new(),
            stmts: BTreeMap::new(),
            doomed: None,
            resolving: BTreeMap::new(),
            retry_resolve: BTreeSet::new(),
            resolve_again: BTreeSet::new(),
            abort_attempts: 0,
            retry_abort: false,
            one_phase_lost: None,
            one_phase_again: None,
            one_phase_tried: false,
            deciding: 0,
            out: Vec::new(),
            events: Vec::new(),
            next_req: 1,
            next_stmt: 1,
            memory: 0,
            budget: MEMORY_BUDGET,
            wall_ms: 0,
            part_bytes: PART_BYTES,
        }
    }

    /// The same coordinator with a memory budget of `bytes` instead of [`MEMORY_BUDGET`].
    pub fn with_budget(self, bytes: usize) -> Self {
        Coordinator { budget: bytes, ..self }
    }

    /// The same coordinator with parts of `bytes` instead of [`PART_BYTES`].
    pub fn with_part_bytes(self, bytes: usize) -> Self {
        Coordinator { part_bytes: bytes, ..self }
    }

    /// The transaction.
    pub fn txn(&self) -> TxnId {
        self.txn
    }

    /// The session and number of the transaction.
    pub fn session(&self) -> (u128, i64) {
        (self.lsid, self.txn_number)
    }

    /// Its read mark.
    pub fn read_ts(&self) -> Hlc {
        self.read_ts
    }

    /// Whether it has written anything (its record went out with its first write): a coordinator
    /// that has not can be replaced by one that reads at another mark (FR-008).
    pub fn written(&self) -> bool {
        self.record != RecordState::NotSent
    }

    /// The highest mark its intents were written at.
    pub fn intent_max(&self) -> Hlc {
        self.intent_max
    }

    /// The ranges it wrote intents in.
    pub fn written_ranges(&self) -> BTreeSet<GroupId> {
        self.written.keys().copied().collect()
    }

    /// Intents it has yet to resolve once it ended: those of the ranges whose resolution has
    /// not answered (008 FR-019).
    pub fn unresolved(&self) -> usize {
        let pending = self.resolving.keys().chain(&self.retry_resolve);
        let ranges: BTreeSet<&GroupId> = pending.collect();
        ranges.iter().filter_map(|g| self.written.get(g)).map(BTreeSet::len).sum()
    }

    /// The wall clock (ms) a commit waits for before it answers, if it is waiting: the embedder
    /// ticks it once its clock is there.
    pub fn clock_wait(&self) -> Option<u64> {
        match self.phase {
            Phase::ClockWait(ts) => Some(ts.physical),
            _ => None,
        }
    }

    /// The requests to send.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.out)
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn send(&mut self, g: GroupId, op: TxnOp, p: Pending) {
        let id = self.next_req;
        self.next_req += 1;
        self.pending.insert(id, (g, p));
        self.out.push(Request { id, g, op });
    }

    fn send_write(&mut self, w: Write) {
        let op =
            TxnOp::PutIntents { txn: self.txn, read_ts: self.read_ts, writes: w.writes.clone() };
        self.send(w.g, op, Pending::Intents(w));
    }

    /// Sends an operation on the record to the record's range.
    fn send_record(&mut self, p: Pending) {
        let op = match p {
            Pending::Begin => RecordOp::Begin {
                lsid: self.lsid,
                txn_number: self.txn_number,
                read_ts: self.read_ts,
            },
            Pending::Heartbeat => RecordOp::Heartbeat { spans: self.spans.intervals() },
            Pending::Commit(ts) => RecordOp::Commit { ts, spans: self.spans.intervals() },
            Pending::Abort => RecordOp::Abort {
                lsid: self.lsid,
                txn_number: self.txn_number,
                spans: self.spans.intervals(),
            },
            Pending::Intents(_)
            | Pending::Push(..)
            | Pending::ResolveOther(_)
            | Pending::OnePhase(..)
            | Pending::Decide
            | Pending::Resolve(_)
            | Pending::Check => unreachable!("not an operation on the record"),
        };
        self.send(self.record_range(), TxnOp::EndTxn { txn: self.txn, op }, p);
    }

    /// A statement that writes `writes` (each key once); the answer comes as [`Event::Written`].
    pub fn write(
        &mut self,
        route: &dyn Route,
        writes: Vec<(Vec<u8>, IntentOp)>,
    ) -> Result<StmtId, TxnError> {
        if self.phase != Phase::Open {
            return Err(TxnError::NoSuchTransaction);
        }
        if let Some(e) = &self.doomed {
            return Err(e.clone());
        }
        let mut by_range: BTreeMap<GroupId, Vec<(Vec<u8>, IntentOp)>> = BTreeMap::new();
        for (key, op) in writes {
            let g = route.range_of(&key).ok_or(TxnError::NoRange)?;
            by_range.entry(g).or_default().push((key, op));
        }
        let added: usize = by_range.values().flatten().map(|(k, _)| 2 * k.len()).sum();
        if self.memory.saturating_add(added) > self.budget {
            // Too large: the transaction cannot go on, and its abort resolves what it wrote.
            self.doomed = Some(TxnError::TooLarge);
            return Err(TxnError::TooLarge);
        }
        self.memory += added;
        if self.record == RecordState::NotSent {
            let g = route.range_of(&record_key(self.txn)).ok_or(TxnError::NoRange)?;
            self.record_g = Some(g);
            self.record = RecordState::Sent;
            self.send_record(Pending::Begin);
        }
        let stmt = self.next_stmt;
        self.next_stmt += 1;
        self.stmts.insert(stmt, (by_range.len(), None));
        if by_range.is_empty() {
            self.finish_stmt(stmt);
        }
        for (g, writes) in by_range {
            // An intent whose answer is lost may exist: the key is resolved in any case.
            for (k, _) in &writes {
                self.spans.add(k);
                self.written.entry(g).or_default().insert(k.clone());
            }
            self.send_write(Write { stmt, g, writes, rounds: 0 });
        }
        Ok(stmt)
    }

    fn finish_stmt(&mut self, stmt: StmtId) {
        if let Some((0, error)) = self.stmts.get(&stmt).cloned() {
            self.stmts.remove(&stmt);
            self.events.push(Event::Written { stmt, result: error.map_or(Ok(()), Err) });
        }
    }

    fn write_done(&mut self, stmt: StmtId, error: Option<TxnError>) {
        let s = self.stmts.get_mut(&stmt).expect("a statement with writes in flight");
        s.0 -= 1;
        if s.1.is_none() {
            s.1 = error;
        }
        self.finish_stmt(stmt);
    }

    /// A transaction made of one batch of writes, committed at no less than `clock`: in one phase
    /// when every key is in one range, nothing was written before and the batch fits in one part
    /// (FR-006), otherwise as statements of at most [`PART_BYTES`] each followed by the commit.
    /// The answer comes as [`Event::Ended`].
    pub fn write_and_commit(
        &mut self,
        route: &dyn Route,
        writes: Vec<(Vec<u8>, IntentOp)>,
        clock: Hlc,
    ) {
        let fresh = self.phase == Phase::Open
            && self.record == RecordState::NotSent
            && self.doomed.is_none()
            && self.stmts.is_empty();
        let ranges: BTreeSet<Option<GroupId>> =
            writes.iter().map(|(k, _)| route.range_of(k)).collect();
        let parts = parts(writes, self.part_bytes);
        let one = match (fresh, ranges.into_iter().collect::<Vec<_>>().as_slice(), &parts[..]) {
            (true, [Some(g)], [_]) => Some(*g),
            _ => None,
        };
        let Some(g) = one else {
            for part in parts {
                if let Err(e) = self.write(route, part) {
                    return self.events.push(Event::Ended(Outcome::Failed(e)));
                }
            }
            return self.commit_at(clock);
        };
        let writes = parts.into_iter().flatten().collect::<Vec<_>>();
        self.send_one_phase(g, writes, clock);
    }

    /// Sends the one `Write` of a one-phase commit of `writes` to range `g`.
    fn send_one_phase(&mut self, g: GroupId, writes: Vec<(Vec<u8>, IntentOp)>, clock: Hlc) {
        self.phase = Phase::Committing;
        // A client transaction's commit leaves its decision with its session (FR-022).
        let session = (self.lsid != 0).then_some((self.lsid, self.txn_number));
        let (txn, read_ts) = (self.txn, self.read_ts);
        let op = TxnOp::OnePhase { txn, read_ts, writes: writes.clone(), session };
        self.send(g, op, Pending::OnePhase(writes, clock));
    }

    /// Commits at no less than `clock`, the coordinator's wall clock; the answer comes as
    /// [`Event::Ended`], once the clock is at the commit's mark. After an [`Outcome::Unknown`] it
    /// asks again.
    pub fn commit(&mut self, clock: Hlc) {
        self.wall_ms = self.wall_ms.max(clock.physical);
        self.commit_at(clock);
    }

    /// [`Coordinator::commit`] at no less than `clock`, which says nothing of the wall clock.
    fn commit_at(&mut self, clock: Hlc) {
        match self.phase {
            Phase::Open | Phase::CommitUnknown => {}
            Phase::Committed(ts) => return self.events.push(Event::Ended(Outcome::Committed(ts))),
            Phase::Aborted | Phase::Aborting => {
                return self
                    .events
                    .push(Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)));
            }
            Phase::CommitWaiting(_) | Phase::Committing | Phase::ClockWait(_) => return,
            // A client transaction's commit goes again: the record it left answers it (FR-022).
            Phase::OnePhaseUnknown => match self.one_phase_again.take() {
                Some((g, writes, clock)) => return self.send_one_phase(g, writes, clock),
                None => return self.repeat_lost(),
            },
        }
        if let Some(e) = self.doomed.clone() {
            self.start_abort();
            return self.events.push(Event::Ended(Outcome::Failed(e)));
        }
        if self.record == RecordState::NotSent {
            // Nothing written: a read-only transaction commits at its read mark.
            return self.ended(Outcome::Committed(self.read_ts));
        }
        let ts = clock.max(self.intent_max);
        self.phase = Phase::CommitWaiting(ts);
        self.try_commit();
    }

    /// Sends the commit once the record exists and no write is in flight.
    fn try_commit(&mut self) {
        let Phase::CommitWaiting(ts) = self.phase else { return };
        let writing = self.pending.values().chain(&self.stalled).any(|(_, p)| {
            matches!(p, Pending::Intents(_) | Pending::Push(..) | Pending::ResolveOther(_))
        });
        if self.record != RecordState::Created || writing {
            return;
        }
        // Every intent applied first: their answers came once they were in the leaders' logs.
        match self.checks {
            Checks::Done => {}
            Checks::Waiting(_) => return,
            Checks::NotSent => {
                let ranges: Vec<(GroupId, Vec<Vec<u8>>)> = self
                    .written
                    .iter()
                    .map(|(g, keys)| (*g, keys.iter().cloned().collect()))
                    .collect();
                self.checks = Checks::Waiting(ranges.len());
                for (g, keys) in ranges {
                    self.send(g, TxnOp::CheckIntents { txn: self.txn, keys }, Pending::Check);
                }
                if !matches!(self.checks, Checks::Waiting(0)) {
                    return;
                }
                self.checks = Checks::Done;
            }
        }
        // Every intent's mark is known now.
        let ts = ts.max(self.intent_max);
        self.phase = Phase::Committing;
        self.send_record(Pending::Commit(ts));
        // Faulty variant (008 T024): the intents become committed versions as the commit goes,
        // before the record says it committed; a record that ends aborted leaves them behind.
        #[cfg(teseladb_bug = "resolve_before_durable_commit")]
        for (g, keys) in self.written.clone() {
            let keys = keys.into_iter().collect();
            let status = TxnStatus::Committed;
            let op = TxnOp::ResolveIntents { txn: self.txn, status, commit_ts: ts, keys };
            self.send(g, op, Pending::Heartbeat);
        }
    }

    /// Nothing tells whether a one-phase commit whose answer was lost was applied.
    fn repeat_lost(&mut self) {
        let e = self.one_phase_lost.clone().unwrap_or(RangeError::Unavailable);
        self.events.push(Event::Ended(Outcome::Unknown(e)));
    }

    /// Aborts; the answer comes as [`Event::Ended`].
    pub fn abort(&mut self) {
        match self.phase {
            Phase::Open | Phase::CommitUnknown => {}
            Phase::Committed(ts) => return self.events.push(Event::Ended(Outcome::Committed(ts))),
            Phase::Aborted => return self.events.push(Event::Ended(Outcome::Aborted)),
            Phase::Aborting | Phase::CommitWaiting(_) | Phase::Committing => return,
            // Committed: the abort gets the commit's answer once the clock is at it.
            Phase::ClockWait(_) => return,
            // It may have committed: an abort cannot undo it.
            Phase::OnePhaseUnknown => return self.repeat_lost(),
        }
        if self.record == RecordState::NotSent {
            self.phase = Phase::Aborted;
            self.events.push(Event::Ended(Outcome::Aborted));
            self.events.push(Event::Resolved);
            return;
        }
        self.start_abort();
    }

    fn start_abort(&mut self) {
        self.phase = Phase::Aborting;
        self.send_record(Pending::Abort);
    }

    /// The range of the record, once the first write sent it.
    fn record_range(&self) -> GroupId {
        self.record_g.expect("the record was sent with the first write")
    }

    /// Sends the heartbeat when it is due, the requests a range refused with `OutOfRange` where
    /// `route` puts their keys now, and resolutions that failed again; answers a commit whose mark
    /// the wall clock `wall_ms` has reached.
    pub fn tick(&mut self, route: &dyn Route, now: Nanos, wall_ms: u64) {
        self.wall_ms = self.wall_ms.max(wall_ms);
        if let Phase::ClockWait(ts) = self.phase
            && ts.physical <= self.wall_ms
        {
            self.ended(Outcome::Committed(ts));
        }
        let alive = matches!(
            self.phase,
            Phase::Open | Phase::CommitWaiting(_) | Phase::Committing | Phase::CommitUnknown
        );
        let beating = self
            .pending
            .values()
            .chain(&self.stalled)
            .any(|(_, p)| matches!(p, Pending::Heartbeat));
        if alive && self.record == RecordState::Created && !beating && self.heartbeat.due(now) {
            self.heartbeat.sent(now);
            self.send_record(Pending::Heartbeat);
        }
        for (g, p) in std::mem::take(&mut self.stalled) {
            self.reroute(route, g, p, Again::SameRange);
        }
        for g in std::mem::take(&mut self.retry_resolve) {
            self.resolve_in(route, g);
        }
        if std::mem::take(&mut self.retry_abort) {
            self.send_record(Pending::Abort);
        }
        self.try_commit();
    }

    fn final_state(&self) -> Option<(TxnStatus, Hlc)> {
        match self.phase {
            Phase::Committed(ts) => Some((TxnStatus::Committed, ts)),
            Phase::Aborted => Some((TxnStatus::Aborted, Hlc { physical: 0, logical: 0 })),
            Phase::Open
            | Phase::CommitWaiting(_)
            | Phase::Committing
            | Phase::CommitUnknown
            | Phase::OnePhaseUnknown
            | Phase::Aborting
            | Phase::ClockWait(_) => None,
        }
    }

    fn send_resolve(&mut self, g: GroupId) {
        let (status, commit_ts) =
            self.final_state().expect("resolutions start once the transaction is final");
        let keys = self.written.get(&g).expect("a range with intents").iter().cloned().collect();
        let op = TxnOp::ResolveIntents { txn: self.txn, status, commit_ts, keys };
        self.send(g, op, Pending::Resolve(g));
    }

    /// Resolves every range's intents once the transaction is final.
    fn start_resolving(&mut self) {
        let ranges: Vec<GroupId> = self.written.keys().copied().collect();
        if ranges.is_empty() {
            self.events.push(Event::Resolved);
            return;
        }
        for g in ranges {
            self.resolving.insert(g, 1);
            self.send_resolve(g);
        }
    }

    fn ended(&mut self, outcome: Outcome) {
        if let Outcome::Committed(ts) = outcome
            && ts.physical > self.wall_ms
        {
            self.phase = Phase::ClockWait(ts);
            return;
        }
        self.phase = match outcome {
            Outcome::Committed(ts) => Phase::Committed(ts),
            Outcome::Aborted | Outcome::Failed(_) => Phase::Aborted,
            Outcome::Unknown(_) => Phase::CommitUnknown,
        };
        let resolve = matches!(self.phase, Phase::Committed(_) | Phase::Aborted);
        self.events.push(Event::Ended(outcome));
        if resolve {
            self.start_resolving();
        }
    }

    /// The answer to request `id`.
    pub fn on_reply(&mut self, route: &dyn Route, id: ReqNo, r: Result<Reply, RangeError>) {
        let Some((g, p)) = self.pending.remove(&id) else { return };
        if matches!(r, Err(RangeError::OutOfRange))
            && !matches!(p, Pending::Resolve(_) | Pending::Decide)
        {
            self.reroute(route, g, p, Again::OtherRange);
            return self.try_commit();
        }
        match p {
            Pending::Begin => self.begin_done(r),
            Pending::Heartbeat => {
                if let Err(RangeError::NoSuchTransaction) = r {
                    self.doomed = Some(TxnError::NoSuchTransaction);
                }
            }
            Pending::Intents(w) => self.intents_done(route, w, r),
            Pending::Push(w, owner) => self.push_done(w, owner, r),
            Pending::ResolveOther(w) => match r {
                Ok(_) => self.send_write(w),
                Err(e) => self.write_done(w.stmt, Some(TxnError::Range(e))),
            },
            Pending::Commit(_) => self.commit_done(r),
            Pending::OnePhase(writes, clock) => self.one_phase_done((g, writes, clock), r),
            Pending::Decide => self.decide_done(r),
            Pending::Abort => self.abort_done(r),
            Pending::Resolve(g) => self.resolve_done(route, g, r),
            Pending::Check => self.check_done(r),
        }
        self.try_commit();
    }

    /// Sends again a request range `from` refused with `OutOfRange`, cut by the ranges `route`
    /// gives its keys now; it waits for the next tick while `route` puts every key in `from` or
    /// knows no range for one.
    fn reroute(&mut self, route: &dyn Route, from: GroupId, p: Pending, again: Again) {
        let record = record_key(self.txn);
        let keys: Vec<&[u8]> = match &p {
            Pending::Begin | Pending::Heartbeat | Pending::Commit(_) | Pending::Abort => {
                vec![&record]
            }
            Pending::Intents(w) | Pending::ResolveOther(w) => {
                w.writes.iter().map(|(k, _)| k.as_slice()).collect()
            }
            Pending::Push(_, owner) => vec![&owner.record_key],
            Pending::OnePhase(writes, _) => writes.iter().map(|(k, _)| k.as_slice()).collect(),
            Pending::Check => {
                return self.recheck(route, from, again);
            }
            Pending::Resolve(_) => unreachable!("a resolution is routed by resolve_in"),
            Pending::Decide => unreachable!("a decision is not routed again"),
        };
        let ranges: Option<BTreeSet<GroupId>> = keys.iter().map(|k| route.range_of(k)).collect();
        let ended = !matches!(self.phase, Phase::Open | Phase::CommitWaiting(_));
        if let (Pending::Intents(w) | Pending::ResolveOther(w) | Pending::Push(w, _), true) =
            (&p, ended)
        {
            // The transaction ended while the statement waited: its intents are not written.
            return self.write_done(w.stmt, Some(TxnError::NoSuchTransaction));
        }
        // Right after the refusal the request waits for the route to change (a split or a
        // merge moves its keys); at the next tick it goes again even to the same range, which
        // thaws if its merge was given up.
        let moved = |r: &BTreeSet<GroupId>| r.len() > 1 || !r.contains(&from);
        let Some(ranges) = ranges.filter(|r| again == Again::SameRange || moved(r)) else {
            return self.stalled.push((from, p));
        };
        match p {
            Pending::Begin | Pending::Heartbeat | Pending::Commit(_) | Pending::Abort => {
                self.record_g = ranges.first().copied();
                self.send_record(p);
            }
            Pending::Intents(w) | Pending::ResolveOther(w) => self.reroute_write(route, w),
            Pending::Push(w, owner) => {
                let g = *ranges.first().expect("the record's range");
                let push = Push::AbortIfAbandoned { intent_ts: owner.ts };
                self.send(g, TxnOp::PushTxn { txn: owner.txn, push }, Pending::Push(w, owner));
            }
            // An earlier try of a client transaction may be applied where its keys were, and a
            // split copied its record to both halves (FR-022): in one range the commit goes
            // again and that record answers it; in several, each says whether it committed
            // there and refuses it otherwise, so that it never commits twice.
            Pending::OnePhase(writes, clock) if self.one_phase_tried => {
                let places: Vec<(GroupId, Vec<u8>)> = ranges
                    .iter()
                    .filter_map(|g| {
                        let at = writes.iter().find(|(k, _)| route.range_of(k) == Some(*g));
                        at.map(|(k, _)| (*g, k.clone()))
                    })
                    .collect();
                if let [(g, _)] = places[..] {
                    return self.send_one_phase(g, writes, clock);
                }
                self.phase = Phase::Committing;
                self.deciding = places.len();
                self.one_phase_again = Some((from, writes, clock));
                let (lsid, txn_number) = (self.lsid, self.txn_number);
                for (g, at) in places {
                    self.send(g, TxnOp::DecideOnePhase { lsid, txn_number, at }, Pending::Decide);
                }
            }
            Pending::OnePhase(writes, clock) => {
                // Not applied: it starts again, in two phases if its keys are now in two ranges.
                self.phase = Phase::Open;
                self.write_and_commit(route, writes, clock);
            }
            Pending::Resolve(_) => unreachable!("a resolution is routed by resolve_in"),
            Pending::Check => unreachable!("rerouted by recheck"),
            Pending::Decide => unreachable!("a decision is not routed again"),
        }
    }

    /// Sends a statement's intents again, cut by the ranges `route` gives its keys, which it
    /// knows; the keys move to those ranges for the resolution.
    fn reroute_write(&mut self, route: &dyn Route, w: Write) {
        let mut by_range: BTreeMap<GroupId, Vec<(Vec<u8>, IntentOp)>> = BTreeMap::new();
        for (k, op) in w.writes {
            let g = route.range_of(&k).expect("every key has a range");
            if let Some(keys) = self.written.get_mut(&w.g) {
                keys.remove(&k);
            }
            self.written.entry(g).or_default().insert(k.clone());
            by_range.entry(g).or_default().push((k, op));
        }
        if self.written.get(&w.g).is_some_and(BTreeSet::is_empty) {
            self.written.remove(&w.g);
        }
        let s = self.stmts.get_mut(&w.stmt).expect("a statement with writes in flight");
        s.0 += by_range.len() - 1;
        for (g, writes) in by_range {
            self.send_write(Write { stmt: w.stmt, g, writes, rounds: w.rounds });
        }
    }

    /// Sends again the check range `from` refused with `OutOfRange` (a split or a merge moved its
    /// keys), cut by the ranges `route` gives those keys now; the keys move to those ranges for the
    /// resolution. While the route still puts them all in `from` the check waits for the next
    /// tick, as any refused request does; without a range for one of them the commit is unknown,
    /// and asked again.
    fn recheck(&mut self, route: &dyn Route, from: GroupId, again: Again) {
        let Checks::Waiting(left) = self.checks else { return };
        let keys: Vec<Vec<u8>> =
            self.written.get(&from).map(|k| k.iter().cloned().collect()).unwrap_or_default();
        let mut by_range: BTreeMap<GroupId, Vec<Vec<u8>>> = BTreeMap::new();
        for k in keys {
            let Some(g) = route.range_of(&k) else {
                self.checks = Checks::NotSent;
                return self.ended(Outcome::Unknown(RangeError::OutOfRange));
            };
            by_range.entry(g).or_default().push(k);
        }
        let unmoved = by_range.len() == 1 && by_range.contains_key(&from);
        if unmoved && again != Again::SameRange {
            return self.stalled.push((from, Pending::Check));
        }
        self.written.remove(&from);
        for (g, keys) in &by_range {
            self.written.entry(*g).or_default().extend(keys.iter().cloned());
        }
        self.checks = Checks::Waiting(left - 1 + by_range.len());
        for (g, keys) in by_range {
            self.send(g, TxnOp::CheckIntents { txn: self.txn, keys }, Pending::Check);
        }
    }

    fn begin_done(&mut self, r: Result<Reply, RangeError>) {
        match r {
            Ok(Reply::Record(rec)) if rec.status == TxnStatus::Pending => {
                self.record = RecordState::Created;
            }
            Ok(_) | Err(RangeError::NoSuchTransaction) => {
                self.record = RecordState::Created;
                self.doomed = Some(TxnError::NoSuchTransaction);
            }
            Err(e) => {
                self.record = RecordState::Created;
                self.doomed = Some(TxnError::Range(e));
            }
        }
        if let (Some(e), Phase::CommitWaiting(_)) = (self.doomed.clone(), self.phase) {
            self.start_abort();
            self.events.push(Event::Ended(Outcome::Failed(e)));
        }
    }

    fn intents_done(&mut self, route: &dyn Route, w: Write, r: Result<Reply, RangeError>) {
        match r {
            Ok(Reply::Intents { ts }) => {
                self.intent_max = self.intent_max.max(ts);
                self.write_done(w.stmt, None);
            }
            Err(RangeError::IntentConflict(owner)) if w.rounds < MAX_CONFLICT_ROUNDS => {
                let Some(g) = route.range_of(&owner.record_key) else {
                    return self.write_done(w.stmt, Some(TxnError::NoRange));
                };
                let push = Push::AbortIfAbandoned { intent_ts: owner.ts };
                let op = TxnOp::PushTxn { txn: owner.txn, push };
                self.send(g, op, Pending::Push(w, owner));
            }
            Err(RangeError::IntentConflict(_) | RangeError::WriteConflict) => {
                self.write_done(w.stmt, Some(TxnError::WriteConflict));
            }
            Ok(other) => {
                let e = RangeError::Mutation(teseladb_range::host::MutationError(format!(
                    "unexpected answer to PutIntents: {other:?}"
                )));
                self.write_done(w.stmt, Some(TxnError::Range(e)));
            }
            Err(e) => self.write_done(w.stmt, Some(TxnError::Range(e))),
        }
    }

    fn push_done(&mut self, mut w: Write, owner: IntentOwner, r: Result<Reply, RangeError>) {
        let rec: TxnRecord = match r {
            Ok(Reply::Record(rec)) => rec,
            Ok(_) => return self.write_done(w.stmt, Some(TxnError::WriteConflict)),
            Err(e) => return self.write_done(w.stmt, Some(TxnError::Range(e))),
        };
        if rec.status == TxnStatus::Pending {
            // Alive: the reference server answers WriteConflict and the driver repeats the
            // transaction.
            return self.write_done(w.stmt, Some(TxnError::WriteConflict));
        }
        w.rounds += 1;
        // The intent is on one of the statement's keys, in its range.
        let op = TxnOp::ResolveIntents {
            txn: owner.txn,
            status: rec.status,
            commit_ts: rec.commit_ts,
            keys: vec![owner.key.clone()],
        };
        let g = w.g;
        self.send(g, op, Pending::ResolveOther(w));
    }

    /// A range's answer to `CheckIntents` (FR-021).
    fn check_done(&mut self, r: Result<Reply, RangeError>) {
        let Checks::Waiting(left) = self.checks else { return };
        if !matches!(self.phase, Phase::CommitWaiting(_)) {
            return;
        }
        match r {
            Ok(Reply::Intents { ts }) => {
                self.intent_max = self.intent_max.max(ts);
                self.checks = if left <= 1 { Checks::Done } else { Checks::Waiting(left - 1) };
            }
            // A change of leader lost an intent: the transaction cannot commit what it wrote.
            Ok(_) | Err(RangeError::NoSuchTransaction) => {
                self.checks = Checks::NotSent;
                self.start_abort();
                self.events.push(Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)));
            }
            // Nothing committed yet: the driver asks again, and the commit checks again.
            Err(e) => {
                self.checks = Checks::NotSent;
                self.ended(Outcome::Unknown(e));
            }
        }
    }

    fn commit_done(&mut self, r: Result<Reply, RangeError>) {
        match r {
            Ok(Reply::Record(rec)) if rec.status == TxnStatus::Committed => {
                self.ended(Outcome::Committed(rec.commit_ts));
            }
            Ok(_) | Err(RangeError::NoSuchTransaction) => {
                self.ended(Outcome::Failed(TxnError::NoSuchTransaction));
            }
            // The entry may still be applied: the driver asks again.
            Err(e) => self.ended(Outcome::Unknown(e)),
        }
    }

    fn one_phase_done(
        &mut self,
        (g, writes, clock): (GroupId, Vec<(Vec<u8>, IntentOp)>, Hlc),
        r: Result<Reply, RangeError>,
    ) {
        match r {
            Ok(Reply::Intents { ts }) => self.ended(Outcome::Committed(ts)),
            Ok(_) | Err(RangeError::IntentConflict(_) | RangeError::WriteConflict) => {
                self.ended(Outcome::Failed(TxnError::WriteConflict));
            }
            // A recovery from another node refused the commit before it came (FR-022).
            Err(RangeError::NoSuchTransaction) => {
                self.ended(Outcome::Failed(TxnError::NoSuchTransaction));
            }
            Err(e) => {
                self.phase = Phase::OnePhaseUnknown;
                self.one_phase_lost = Some(e.clone());
                if self.lsid != 0 {
                    self.one_phase_tried = true;
                    self.one_phase_again = Some((g, writes, clock));
                }
                self.events.push(Event::Ended(Outcome::Unknown(e)));
            }
        }
    }

    /// A range's answer to `DecideOnePhase`, asked after a one-phase commit whose answer was
    /// lost and whose keys are in several ranges now (FR-022).
    fn decide_done(&mut self, r: Result<Reply, RangeError>) {
        if self.phase != Phase::Committing || self.deciding == 0 {
            return;
        }
        match r {
            // It committed there.
            Ok(Reply::Intents { ts }) => {
                self.deciding = 0;
                self.one_phase_again = None;
                self.ended(Outcome::Committed(ts));
            }
            // Refused there: once every range refused it, it never committed.
            Ok(Reply::Batch(_)) | Err(RangeError::NoSuchTransaction) => {
                self.deciding -= 1;
                if self.deciding == 0 {
                    self.one_phase_again = None;
                    self.ended(Outcome::Failed(TxnError::NoSuchTransaction));
                }
            }
            // Still unknown: a commit asked again asks the ranges again.
            Ok(_) | Err(_) => {
                self.deciding = 0;
                let e = r.err().unwrap_or(RangeError::Unavailable);
                self.phase = Phase::OnePhaseUnknown;
                self.one_phase_lost = Some(e.clone());
                self.events.push(Event::Ended(Outcome::Unknown(e)));
            }
        }
    }

    fn abort_done(&mut self, r: Result<Reply, RangeError>) {
        match r {
            Ok(Reply::Record(rec)) if rec.status == TxnStatus::Committed => {
                self.ended(Outcome::Committed(rec.commit_ts));
            }
            Ok(_) | Err(RangeError::NoSuchTransaction) => self.ended(Outcome::Aborted),
            // No answer: the abort goes again at the next tick. Intents whose answers were lost
            // may be in their ranges while the record, if a push created it, has none of their
            // intervals; the reaper aborts an abandoned record but only resolves those intervals.
            Err(_) if self.abort_attempts + 1 < MAX_RESOLVE_ATTEMPTS => {
                self.abort_attempts += 1;
                self.retry_abort = true;
            }
            // The record may stay pending: the reaper aborts it once it has no heartbeat.
            Err(_) => self.ended(Outcome::Aborted),
        }
    }

    /// Sends the resolution of range `g`'s keys, cut by the ranges `route` gives them now.
    fn resolve_in(&mut self, route: &dyn Route, g: GroupId) {
        let keys = self.written.remove(&g).expect("a range with intents");
        let mut stay = BTreeSet::new();
        let mut moved: BTreeMap<GroupId, BTreeSet<Vec<u8>>> = BTreeMap::new();
        for k in keys {
            match route.range_of(&k) {
                Some(to) if to != g => moved.entry(to).or_default().insert(k),
                Some(_) | None => stay.insert(k),
            };
        }
        for (to, keys) in moved {
            self.written.entry(to).or_default().extend(keys);
            if let std::collections::btree_map::Entry::Vacant(e) = self.resolving.entry(to) {
                e.insert(1);
                self.send_resolve(to);
            } else {
                self.resolve_again.insert(to);
            }
        }
        if stay.is_empty() {
            self.resolving.remove(&g);
            self.resolve_again.remove(&g);
        } else {
            self.written.insert(g, stay);
            self.send_resolve(g);
        }
    }

    fn resolve_done(&mut self, route: &dyn Route, g: GroupId, r: Result<Reply, RangeError>) {
        let attempts = self.resolving.get(&g).copied().unwrap_or(MAX_RESOLVE_ATTEMPTS);
        if r.is_err() && attempts < MAX_RESOLVE_ATTEMPTS {
            self.resolving.insert(g, attempts + 1);
            self.resolve_again.remove(&g);
            let moved =
                self.written[&g].iter().any(|k| route.range_of(k).is_some_and(|to| to != g));
            if matches!(r, Err(RangeError::OutOfRange)) && moved {
                self.resolve_in(route, g);
            } else {
                self.retry_resolve.insert(g);
            }
            return;
        }
        if r.is_ok() && self.resolve_again.remove(&g) {
            return self.send_resolve(g);
        }
        self.resolve_again.remove(&g);
        self.resolving.remove(&g);
        if self.resolving.is_empty() {
            self.events.push(Event::Resolved);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_range::intents::TxnRecord;
    use teseladb_types::NodeId;

    const G1: GroupId = GroupId(1);
    const G2: GroupId = GroupId(2);

    /// Keys starting with `a` in G1, the rest in G2; `x` keys in none.
    struct Two;

    impl Route for Two {
        fn range_of(&self, key: &[u8]) -> Option<GroupId> {
            match key.first() {
                Some(b'a') => Some(G1),
                Some(b'x') => None,
                _ => Some(G2),
            }
        }
    }

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    fn put() -> IntentOp {
        IntentOp::Put { value_type: 1, value: vec![1] }
    }

    fn rec(status: TxnStatus, commit: u64) -> TxnRecord {
        TxnRecord {
            txn: TxnId(1),
            lsid: 0,
            txn_number: 0,
            status,
            read_ts: ts(1),
            min_commit_ts: ts(0),
            commit_ts: ts(commit),
            heartbeat_ts: ts(1),
            spans: vec![],
        }
    }

    /// The wall clock of the coordinators of these tests: above every mark they commit at.
    const WALL: u64 = 100;

    fn coord() -> Coordinator {
        let mut c = Coordinator::new(TxnId(1), (7, 1), ts(1), Nanos::ZERO);
        c.tick(&Two, Nanos::ZERO, WALL);
        c
    }

    /// Answers every request with `f` until none is left.
    fn answer_all(c: &mut Coordinator, f: impl FnMut(&Request) -> Result<Reply, RangeError>) {
        answer_on(c, &Two, f);
    }

    /// Answers every request with `f`, the coordinator routing with `route`, until none is left.
    fn answer_on(
        c: &mut Coordinator,
        route: &dyn Route,
        mut f: impl FnMut(&Request) -> Result<Reply, RangeError>,
    ) {
        for _ in 0..50 {
            let reqs = c.take_requests();
            if reqs.is_empty() {
                return;
            }
            for r in reqs {
                let a = f(&r);
                c.on_reply(route, r.id, a);
            }
        }
        panic!("the coordinator never stopped asking");
    }

    fn ok_for(r: &Request, record: &TxnRecord) -> Result<Reply, RangeError> {
        match &r.op {
            TxnOp::EndTxn { op: RecordOp::Commit { ts, .. }, .. } => Ok(Reply::Record(TxnRecord {
                status: TxnStatus::Committed,
                commit_ts: *ts,
                ..record.clone()
            })),
            TxnOp::EndTxn { op: RecordOp::Abort { .. }, .. } => {
                Ok(Reply::Record(TxnRecord { status: TxnStatus::Aborted, ..record.clone() }))
            }
            TxnOp::EndTxn { .. } | TxnOp::PushTxn { .. } => Ok(Reply::Record(record.clone())),
            TxnOp::PutIntents { .. }
            | TxnOp::ResolveIntents { .. }
            | TxnOp::OnePhase { .. }
            | TxnOp::LockKeys { .. }
            | TxnOp::UnlockKeys { .. }
            | TxnOp::DecideOnePhase { .. }
            | TxnOp::CheckIntents { .. } => Ok(Reply::Intents { ts: ts(5) }),
            TxnOp::FindIntents { .. }
            | TxnOp::ForgetRecord { .. }
            | TxnOp::ReadRecord { .. }
            | TxnOp::PushByRead { .. } => {
                panic!("only the reaper sends {:?}", r.op)
            }
        }
    }

    #[test]
    fn statements_without_keys_or_ranges_and_doomed_transactions() {
        let mut c = coord();
        assert_eq!(c.txn(), TxnId(1));
        assert_eq!(c.write(&Two, vec![(b"x1".to_vec(), put())]), Err(TxnError::NoRange));
        let s = c.write(&Two, vec![]).unwrap();
        assert!(c.take_events().contains(&Event::Written { stmt: s, result: Ok(()) }));
        // The record was refused: the transaction is doomed.
        answer_all(&mut c, |_| Err(RangeError::NoSuchTransaction));
        assert_eq!(c.write(&Two, vec![(b"a".to_vec(), put())]), Err(TxnError::NoSuchTransaction));
        c.commit(ts(9));
        assert!(
            c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)))
        );
        // A record range that is unknown.
        struct Nowhere;
        impl Route for Nowhere {
            fn range_of(&self, k: &[u8]) -> Option<GroupId> {
                (k.first() == Some(&b'a')).then_some(G1)
            }
        }
        let mut c = coord();
        assert_eq!(c.write(&Nowhere, vec![(b"a".to_vec(), put())]), Err(TxnError::NoRange));
    }

    #[test]
    fn a_record_that_fails_while_the_commit_waits_ends_it() {
        for err in [RangeError::Timeout, RangeError::NoSuchTransaction] {
            let mut c = coord();
            c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
            c.commit(ts(9));
            let record = rec(TxnStatus::Pending, 0);
            let e = err.clone();
            answer_all(&mut c, |r| match r.op {
                TxnOp::EndTxn { op: RecordOp::Begin { .. }, .. } => Err(e.clone()),
                _ => ok_for(r, &record),
            });
            let ended: Vec<Event> =
                c.take_events().into_iter().filter(|e| matches!(e, Event::Ended(_))).collect();
            assert!(matches!(ended[0], Event::Ended(Outcome::Failed(_))), "{ended:?}");
        }
        // A begin answered with a final record dooms it too.
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |r| match r.op {
            TxnOp::EndTxn { .. } => Ok(Reply::Record(rec(TxnStatus::Aborted, 0))),
            _ => ok_for(r, &rec(TxnStatus::Pending, 0)),
        });
        c.commit(ts(9));
        assert!(c.take_events().iter().any(|e| matches!(e, Event::Ended(Outcome::Failed(_)))));
    }

    #[test]
    fn heartbeats_go_when_due_and_an_aborted_record_dooms_the_transaction() {
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        let record = rec(TxnStatus::Pending, 0);
        answer_all(&mut c, |r| ok_for(r, &record));
        c.tick(&Two, Nanos::from_millis(10), 0);
        assert!(c.take_requests().is_empty(), "not due yet");
        c.tick(&Two, Nanos::from_secs(2), 0);
        let beat = c.take_requests();
        assert!(matches!(beat[0].op, TxnOp::EndTxn { op: RecordOp::Heartbeat { .. }, .. }));
        // While one is in flight no other goes.
        c.tick(&Two, Nanos::from_secs(4), 0);
        assert!(c.take_requests().is_empty());
        c.on_reply(&Two, beat[0].id, Err(RangeError::NoSuchTransaction));
        assert_eq!(c.write(&Two, vec![(b"b".to_vec(), put())]), Err(TxnError::NoSuchTransaction));
        // An answer nobody asked for is ignored.
        c.on_reply(&Two, 999, Err(RangeError::Timeout));
    }

    #[test]
    fn conflicts_push_resolve_and_give_up() {
        let owner =
            IntentOwner { txn: TxnId(2), record_key: b"r".to_vec(), key: b"a".to_vec(), ts: ts(3) };
        // (answer to the push, the statement's result)
        let cases: [(Result<Reply, RangeError>, Result<(), TxnError>); 4] = [
            (Ok(Reply::Record(rec(TxnStatus::Pending, 0))), Err(TxnError::WriteConflict)),
            (Ok(Reply::Intents { ts: ts(1) }), Err(TxnError::WriteConflict)),
            (Err(RangeError::Timeout), Err(TxnError::Range(RangeError::Timeout))),
            (Ok(Reply::Record(rec(TxnStatus::Committed, 4))), Ok(())),
        ];
        for (push, want) in cases {
            let mut c = coord();
            let s = c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
            let mut conflicted = false;
            let record = rec(TxnStatus::Pending, 0);
            answer_all(&mut c, |r| match &r.op {
                TxnOp::PutIntents { .. } if !conflicted => {
                    conflicted = true;
                    Err(RangeError::IntentConflict(owner.clone()))
                }
                TxnOp::PushTxn { .. } => push.clone(),
                _ => ok_for(r, &record),
            });
            assert!(c.take_events().contains(&Event::Written { stmt: s, result: want.clone() }));
        }
        // The owner's record in no range; a failed resolution; a conflict that never ends.
        let nowhere = IntentOwner { record_key: b"x".to_vec(), ..owner.clone() };
        let mut c = coord();
        let s = c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        let record = rec(TxnStatus::Pending, 0);
        answer_all(&mut c, |r| match r.op {
            TxnOp::PutIntents { .. } => Err(RangeError::IntentConflict(nowhere.clone())),
            _ => ok_for(r, &record),
        });
        assert!(
            c.take_events().contains(&Event::Written { stmt: s, result: Err(TxnError::NoRange) })
        );
        for resolve in [Err(RangeError::Timeout), Ok(Reply::Intents { ts: ts(1) })] {
            let mut c = coord();
            let s = c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
            answer_all(&mut c, |r| match r.op {
                TxnOp::PutIntents { .. } => Err(RangeError::IntentConflict(owner.clone())),
                TxnOp::PushTxn { .. } => Ok(Reply::Record(rec(TxnStatus::Aborted, 0))),
                TxnOp::ResolveIntents { .. } => resolve.clone(),
                _ => ok_for(r, &record),
            });
            let events = c.take_events();
            let result = events.iter().find_map(|e| match e {
                Event::Written { stmt, result } if *stmt == s => Some(result.clone()),
                _ => None,
            });
            assert!(matches!(result, Some(Err(_))), "{events:?}");
        }
        // Other answers to PutIntents.
        for (answer, want) in [
            (Err(RangeError::WriteConflict), Err(TxnError::WriteConflict)),
            (
                Ok(Reply::Record(rec(TxnStatus::Pending, 0))),
                Err(TxnError::Range(RangeError::Mutation(teseladb_range::host::MutationError(
                    format!(
                        "unexpected answer to PutIntents: {:?}",
                        Reply::Record(rec(TxnStatus::Pending, 0))
                    ),
                )))),
            ),
            (Err(RangeError::Overloaded), Err(TxnError::Range(RangeError::Overloaded))),
        ] {
            let mut c = coord();
            let s = c.write(&Two, vec![(b"a".to_vec(), put()), (b"b".to_vec(), put())]).unwrap();
            let record = rec(TxnStatus::Pending, 0);
            answer_all(&mut c, |r| match r.op {
                TxnOp::PutIntents { .. } => answer.clone(),
                _ => ok_for(r, &record),
            });
            assert!(c.take_events().contains(&Event::Written { stmt: s, result: want }));
        }
    }

    #[test]
    fn commits_and_aborts_with_every_answer() {
        let record = rec(TxnStatus::Pending, 0);
        let written = |c: &mut Coordinator| {
            c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
            answer_all(c, |r| ok_for(r, &record));
            c.take_events();
        };
        // A commit whose answer is lost is unknown, and asked again.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(9));
        answer_all(&mut c, |_| Err(RangeError::Timeout));
        assert!(c.take_events().contains(&Event::Ended(Outcome::Unknown(RangeError::Timeout))));
        c.commit(ts(9));
        answer_all(&mut c, |r| ok_for(r, &record));
        assert!(c.take_events().contains(&Event::Ended(Outcome::Committed(ts(9)))));
        // A commit answered with a pending record failed; a second commit or abort says so.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(9));
        c.commit(ts(9));
        answer_all(&mut c, |_| Ok(Reply::Record(record.clone())));
        assert!(
            c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)))
        );
        c.abort();
        c.commit(ts(9));
        let events = c.take_events();
        assert!(events.contains(&Event::Ended(Outcome::Aborted)), "{events:?}");
        // An abort that finds it committed, and one whose answer is lost.
        // The lost one goes again at the next tick.
        for (answer, want) in [
            (Ok(Reply::Record(rec(TxnStatus::Committed, 7))), Outcome::Committed(ts(7))),
            (Err(RangeError::Timeout), Outcome::Aborted),
        ] {
            let mut c = coord();
            written(&mut c);
            c.abort();
            c.abort();
            let mut first = true;
            answer_all(&mut c, |r| match r.op {
                TxnOp::EndTxn { .. } if first => {
                    first = false;
                    answer.clone()
                }
                _ => ok_for(r, &record),
            });
            c.tick(&Two, Nanos::ZERO, WALL);
            answer_all(&mut c, |r| ok_for(r, &record));
            assert!(c.take_events().contains(&Event::Ended(want)));
        }
        // Resolutions that fail are sent again at the next tick, up to their limit.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(9));
        let mut resolves = 0;
        for _ in 0..MAX_RESOLVE_ATTEMPTS + 2 {
            answer_all(&mut c, |r| match r.op {
                TxnOp::ResolveIntents { .. } => {
                    resolves += 1;
                    Err(RangeError::Timeout)
                }
                _ => ok_for(r, &record),
            });
            c.tick(&Two, Nanos::ZERO, 0);
        }
        assert_eq!(resolves, MAX_RESOLVE_ATTEMPTS);
        assert!(c.take_events().contains(&Event::Resolved));
    }

    #[test]
    fn one_phase_needs_a_fresh_transaction_and_ticks_need_a_live_record() {
        let record = rec(TxnStatus::Pending, 0);
        // A tick before any write sends nothing.
        let mut c = coord();
        c.tick(&Two, Nanos::from_secs(9), 0);
        assert!(c.take_requests().is_empty());
        // After a write, one batch goes as a statement and a commit.
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |r| ok_for(r, &record));
        c.write_and_commit(&Two, vec![(b"b".to_vec(), put())], ts(9));
        let reqs = c.take_requests();
        assert!(matches!(reqs[0].op, TxnOp::PutIntents { .. }), "{reqs:?}");
        for r in reqs {
            let a = ok_for(&r, &record);
            c.on_reply(&Two, r.id, a);
        }
        answer_all(&mut c, |r| ok_for(r, &record));
        assert!(c.take_events().iter().any(|e| matches!(e, Event::Ended(Outcome::Committed(_)))));
        // Ended: no heartbeat, and no new batch.
        c.tick(&Two, Nanos::from_secs(30), 0);
        assert!(c.take_requests().iter().all(|r| !matches!(r.op, TxnOp::EndTxn { .. })));
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        assert!(
            c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)))
        );
        // Doomed before the batch: it fails.
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |_| Err(RangeError::NoSuchTransaction));
        c.take_events();
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        assert!(
            c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction)))
        );
    }

    #[test]
    fn one_phase_commits_and_their_lost_answers() {
        // Two ranges, or after a write: the usual path; a key without a range fails it.
        let mut c = coord();
        c.write_and_commit(&Two, vec![(b"x".to_vec(), put())], ts(9));
        assert!(c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::NoRange))));
        let mut c = coord();
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        answer_all(&mut c, |_| Err(RangeError::Timeout));
        let lost = Event::Ended(Outcome::Unknown(RangeError::Timeout));
        assert!(c.take_events().contains(&lost));
        // An abort cannot undo what may be committed: it gets the same answer.
        c.abort();
        assert_eq!(c.take_events(), std::slice::from_ref(&lost));
        // A client transaction's commit asked again goes again with its session, and the record
        // the first one may have left answers it (FR-022).
        c.commit(ts(9));
        let again = c.take_requests();
        let [Request { id, op: TxnOp::OnePhase { session: Some((7, 1)), .. }, .. }] = &again[..]
        else {
            panic!("{again:?}")
        };
        c.on_reply(&Two, *id, Ok(Reply::Intents { ts: ts(12) }));
        assert_eq!(c.take_events(), [Event::Ended(Outcome::Committed(ts(12))), Event::Resolved]);
        // A recovery from another node refused it first: the transaction never committed.
        let mut c = coord();
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        answer_all(&mut c, |_| Err(RangeError::NoSuchTransaction));
        let refused = Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction));
        assert!(c.take_events().contains(&refused));
        // A transaction of the node itself has no session: its lost commit stays unknown.
        let mut c = Coordinator::new(TxnId(1), (0, 0), ts(1), Nanos::ZERO);
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        answer_all(&mut c, |_| Err(RangeError::Timeout));
        c.take_events();
        c.commit(ts(9));
        assert!(c.take_requests().is_empty());
        assert_eq!(c.take_events(), std::slice::from_ref(&lost));
        let mut c = coord();
        c.write_and_commit(&Two, vec![(b"a".to_vec(), put())], ts(9));
        answer_all(&mut c, |_| Ok(Reply::Record(rec(TxnStatus::Pending, 0))));
        assert!(c.take_events().contains(&Event::Ended(Outcome::Failed(TxnError::WriteConflict))));
    }

    const G3: GroupId = GroupId(3);
    const G4: GroupId = GroupId(4);

    /// `Two` after splits: keys from `a2` on in G3, the records in G4.
    struct Cut;

    impl Route for Cut {
        fn range_of(&self, key: &[u8]) -> Option<GroupId> {
            match key {
                [2, ..] => Some(G4),
                [b'a', b'2'..=b'9', ..] => Some(G3),
                _ => Two.range_of(key),
            }
        }
    }

    /// A routing table that knows no range.
    struct Blank;

    impl Route for Blank {
        fn range_of(&self, _: &[u8]) -> Option<GroupId> {
            None
        }
    }

    fn out_of_range(_: &Request) -> Result<Reply, RangeError> {
        Err(RangeError::OutOfRange)
    }

    fn ranges(reqs: &[Request]) -> BTreeSet<GroupId> {
        reqs.iter().map(|r| r.g).collect()
    }

    /// A range answers a statement once its intents are in the leader's log (FR-021, D-160): the
    /// commit first asks each range written in whether they were applied.
    #[test]
    fn the_commit_checks_the_intents_were_applied_before_it_commits() {
        let record = rec(TxnStatus::Pending, 0);
        let written = |c: &mut Coordinator| {
            c.write(&Two, vec![(b"a1".to_vec(), put()), (b"b1".to_vec(), put())]).unwrap();
            answer_all(c, |r| ok_for(r, &record));
            c.take_events();
        };
        let only = |reqs: &[Request], check: bool| {
            reqs.iter().all(|r| matches!(r.op, TxnOp::CheckIntents { .. }) == check)
        };
        // Applied in both ranges: then the commit, at no less than their marks.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(3));
        let checks = c.take_requests();
        assert_eq!(ranges(&checks), BTreeSet::from([G1, G2]));
        assert!(only(&checks, true), "{checks:?}");
        for r in checks {
            c.on_reply(&Two, r.id, Ok(Reply::Intents { ts: ts(7) }));
        }
        let commit = c.take_requests();
        assert!(only(&commit, false) && commit.len() == 1, "{commit:?}");
        let TxnOp::EndTxn { op: RecordOp::Commit { ts: at, .. }, .. } = commit[0].op else {
            panic!("{commit:?}")
        };
        assert_eq!(at, ts(7));
        // A change of leader lost one: the transaction aborts and the commit fails.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(3));
        for r in c.take_requests() {
            let a = match r.g {
                G1 => Err(RangeError::NoSuchTransaction),
                _ => Ok(Reply::Intents { ts: ts(5) }),
            };
            c.on_reply(&Two, r.id, a);
        }
        let failed = Event::Ended(Outcome::Failed(TxnError::NoSuchTransaction));
        assert!(c.take_events().contains(&failed));
        let abort = c.take_requests();
        assert!(
            abort.iter().any(|r| matches!(r.op, TxnOp::EndTxn { op: RecordOp::Abort { .. }, .. }))
        );
        // No answer: the commit is unknown, and the driver's next commit checks again.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(3));
        answer_all(&mut c, |_| Err(RangeError::Timeout));
        assert!(c.take_events().contains(&Event::Ended(Outcome::Unknown(RangeError::Timeout))));
        c.commit(ts(3));
        assert!(only(&c.take_requests(), true), "it checks again");
        // Refused with `OutOfRange` while the route still names the range: the check waits for
        // the next tick instead of going back at once, and then goes again.
        let mut c = coord();
        written(&mut c);
        c.commit(ts(3));
        let checks = c.take_requests();
        let g1 = checks.iter().find(|r| r.g == G1).expect("a check in G1").id;
        c.on_reply(&Two, g1, Err(RangeError::OutOfRange));
        assert!(c.take_requests().is_empty(), "no request while the route did not move");
        c.tick(&Two, Nanos::ZERO, WALL);
        let again = c.take_requests();
        assert!(again.len() == 1 && again[0].g == G1 && only(&again, true), "{again:?}");
    }

    #[test]
    fn requests_refused_by_a_split_wait_for_the_route_and_go_to_the_new_ranges() {
        let record = rec(TxnStatus::Pending, 0);
        let mut c = coord();
        let s = c.write(&Two, vec![(b"a1".to_vec(), put()), (b"a2".to_vec(), put())]).unwrap();
        // Refused while the route still names the same ranges, or none: nothing goes.
        answer_on(&mut c, &Two, out_of_range);
        c.tick(&Blank, Nanos::ZERO, 0);
        assert!(c.take_requests().is_empty());
        // The commit waits for the statement.
        c.commit(ts(9));
        assert!(c.take_requests().is_empty());
        // The route learns the splits: the record goes to G4, the statement to G1 and G3.
        c.tick(&Cut, Nanos::ZERO, 0);
        let reqs = c.take_requests();
        assert_eq!(ranges(&reqs), BTreeSet::from([G1, G3, G4]));
        for r in reqs {
            let a = ok_for(&r, &record);
            c.on_reply(&Cut, r.id, a);
        }
        assert!(c.take_events().contains(&Event::Written { stmt: s, result: Ok(()) }));
        // Before the commit, each range written in says its intents are applied (FR-021).
        let checks = c.take_requests();
        assert_eq!(ranges(&checks), BTreeSet::from([G1, G3]));
        assert!(checks.iter().all(|r| matches!(r.op, TxnOp::CheckIntents { .. })));
        for r in checks {
            let a = ok_for(&r, &record);
            c.on_reply(&Cut, r.id, a);
        }
        // The commit went to G4; refused there, the route of `Two` sends it back to G2.
        let commit = c.take_requests();
        assert_eq!(ranges(&commit), BTreeSet::from([G4]));
        c.on_reply(&Two, commit[0].id, Err(RangeError::OutOfRange));
        let commit = c.take_requests();
        assert_eq!(ranges(&commit), BTreeSet::from([G2]));
        let a = ok_for(&commit[0], &record);
        c.on_reply(&Cut, commit[0].id, a);
        assert!(c.take_events().iter().any(|e| matches!(e, Event::Ended(Outcome::Committed(_)))));
        // G3 refuses its resolution and `Two` puts `a2` back in G1, whose resolution is in
        // flight: G1 is resolved again with both keys once it answers.
        let resolves = c.take_requests();
        assert_eq!(ranges(&resolves), BTreeSet::from([G1, G3]));
        let (g1, g3): (Vec<Request>, Vec<Request>) = resolves.into_iter().partition(|r| r.g == G1);
        c.on_reply(&Two, g3[0].id, Err(RangeError::OutOfRange));
        assert!(c.take_requests().is_empty());
        c.on_reply(&Two, g1[0].id, Ok(Reply::Intents { ts: ts(9) }));
        let again = c.take_requests();
        let TxnOp::ResolveIntents { keys, .. } = &again[0].op else { panic!("{again:?}") };
        assert_eq!((again[0].g, keys.len()), (G1, 2));
        c.on_reply(&Two, again[0].id, Ok(Reply::Intents { ts: ts(9) }));
        assert!(c.take_events().contains(&Event::Resolved));
    }

    #[test]
    fn resolutions_follow_their_keys_and_wait_for_a_route_that_moved_none() {
        let record = rec(TxnStatus::Pending, 0);
        let mut c = coord();
        c.write(&Two, vec![(b"a1".to_vec(), put()), (b"a2".to_vec(), put())]).unwrap();
        answer_all(&mut c, |r| ok_for(r, &record));
        c.abort();
        answer_all(&mut c, |r| match r.op {
            TxnOp::EndTxn { .. } => ok_for(r, &record),
            _ => Err(RangeError::OutOfRange),
        });
        // Refused while `Two` still puts both keys in G1: it goes again at the next tick, where
        // `Cut` sends `a2` to G3 and keeps `a1` in G1.
        c.tick(&Cut, Nanos::ZERO, 0);
        let reqs = c.take_requests();
        assert_eq!(ranges(&reqs), BTreeSet::from([G1, G3]));
        // G1 refuses again with both keys gone from it per `Cut2`: none stays.
        struct Cut2;
        impl Route for Cut2 {
            fn range_of(&self, key: &[u8]) -> Option<GroupId> {
                Some(if key.first() == Some(&b'a') { G3 } else { G2 })
            }
        }
        for r in reqs {
            let a = if r.g == G1 { Err(RangeError::OutOfRange) } else { ok_for(&r, &record) };
            c.on_reply(&Cut2, r.id, a);
        }
        // G3 had answered: `a1` goes there now.
        let reqs = c.take_requests();
        assert_eq!(ranges(&reqs), BTreeSet::from([G3]));
        let a = ok_for(&reqs[0], &record);
        c.on_reply(&Cut2, reqs[0].id, a);
        assert!(c.take_events().contains(&Event::Resolved));
    }

    #[test]
    fn pushes_one_phase_commits_and_late_statements_after_a_split() {
        let record = rec(TxnStatus::Pending, 0);
        let owner = IntentOwner {
            txn: TxnId(2),
            record_key: record_key(TxnId(2)),
            key: b"a2".to_vec(),
            ts: ts(1),
        };
        // The push of the owner of an intent goes where its record moved; so does the write
        // after the resolution of that intent, refused too.
        let mut c = coord();
        let s = c.write(&Two, vec![(b"a2".to_vec(), put())]).unwrap();
        let mut conflicted = false;
        let mut pushed = Vec::new();
        answer_on(&mut c, &Two, |r| match &r.op {
            TxnOp::PutIntents { .. } if !conflicted => {
                conflicted = true;
                Err(RangeError::IntentConflict(owner.clone()))
            }
            TxnOp::PushTxn { .. } => {
                pushed.push(r.g);
                Err(RangeError::OutOfRange)
            }
            _ => ok_for(r, &record),
        });
        c.tick(&Cut, Nanos::ZERO, 0);
        let mut resolved_other = false;
        answer_on(&mut c, &Cut, |r| match &r.op {
            TxnOp::PushTxn { .. } => {
                pushed.push(r.g);
                Ok(Reply::Record(rec(TxnStatus::Aborted, 0)))
            }
            TxnOp::ResolveIntents { .. } if !resolved_other => {
                resolved_other = true;
                Err(RangeError::OutOfRange)
            }
            _ => ok_for(r, &record),
        });
        assert_eq!(pushed, [G2, G4]);
        assert!(c.take_events().contains(&Event::Written { stmt: s, result: Ok(()) }));
        // A push refused while the route still names the same range waits for the route.
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_on(&mut c, &Two, |r| match &r.op {
            TxnOp::PutIntents { .. } => Err(RangeError::IntentConflict(owner.clone())),
            TxnOp::PushTxn { .. } => Err(RangeError::OutOfRange),
            _ => ok_for(r, &record),
        });
        assert!(c.take_requests().is_empty());
        // A statement still waiting when the transaction aborts is not written.
        c.abort();
        answer_all(&mut c, |r| ok_for(r, &record));
        c.tick(&Cut, Nanos::ZERO, 0);
        assert!(
            c.take_events()
                .contains(&Event::Written { stmt: 1, result: Err(TxnError::NoSuchTransaction) })
        );
        // A one-phase commit refused by a split: it waits for the route, then commits in two
        // phases since its keys are now in two ranges.
        let mut c = coord();
        c.write_and_commit(&Two, vec![(b"a1".to_vec(), put()), (b"a2".to_vec(), put())], ts(9));
        answer_on(&mut c, &Two, out_of_range);
        assert!(c.take_requests().is_empty());
        c.tick(&Cut, Nanos::ZERO, 0);
        let mut sent = Vec::new();
        answer_on(&mut c, &Cut, |r| {
            sent.push(r.g);
            ok_for(r, &record)
        });
        assert_eq!(BTreeSet::from_iter(sent), BTreeSet::from([G1, G3, G4]));
        assert!(c.take_events().iter().any(|e| matches!(e, Event::Ended(Outcome::Committed(_)))));
    }

    #[test]
    fn a_transaction_is_too_large_only_past_the_memory_budget() {
        let record = rec(TxnStatus::Pending, 0);
        // Two million keys in one statement: no limit on their number.
        let mut c = coord();
        let keys: Vec<(Vec<u8>, IntentOp)> = (0..2_000_000u32)
            .map(|i| ([b"a".as_slice(), &i.to_be_bytes()].concat(), put()))
            .collect();
        let s = c.write(&Two, keys).unwrap();
        answer_all(&mut c, |r| ok_for(r, &record));
        assert!(c.take_events().contains(&Event::Written { stmt: s, result: Ok(()) }));
        // Past the budget: `TooLarge`, the transaction is doomed and its commit finds none.
        let mut c = coord().with_budget(16);
        assert!(c.write(&Two, vec![(b"a1".to_vec(), put())]).is_ok());
        let big = vec![(b"a-long-key".to_vec(), put())];
        assert_eq!(c.write(&Two, big), Err(TxnError::TooLarge));
        answer_all(&mut c, |r| ok_for(r, &record));
        c.commit(ts(9));
        answer_all(&mut c, |r| ok_for(r, &record));
        let ended = c.take_events();
        assert!(ended.contains(&Event::Ended(Outcome::Failed(TxnError::TooLarge))), "{ended:?}");
        assert_eq!(
            crate::commit_retry::commit_answer(&Outcome::Failed(TxnError::TooLarge)),
            crate::commit_retry::CommitAnswer::NoSuchTransaction
        );
    }

    #[test]
    fn a_commit_above_the_clock_answers_once_the_clock_reaches_it() {
        let record = rec(TxnStatus::Pending, 0);
        let mut c = Coordinator::new(TxnId(1), (7, 1), ts(1), Nanos::ZERO);
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |r| ok_for(r, &record));
        c.take_events();
        // The clock reads 9 and the record commits at 300 (a new leader's cache).
        c.commit(ts(9));
        answer_all(&mut c, |r| match &r.op {
            TxnOp::EndTxn { op: RecordOp::Commit { .. }, .. } => Ok(Reply::Record(TxnRecord {
                status: TxnStatus::Committed,
                ..rec(TxnStatus::Committed, 300)
            })),
            _ => ok_for(r, &record),
        });
        assert!(c.take_events().is_empty(), "no answer before the clock is at the commit");
        assert_eq!(c.clock_wait(), Some(300));
        // Asked again meanwhile, it still waits; no resolution goes before the answer.
        c.commit(ts(10));
        c.abort();
        assert!(c.take_events().is_empty());
        assert!(c.take_requests().is_empty());
        c.tick(&Two, Nanos::ZERO, 299);
        assert!(c.take_events().is_empty());
        c.tick(&Two, Nanos::ZERO, 300);
        assert_eq!(c.take_events(), vec![Event::Ended(Outcome::Committed(ts(300)))]);
        assert_eq!(c.clock_wait(), None);
        assert!(c.take_requests().iter().all(|r| matches!(r.op, TxnOp::ResolveIntents { .. })));
        // A read-only transaction whose mark is ahead of the clock waits the same way.
        let mut c = Coordinator::new(TxnId(2), (7, 2), ts(50), Nanos::ZERO);
        c.commit(ts(40));
        assert!(c.take_events().is_empty());
        c.tick(&Two, Nanos::ZERO, 50);
        assert_eq!(
            c.take_events(),
            vec![Event::Ended(Outcome::Committed(ts(50))), Event::Resolved]
        );
    }

    #[test]
    fn a_batch_past_one_part_goes_in_parts_and_commits_them_at_once() {
        let record = rec(TxnStatus::Pending, 0);
        let big = |k: &[u8]| (k.to_vec(), IntentOp::Put { value_type: 1, value: vec![0; 10] });
        let mut c = coord().with_part_bytes(25);
        // Three writes of 12 bytes each in one range: two parts, never one phase.
        c.write_and_commit(&Two, vec![big(b"a1"), big(b"a2"), big(b"a3")], ts(9));
        let reqs = c.take_requests();
        let puts: Vec<usize> = reqs
            .iter()
            .filter_map(|r| match &r.op {
                TxnOp::PutIntents { writes, .. } => Some(writes.len()),
                _ => None,
            })
            .collect();
        assert_eq!(puts, vec![2, 1], "{reqs:?}");
        assert!(reqs.iter().all(|r| !matches!(r.op, TxnOp::OnePhase { .. })));
        for r in reqs {
            let a = ok_for(&r, &record);
            c.on_reply(&Two, r.id, a);
        }
        answer_all(&mut c, |r| ok_for(r, &record));
        assert!(c.take_events().iter().any(|e| matches!(e, Event::Ended(Outcome::Committed(_)))));
        // A single write past the part is a part of its own; a batch that fits is one phase.
        assert_eq!(parts(vec![big(b"a1")], 5).len(), 1);
        assert_eq!(parts(vec![(b"k".to_vec(), IntentOp::Delete)], 0).len(), 1);
        let mut c = coord().with_part_bytes(100);
        c.write_and_commit(&Two, vec![big(b"a1"), big(b"a2")], ts(9));
        assert!(matches!(c.take_requests()[..], [Request { op: TxnOp::OnePhase { .. }, .. }]));
    }

    /// The `Begin` and the intents of a transaction lose their answers (their node went down) and
    /// its abort finds no leader. The intents may be in the range, and the record, if a push
    /// created it, has none of their intervals: only the abort gives them to it, and the reaper
    /// resolves no more than those (seed 790 of the simulator's `search`, plan `heavy`: an intent
    /// no one resolved held back every search for the rest of the run). The abort goes again at
    /// the next ticks until a range answers it, and the transaction ends then.
    #[test]
    fn an_abort_without_an_answer_goes_again_until_the_record_has_its_intervals() {
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |_| Err(RangeError::Unavailable));
        c.abort();
        let mut aborts = Vec::new();
        for answer in [Err(RangeError::NotLeader { hint: NodeId(0) }), Err(RangeError::Timeout)] {
            answer_all(&mut c, |r| {
                if let TxnOp::EndTxn { op: RecordOp::Abort { spans, .. }, .. } = &r.op {
                    aborts.push(spans.clone());
                }
                answer.clone()
            });
            let events = c.take_events();
            assert!(
                !events.iter().any(|e| matches!(e, Event::Ended(_))),
                "not ended while the abort has no answer: {events:?}"
            );
            c.tick(&Two, Nanos::ZERO, WALL);
        }
        let record = rec(TxnStatus::Pending, 0);
        answer_all(&mut c, |r| {
            if let TxnOp::EndTxn { op: RecordOp::Abort { spans, .. }, .. } = &r.op {
                aborts.push(spans.clone());
            }
            ok_for(r, &record)
        });
        let want = vec![(b"a".to_vec(), b"a\0".to_vec())];
        assert_eq!(aborts, vec![want.clone(), want.clone(), want]);
        let events = c.take_events();
        assert!(events.contains(&Event::Ended(Outcome::Aborted)), "{events:?}");
        assert!(events.contains(&Event::Resolved), "{events:?}");
        // A range that never answers: the abort gives up after as many attempts as a resolution.
        let mut c = coord();
        c.write(&Two, vec![(b"a".to_vec(), put())]).unwrap();
        answer_all(&mut c, |_| Err(RangeError::Unavailable));
        c.abort();
        let mut sent = 0;
        for _ in 0..MAX_RESOLVE_ATTEMPTS + 2 {
            answer_all(&mut c, |r| {
                if matches!(r.op, TxnOp::EndTxn { op: RecordOp::Abort { .. }, .. }) {
                    sent += 1;
                }
                Err(RangeError::Timeout)
            });
            c.tick(&Two, Nanos::ZERO, WALL);
        }
        assert_eq!(sent, MAX_RESOLVE_ATTEMPTS);
        assert!(c.take_events().contains(&Event::Ended(Outcome::Aborted)));
    }
}
