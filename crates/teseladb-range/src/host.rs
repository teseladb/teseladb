//! The ranges of a core (FR-007 to FR-010, plan «Camino de una escritura»): one host per core,
//! sans-I/O, beside its `MultiRaftCore` and its LSM. On the leader of a range it keeps the
//! latches and the read cache of the term, which starts 250 ms above the node's clock when the
//! replica wins the term (FR-005).
//!
//! A write takes the latches of its documents, reads their current versions in the LSM, and is
//! evaluated inside `MultiRaftCore::propose_at` at its own mark, above the reads of its keys; it is
//! answered at the point its write concern asks for and frees its latches once applied. A read
//! asks the core to confirm the leadership (lease or ReadIndex), then waits for the latches it has
//! to, enters the read cache and reads the LSM at its mark. The LSM's release floor reaches the
//! log through the state machine (`RangeSm::release_floor`). The embedder runs the core's and the
//! LSM's I/O and passes the core's results to [`RangeHost::on_done`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Kind, Lsm, Pressure, Step};
use teseladb_multiraft::{Done, MultiRaftCore, NodeClock, OpError, OpId, ProposeAt, StateMachine};
use teseladb_raft::Role;
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::ack::{AckWhen, WriteConcern, ack_when, acked};
use crate::checksum::{CHECKSUM_EVERY_MS, Computer, Rebuild, Report, Tally};
use crate::command::{Command, DedupKey};
use crate::dedup;
use crate::gc::{Periodic, Round};
use crate::latch::{LatchId, Latches, ReadWait, WaitFor};
use crate::metrics::{Counters, RangeMetrics};
use crate::mutate::{Outcome, Refused, evaluate};
use crate::push::{ReadFault, ScanState, TxnView, Window, read_visible};
use crate::read::{ReadConcern, admit_own_read, admit_read};
use crate::sm::{RangeState, dedup_key};
use crate::tscache::TsCache;

mod closed;
mod feed;
mod fence;
mod merge;
mod purge;
mod recount;
mod split;
mod txn;
mod write;

pub use feed::{ChangesReq, STREAM_HOLD};
pub use fence::FenceOutcome;
pub use merge::MergeSource;
pub use split::SplitDone;
pub use txn::{CONTINUES, FIND_BATCH, TxnOp, TxnReq, continues, last_activity};

/// The lowest mark.
const ZERO: Hlc = Hlc { physical: 0, logical: 0 };
/// The newest mark: a write reads the current versions at it.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
/// How long after the last tally of reads served outside the core a range's group is kept awake
/// (005 FR-028): one second, as the tallies come every 64 reads or 10 ms.
pub const READS_KEEP_AWAKE: Nanos = Nanos::from_millis(1_000);

/// How long a published view serves reads on other threads before the core checks the lease and
/// the clock rule again (005 FR-028): a core polls far more often. It stays under the 250 ms
/// a new leader's read cache starts above the clock (`LEASE_MARGIN_MS`).
pub const VIEW_SPAN: Nanos = Nanos::from_millis(50);
/// How far ahead of this node's clock a client's mark may be: twice the most two clocks of the
/// cluster differ (D-004). A mark further ahead comes from a clock outside the cluster's rule,
/// and waiting for it would hold the read for that long.
const FUTURE_READ_MS: u64 = 2 * crate::tscache::LEASE_MARGIN_MS;
/// Intervals the read cache of a leader holds.
const CACHE_INTERVALS: usize = 4096;

/// A request to the host.
pub type ReqId = u64;

/// A mutation of the layer above (contract `range-client-api.md`).
pub trait Mutation {
    /// The new versions, index changes and change description, from the current versions of
    /// the write's documents and its mark.
    fn eval(&self, current: &[Option<(u8, &[u8])>], ts: Hlc) -> Result<Outcome, MutationError>;

    /// [`Mutation::eval`] knowing the keys `current` is for: the write's own keys, then those
    /// [`Mutation::reads`] asked for.
    fn eval_with(
        &self,
        _keys: &[Vec<u8>],
        current: &[Option<(u8, &[u8])>],
        ts: Hlc,
    ) -> Result<Outcome, MutationError> {
        self.eval(current, ts)
    }

    /// Keys `outcome` writes whose current versions the evaluation has to see (009 FR-004: the
    /// entries of a unique index). The leader adds the ones it did not read to the write's keys,
    /// latches and reads them, and evaluates again.
    fn reads(&self, _outcome: &Outcome) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// A mutation the layer above refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationError(pub String);

/// The start of the refusal of a mutation whose outcome reaches a key of another range: the
/// layer above writes it as a transaction across ranges (008, 009 FR-003).
pub const CROSSES_RANGES: &str = "the write reaches a key of another range";

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// This replica does not lead the range (`hint`: the known leader, 0 if none).
    NotLeader {
        /// Known leader.
        hint: NodeId,
    },
    /// No answer within the deadline.
    Timeout,
    /// The LSM or the proposal queue is full.
    Overloaded,
    /// The range does not cover the key (007), or it is not on this core.
    OutOfRange,
    /// `w` asks for more replicas than the range has.
    Unsatisfiable,
    /// The write does not fit in one entry of 64 MiB (FR-023).
    TooLarge,
    /// The range stopped, or left the host while a write was proposed: whether the write was
    /// applied is unknown.
    Unavailable,
    /// The read's mark is below the range's GC threshold: its versions may be gone (RefDb's
    /// `SnapshotTooOld`, 239).
    SnapshotTooOld,
    /// A version inside the read's uncertainty window, at this mark (008 FR-008): the read goes
    /// again at or above it.
    ReadUncertain {
        /// The version's mark.
        ts: Hlc,
    },
    /// The mutation failed.
    Mutation(MutationError),
    /// A transaction's write found a version newer than its read mark (008 FR-002): RefDb's
    /// `WriteConflict` (112).
    WriteConflict,
    /// A write found another transaction's intent (008 FR-002): its owner, to push or wait for.
    IntentConflict(crate::intents::IntentOwner),
    /// A read at mark `at` found the intent of a transaction it knows nothing about (008 FR-003):
    /// the reader pushes the owner above `at`, learns its state and reads again at `at` with it.
    ReadIntent {
        /// The intent's transaction, its record and the key that holds the intent.
        owner: crate::intents::IntentOwner,
        /// The read's mark.
        at: Hlc,
    },
    /// The transaction's record is aborted, or never began: RefDb's `NoSuchTransaction` (251).
    NoSuchTransaction,
    /// A write fence holds a key of the write (018 FR-002): a resharding is in its critical
    /// section, and nothing was written. The caller reads the catalog again and retries.
    Fenced,
}

/// A write: its range, the documents it changes, the mutation, its idempotence record and its
/// write concern.
pub struct WriteReq {
    /// Range.
    pub g: GroupId,
    /// Keys of the documents (their latches are taken before reading them).
    pub keys: Vec<Vec<u8>>,
    /// The mutation.
    pub m: Box<dyn Mutation>,
    /// The statement of a retryable write (FR-011).
    pub dedup: Option<DedupKey>,
    /// Write concern.
    pub wc: WriteConcern,
}

/// A deletion of one document (FR-017): its range, its key, its idempotence record and its write
/// concern. The answer's `result` is `[1]` if a version was deleted and `[0]` if there was none.
pub struct DeleteReq {
    /// Range.
    pub g: GroupId,
    /// Key of the document.
    pub key: Vec<u8>,
    /// The statement of a retryable write (FR-011).
    pub dedup: Option<DedupKey>,
    /// Write concern.
    pub wc: WriteConcern,
}

/// The mutation of a [`DeleteReq`]: a tombstone over the current version, nothing without one.
struct DeleteDoc(Vec<u8>);

impl Mutation for DeleteDoc {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let found = current.first().is_some_and(Option::is_some);
        Ok(Outcome {
            puts: Vec::new(),
            deletes: if found { vec![self.0.clone()] } else { Vec::new() },
            change: Vec::new(),
            result: vec![u8::from(found)],
        })
    }
}

/// A request's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A write, with its mark (`operationTime`), its change description and what the driver gets
    /// back. A retry of a recorded statement gets the original mark and result, and no change.
    Mutated {
        /// Mark of the write.
        ts: Hlc,
        /// Description of the change.
        change: Vec<u8>,
        /// The mutation's result.
        result: Vec<u8>,
    },
    /// A read at mark `ts`: the visible version (`(value type, value)`).
    Value {
        /// Mark of the read.
        ts: Hlc,
        /// The visible version.
        value: Option<(u8, Vec<u8>)>,
    },
    /// A scan: the visible documents in key order.
    Batch(Vec<Row>),
    /// A transaction's intents written, or resolved, at mark `ts` (008).
    Intents {
        /// Mark of the entry.
        ts: Hlc,
    },
    /// A transaction's record after `EndTxn` or `PushTxn` (008).
    Record(crate::intents::TxnRecord),
    /// What a change stream reads of the range (014): the versions of `(from, to]`, `to` at or
    /// below the closed mark, and the oldest intent pending there.
    Changes {
        /// The versions and deletions.
        rows: Vec<crate::changes::Versioned>,
        /// The end of the interval read.
        to: Hlc,
        /// The mark of the oldest pending intent.
        oldest_intent: Option<Hlc>,
    },
    /// A `Fence` applied at mark `ts` (018): every write of the span the range took before it
    /// is at or below it.
    Fenced {
        /// Mark of the entry.
        ts: Hlc,
    },
}

pub(crate) struct Leader {
    pub(crate) term: u64,
    /// The last index of its log when it took the term (the term's `Noop` and every entry of
    /// earlier terms): nothing is proposed before the range applied it, because an entry of an
    /// earlier term may still change the documents, its latches went with that term, and its mark
    /// reaches the clock only when it is applied.
    ready: u64,
    latches: Latches,
    cache: TsCache,
    /// The round of purging idempotence records (FR-011).
    purge: Purge,
    /// The round of raising the GC threshold (FR-013).
    pub(crate) gc: Round,
    /// The round of checksums (FR-014) and the tally of the last one.
    pub(crate) check: Round,
    pub(crate) tally: Option<Tally>,
    /// The split it carries out (007).
    split: split::SplitJob,
    /// Its part of a merge (007).
    merge: merge::MergeJob,
    /// The load it serves, for a split by load (007 FR-006).
    load: crate::load::Load,
    /// When it closes its next mark, and when it next writes one in the log (007 FR-012).
    close_at: Nanos,
    log_close_at: Nanos,
    /// Until when change streams read the range: it closes marks 200 ms behind its clock till
    /// then (014 FR-004).
    pub(crate) streams_until: Nanos,
    /// The highest mark it closed, with the last index of its log then: a mark closed with the
    /// longer lag never goes below it, and the range reads at it once it applied that index.
    pub(crate) last_closed: (Hlc, u64),
    /// The spans of the fences it proposed and the range has not applied yet (018).
    pub(crate) fencing: Vec<crate::fence::Span>,
    /// Its recount of the live documents of each collection (007 FR-039).
    recount: recount::RecountJob,
    /// When the reads served outside the core last reported a tally of this range (005 FR-028).
    read_outside_at: Option<Nanos>,
}

impl Leader {
    /// Whether the range applied every entry of the terms before this one: until then nothing is
    /// proposed, because those entries may carry marks above the leader's clock (FR-004).
    pub(crate) fn caught_up<S: StateMachine>(&self, core: &MultiRaftCore<S>, g: GroupId) -> bool {
        core.status(g).is_some_and(|st| st.applied >= self.ready)
    }

    /// Its round `kind`.
    pub(crate) fn round(&mut self, kind: Periodic) -> &mut Round {
        match kind {
            Periodic::Gc => &mut self.gc,
            Periodic::Checksum => &mut self.check,
        }
    }
}

/// Where a leader's purge round is.
enum Purge {
    /// The next round starts at this cluster time (ms).
    Idle(u64),
    /// Looking for the expired records.
    Scanning(Box<dedup::PurgeScan>),
    /// Waiting for the latches of the records found.
    Latching(Vec<Vec<u8>>),
    /// Its `PurgeDedup` is proposed (a [`Req::Purge`] waits for it).
    Proposed,
}

/// A proposed `PurgeDedup`: its latch goes when the core answers.
pub(crate) struct PurgeReq {
    g: GroupId,
    term: u64,
    latch: LatchId,
    index: u64,
}

/// A proposed `SetGcThreshold` or `ComputeChecksum`.
pub(crate) struct RoundReq {
    pub(crate) g: GroupId,
    pub(crate) term: u64,
    pub(crate) kind: Periodic,
}

pub(crate) struct Range {
    pub(crate) start: Vec<u8>,
    pub(crate) end: Vec<u8>,
    pub(crate) leader: Option<Leader>,
    /// This replica's checksums (FR-014).
    pub(crate) sum: Computer,
    /// What it counts for FR-022.
    pub(crate) counters: Counters,
    /// Born from a split and its `SplitGenesis` not proposed yet: its parent and the parent's
    /// `Split` (007).
    genesis: Option<(GroupId, u64)>,
    /// Frozen for a merge (007): it serves nothing.
    frozen: bool,
}

enum MutState {
    /// Waiting for the latches of its documents.
    Latch,
    /// Holding them, reading the current versions.
    Read(LatchId),
    /// Proposed (its operation is in `RangeHost::ops`).
    Proposed(Proposal),
    /// Refused by its mutation (a compare that does not match, say): the answer says what the
    /// replica holds, so it goes only once the replica confirms it still leads, as a read does
    /// (its operation is in `RangeHost::ops`).
    Confirm(RangeError),
    /// A read of a transaction's record that pushed it (008 T043), or a mutation that changed
    /// nothing (005 FR-026): its answer goes once the replica confirms it still leads (its
    /// operation is in `RangeHost::ops`).
    Answer(Box<Reply>),
}

/// A proposed write: at `ts`, entry `index`; once applied it may still wait for the copies its
/// write concern asks for.
struct Proposal {
    latch: LatchId,
    ts: Hlc,
    change: Vec<u8>,
    result: Vec<u8>,
    index: u64,
    replied: bool,
    applied: bool,
    /// When it was proposed (FR-022).
    proposed_at: Nanos,
    /// The answer of a transaction's request; a mutation's is built from `change` and `result`.
    reply: Option<Box<Reply>>,
}

impl Proposal {
    /// Its answer once acknowledged.
    fn answer(&mut self) -> Reply {
        let (change, result) = (std::mem::take(&mut self.change), std::mem::take(&mut self.result));
        self.reply.take().map_or(Reply::Mutated { ts: self.ts, change, result }, |r| *r)
    }
}

/// What a write request does: a mutation of the layer above, or a transaction's command (008).
pub(crate) enum Work {
    Mutation(Box<dyn Mutation>),
    Txn(Box<TxnOp>),
}

impl MutState {
    /// The proposal of a write the core is working on.
    fn proposal(&mut self) -> &mut Proposal {
        match self {
            MutState::Proposed(p) => p,
            MutState::Latch | MutState::Read(_) | MutState::Confirm(_) | MutState::Answer(_) => {
                unreachable!("only a proposed write waits")
            }
        }
    }
}

pub(crate) struct MutReq {
    g: GroupId,
    /// Term of the leader whose latches it holds.
    term: u64,
    keys: Vec<Vec<u8>>,
    work: Work,
    dedup: Option<DedupKey>,
    ack: AckWhen,
    state: MutState,
    /// It already waited for a latch (counted once, FR-022).
    waited: bool,
    /// It takes its latches only once the pending writes of its keys are applied (FR-029): its
    /// evaluation on them failed, and a failure answers only what is committed.
    settled: bool,
    /// The mark's physical part past which the leader no longer evaluates it (009 FR-006): the
    /// operation that sent it may have ended and released what it read of the catalog.
    deadline: Option<u64>,
}

enum GetState {
    /// A mark the client gave ahead of this node's wall clock: the read waits for the clock to
    /// reach it before the core confirms the leadership.
    Clock,
    /// The core confirms the leadership (its operation is in `RangeHost::ops`).
    Core,
    /// Waiting for the latches at its mark.
    Admit(Hlc),
    /// Reading the LSM at its mark.
    Read(Hlc),
    /// Scanning the LSM at its mark: the cursor keeps its place while a block is read.
    Scanning(Box<ScanState>),
    /// Reading its uncertainty window above its mark before the read (008 FR-008).
    Window(Hlc, Box<Window>),
}

pub use crate::push::Row;

/// Where a node's wall clock is against the mark a client gave a read.
enum ClockAt {
    /// At or past it, or the read has no mark of the client's.
    Reached,
    /// Behind it, within the time a read waits for.
    Behind,
    /// Too far behind it to wait for.
    TooFar,
}

pub(crate) struct GetReq {
    g: GroupId,
    /// The key, or the start of a scan.
    key: Vec<u8>,
    /// A scan's end and the most documents it returns (`None`: a point read).
    scan: Option<(Vec<u8>, usize)>,
    /// A scan from the last key down (H-003).
    backward: bool,
    /// The mark it reads at, if the client gave one (`get_at`); otherwise the clock's.
    at: Option<Hlc>,
    /// The range's GC threshold when the read arrived: a read below it fails (FR-013).
    gc_floor: Hlc,
    concern: ReadConcern,
    state: GetState,
    /// It already waited for a latch (counted once, FR-022).
    waited: bool,
    /// What the reader knows about the transactions whose intents it may meet (008).
    view: TxnView,
    /// The end of its uncertainty window, for a mark from another node's clock (008 FR-008).
    uncertain_to: Option<Hlc>,
}

/// A read with what its reader knows about transactions (008 FR-003): a point read of `key`, or a
/// scan of `[key, end)` of at most `limit` documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadSpec {
    /// Range.
    pub g: GroupId,
    /// The key, or the start of a scan.
    pub key: Vec<u8>,
    /// A scan's end and the most documents it returns (`None`: a point read).
    pub scan: Option<(Vec<u8>, usize)>,
    /// A scan from the last key down, the documents nearest its end first (H-003).
    pub backward: bool,
    /// The mark it reads at (`None`: the clock's).
    pub at: Option<Hlc>,
    /// Read concern.
    pub concern: ReadConcern,
    /// Its own transaction and the ones it learned about.
    pub view: TxnView,
    /// The end of its uncertainty window (`None`: none), for a mark taken from another node's
    /// clock (008 FR-008).
    pub uncertain_to: Option<Hlc>,
}

/// A scan of `[start, end)` that returns at most `limit` documents.
pub struct ScanReq {
    /// Range.
    pub g: GroupId,
    /// First key.
    pub start: Vec<u8>,
    /// End (excluded).
    pub end: Vec<u8>,
    /// Read concern.
    pub concern: ReadConcern,
    /// Most documents.
    pub limit: usize,
    /// From the last key down, the documents nearest `end` first (H-003).
    pub backward: bool,
}

pub(crate) enum Req {
    Mut(MutReq),
    Get(GetReq),
    Changes(feed::FeedReq),
    Purge(PurgeReq),
    Round(RoundReq),
    Fence(fence::FenceReq),
}

/// The ranges of a core.
pub struct RangeHost {
    pub(crate) lsm: Rc<RefCell<Lsm>>,
    clock: NodeClock,
    /// The views of the ranges it leads, for reads outside the core (005 FR-028).
    views: crate::view::RangeViews,
    /// The latest `valid_until` each range's view was published with: a read outside the core
    /// may take its mark until then.
    views_until: BTreeMap<GroupId, Nanos>,
    pub(crate) ranges: BTreeMap<GroupId, Range>,
    pub(crate) reqs: BTreeMap<ReqId, Req>,
    pub(crate) ops: BTreeMap<OpId, ReqId>,
    replies: Vec<(ReqId, Result<Reply, RangeError>)>,
    /// Splits its leaders saw applied, for the embedder (007).
    splits: Vec<SplitDone>,
    next: ReqId,
    /// Latches of writes that failed with an uncertain outcome, `(range, term, latch, index)`: the
    /// entry may still be applied at its mark, so the reads above it wait until it is or the term
    /// ends.
    uncertain: Vec<(GroupId, u64, LatchId, u64)>,
    /// Ranges that Group 0 moves to another core of the node (007 FR-011): their writes are
    /// refused before the proposal, so that none goes into the log past `Relocate` (H-025).
    moving: BTreeSet<GroupId>,
    /// Every node reads the writes that carry their change of documents (007 FR-039): the
    /// leaders write them.
    counted_writes: bool,
    wall_ms: u64,
    /// Monotonic time of the last call.
    now: Nanos,
    /// The GC thresholds last handed to the LSM (FR-013).
    pub(crate) gc_handed: Option<Vec<(GroupId, Hlc)>>,
    /// This replica's sums for the leaders (FR-014).
    pub(crate) reports: Vec<Report>,
    /// The replicas this leader marked for rebuild.
    pub(crate) rebuilds: Vec<Rebuild>,
    /// Operations per second above which a range splits by load (007 FR-006).
    load_split_ops: u64,
    /// History each range keeps, in cluster time (007 FR-019).
    pub(crate) gc_ttl_ms: u64,
}

fn op_error(e: OpError) -> RangeError {
    match e {
        OpError::NotLeader { hint } => RangeError::NotLeader { hint },
        // Another leader's entry took its index: the client retries with the new leader.
        OpError::ProposalDropped => RangeError::NotLeader { hint: NodeId(0) },
        OpError::Timeout => RangeError::Timeout,
        OpError::Overloaded => RangeError::Overloaded,
        // It will never fit: retrying does not help (RefDb's `BSONObjectTooLarge`).
        OpError::TooLarge => RangeError::TooLarge,
        OpError::GroupNotFound => RangeError::OutOfRange,
        _ => RangeError::Unavailable,
    }
}

/// The error a write gives for a fault of the read of its documents.
fn read_error(e: ReadFault) -> RangeError {
    match e {
        ReadFault::Intent(owner) => RangeError::IntentConflict(owner),
        ReadFault::Unavailable => RangeError::Unavailable,
        ReadFault::Uncertain(ts) => RangeError::ReadUncertain { ts },
    }
}

/// The end of `q`'s uncertainty window. The variant reads without one: a version written before
/// the read began on a node whose clock is ahead stays out of it (008 T024).
fn window_of(q: &GetReq) -> Option<Hlc> {
    q.uncertain_to.filter(|_| !cfg!(teseladb_bug = "zero_uncertainty"))
}

/// The state that reads `q` at mark `ts`: a point read, or a scan.
fn reading(lsm: &RefCell<Lsm>, q: &GetReq, ts: Hlc) -> GetState {
    match &q.scan {
        Some((end, _)) => {
            let mark = (ts, window_of(q));
            GetState::Scanning(Box::new(ScanState::new(
                lsm,
                (&q.key, end),
                mark,
                &q.view,
                q.backward,
            )))
        }
        None => GetState::Read(ts),
    }
}

/// The error a read at mark `at` gives for a fault.
fn read_fault(e: ReadFault, at: Hlc) -> RangeError {
    match e {
        ReadFault::Intent(owner) => RangeError::ReadIntent { owner, at },
        ReadFault::Unavailable => RangeError::Unavailable,
        ReadFault::Uncertain(ts) => RangeError::ReadUncertain { ts },
    }
}

impl RangeHost {
    /// The host of a core with this LSM and the node's clock.
    pub fn new(lsm: Rc<RefCell<Lsm>>, clock: NodeClock) -> RangeHost {
        RangeHost {
            lsm,
            clock,
            views: crate::view::RangeViews::default(),
            views_until: BTreeMap::new(),
            ranges: BTreeMap::new(),
            reqs: BTreeMap::new(),
            ops: BTreeMap::new(),
            replies: Vec::new(),
            splits: Vec::new(),
            next: 1,
            uncertain: Vec::new(),
            moving: BTreeSet::new(),
            counted_writes: false,
            wall_ms: 0,
            now: Nanos::ZERO,
            gc_handed: None,
            reports: Vec::new(),
            rebuilds: Vec::new(),
            load_split_ops: crate::load::LOAD_SPLIT_OPS,
            gc_ttl_ms: crate::gc::GC_TTL_MS,
        }
    }

    /// Operations per second above which a range splits by load (007 FR-006); leaderships taken
    /// from now on count with it.
    pub fn set_load_split_ops(&mut self, ops: u64) {
        self.load_split_ops = ops;
    }

    /// History each range this host leads keeps (`teselaHistoryRetention`, 007 FR-019): its next
    /// GC threshold is «now − ttl»; a threshold already raised never goes down.
    pub fn set_gc_ttl_ms(&mut self, ttl_ms: u64) {
        self.gc_ttl_ms = ttl_ms;
    }

    /// Counts an operation of range `g` on `key` if this host leads it.
    fn count_load(&mut self, g: GroupId, key: &[u8], now: Nanos) {
        if let Some(l) = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()) {
            l.load.record(key, now);
        }
    }

    /// The key at which range `g` splits by load, once this host led it above the threshold for
    /// 10 s (007 FR-006); asking starts the count again.
    pub fn load_cut(&mut self, g: GroupId, now: Nanos) -> Option<Vec<u8>> {
        let r = self.ranges.get_mut(&g)?;
        let start = r.start.clone();
        r.leader.as_mut()?.load.cut(now, &start)
    }

    /// Operations per second range `g` served in the last whole second (0 if this host does not
    /// lead it).
    pub fn load(&self, g: GroupId) -> u64 {
        self.ranges.get(&g).and_then(|r| r.leader.as_ref()).map_or(0, |l| l.load.ops_per_second())
    }

    /// Range `g` covers `[start, end)` on this core.
    pub fn add_range(&mut self, g: GroupId, start: Vec<u8>, end: Vec<u8>) {
        self.ranges.insert(
            g,
            Range {
                start,
                end,
                leader: None,
                sum: Computer::default(),
                counters: Counters::default(),
                genesis: None,
                frozen: false,
            },
        );
    }

    /// Range `g` leaves this core's host (FR-014: its replica is rebuilt; 007: a merge absorbed
    /// it): its requests end with `NotLeader` and no hint, so that their clients ask again, but a
    /// proposed write ends with `Unavailable`, since it may still be applied; what it had with the
    /// core is forgotten. `false` if the host did not have it.
    pub fn remove_range(&mut self, g: GroupId) -> bool {
        if self.ranges.remove(&g).is_none() {
            return false;
        }
        // The clients' requests are answered; the leader's own rounds and purges just end.
        // A proposed write may still be applied: its client learns that its outcome is unknown.
        let gone: Vec<(ReqId, Option<RangeError>)> = self
            .reqs
            .iter()
            .filter_map(|(id, r)| match r {
                Req::Mut(MutReq { g: rg, state: MutState::Proposed(_), .. }) => {
                    (*rg == g).then_some((*id, Some(RangeError::Unavailable)))
                }
                Req::Mut(MutReq { g: rg, .. })
                | Req::Get(GetReq { g: rg, .. })
                | Req::Changes(feed::FeedReq { g: rg, .. }) => {
                    (*rg == g).then_some((*id, Some(RangeError::NotLeader { hint: NodeId(0) })))
                }
                Req::Purge(PurgeReq { g: rg, .. }) | Req::Round(RoundReq { g: rg, .. }) => {
                    (*rg == g).then_some((*id, None))
                }
                // A `Fence` may still be applied; asking again changes nothing.
                Req::Fence(fence::FenceReq { g: rg, .. }) => {
                    (*rg == g).then_some((*id, Some(RangeError::Unavailable)))
                }
            })
            .collect();
        for (id, answer) in gone {
            self.reqs.remove(&id);
            self.ops.retain(|_, r| *r != id);
            if let Some(e) = answer {
                self.reply(id, Err(e));
            }
        }
        self.uncertain.retain(|(rg, ..)| *rg != g);
        self.reports.retain(|r| r.range != g);
        self.rebuilds.retain(|r| r.range != g);
        true
    }

    /// The ranges that Group 0 moves from this core to another one of the node (007 FR-011). A
    /// write of one of them that reaches its proposal is refused with `NotLeader` and no hint: the
    /// `Relocate` entry hands the log past it to the new core, and the old core could only answer
    /// such a write that its outcome is unknown (H-025). A move called off lets them through again.
    pub fn set_moving(&mut self, groups: impl IntoIterator<Item = GroupId>) {
        self.moving = groups.into_iter().collect();
    }

    /// Whether every node of the cluster reads the writes that carry their change of each
    /// collection's documents (007 FR-039): from then on this core's leaders write them.
    pub fn set_counted_writes(&mut self, on: bool) {
        self.counted_writes = on;
    }

    /// Whether range `g` is on its way to another core.
    pub(crate) fn is_moving(&self, g: GroupId) -> bool {
        self.moving.contains(&g)
    }

    /// The core's LSM.
    pub fn lsm(&self) -> &Rc<RefCell<Lsm>> {
        &self.lsm
    }

    /// The metrics of range `g` on this replica (FR-022), if this host has it.
    pub fn metrics<S: StateMachine>(
        &self,
        core: &MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> Option<RangeMetrics> {
        let r = self.ranges.get(&g)?;
        let (lease_reads, read_index_reads) = core.read_counts(g).unwrap_or_default();
        Some(RangeMetrics {
            applied_index: core.status(g).map_or(0, |st| st.applied),
            propose_latency: r.counters.propose_latency.clone(),
            lease_reads,
            read_index_reads,
            latch_waits: r.counters.latch_waits,
            clock_offset_ms: core.clock_offset(now),
        })
    }

    /// A request of range `g`, which this host leads, waits for a latch.
    fn count_wait(&mut self, g: GroupId) {
        self.ranges.get_mut(&g).expect("a range of this host").counters.latch_waits += 1;
    }

    /// The answers since the last call.
    pub fn take_replies(&mut self) -> Vec<(ReqId, Result<Reply, RangeError>)> {
        std::mem::take(&mut self.replies)
    }

    /// The mark and the leader's term of write `id` while it is proposed and not answered (an
    /// ordered batch proposes a document after the ones before it, FR-023).
    pub fn proposed(&self, id: ReqId) -> Option<(Hlc, u64)> {
        match self.reqs.get(&id)? {
            Req::Mut(MutReq { term, state: MutState::Proposed(p), .. }) => Some((p.ts, *term)),
            Req::Mut(_)
            | Req::Get(_)
            | Req::Changes(_)
            | Req::Purge(_)
            | Req::Round(_)
            | Req::Fence(_) => None,
        }
    }

    fn reply(&mut self, id: ReqId, r: Result<Reply, RangeError>) {
        self.replies.push((id, r));
    }

    /// Cluster time in milliseconds: the node's clock, never behind its wall clock.
    pub(crate) fn cluster_ms(&self) -> u64 {
        self.clock.0.peek().physical.max(self.wall_ms)
    }

    pub(crate) fn new_id(&mut self) -> ReqId {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Whether range `g` is on this core and holds `key`.
    fn covers(&self, g: GroupId, key: &[u8]) -> bool {
        // Faulty variant (007 T017): a range frozen for a merge goes on taking writes.
        #[cfg(teseladb_bug = "merge_without_freeze")]
        let frozen_ok = |_: &&Range| true;
        #[cfg(not(teseladb_bug = "merge_without_freeze"))]
        let frozen_ok = |r: &&Range| !r.frozen;
        self.ranges
            .get(&g)
            .filter(frozen_ok)
            .is_some_and(|r| r.start.as_slice() <= key && key < r.end.as_slice())
    }

    /// A write.
    pub fn mutate<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        w: WriteReq,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        let WriteReq { g, keys, m, dedup, wc } = w;
        self.submit(core, (g, keys, Work::Mutation(m), dedup, wc), None, (now, wall_ms))
    }

    /// A write that the leader evaluates only at a mark whose physical part is at most
    /// `deadline`; past it the write answers [`RangeError::Timeout`] and writes nothing (009
    /// FR-006).
    pub fn mutate_until<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        w: WriteReq,
        deadline: u64,
        (now, wall_ms): (Nanos, u64),
    ) -> ReqId {
        let WriteReq { g, keys, m, dedup, wc } = w;
        self.submit(core, (g, keys, Work::Mutation(m), dedup, wc), Some(deadline), (now, wall_ms))
    }

    /// A write request: a mutation or a transaction's command.
    fn submit<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        (g, keys, work, dedup, wc): (GroupId, Vec<Vec<u8>>, Work, Option<DedupKey>, WriteConcern),
        deadline: Option<u64>,
        (now, wall_ms): (Nanos, u64),
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        let id = self.new_id();
        if keys.is_empty() || !keys.iter().all(|k| self.covers(g, k)) {
            self.reply(id, crossing(&keys, |k| self.covers(g, k), &work));
            return id;
        }
        self.count_load(g, &keys[0], now);
        let replicas = core.conf(g).map_or(1, |c| c.voters.len() + c.learners.len());
        let Ok(ack) = ack_when(wc, replicas) else {
            self.reply(id, Err(RangeError::Unsatisfiable));
            return id;
        };
        // Level 0 past its rejection limit: no new write until compaction empties it (004
        // `lsm-api.md`; entries already in the log still apply).
        if self.lsm.borrow().pressure() == Pressure::Reject {
            self.reply(id, Err(RangeError::Overloaded));
            return id;
        }
        self.refresh(core, g);
        let (state, waited) = (MutState::Latch, false);
        let req =
            MutReq { g, term: 0, keys, work, dedup, ack, state, waited, settled: false, deadline };
        self.reqs.insert(id, Req::Mut(req));
        self.step(core, id, now);
        id
    }

    /// A deletion of one document: a write whose mutation deletes the version it finds.
    pub fn delete<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        d: DeleteReq,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        let DeleteReq { g, key, dedup, wc } = d;
        let m = Box::new(DeleteDoc(key.clone()));
        self.mutate(core, WriteReq { g, keys: vec![key], m, dedup, wc }, now, wall_ms)
    }

    /// A read of `key` with read concern `concern`.
    pub fn get<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        key: &[u8],
        concern: ReadConcern,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        let req = GetReq {
            g,
            key: key.to_vec(),
            scan: None,
            backward: false,
            at: None,
            gc_floor: ZERO,
            concern,
            state: GetState::Core,
            waited: false,
            view: TxnView::default(),
            uncertain_to: None,
        };
        self.read(core, req, true, now)
    }

    /// A read of `key` at mark `ts` (contract `ReadSpec::ts`): once this node's wall clock reaches
    /// `ts`, the core confirms the leadership, then the read waits for the writes proposed at or
    /// below `ts` and enters the read cache there, so that nothing is written below it later.
    pub fn get_at<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        key: &[u8],
        ts: Hlc,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        let concern = ReadConcern::Linearizable;
        let req = GetReq {
            g,
            key: key.to_vec(),
            scan: None,
            backward: false,
            at: Some(ts),
            gc_floor: core.state_machine(g).map_or(ZERO, RangeState::gc_threshold),
            concern,
            state: GetState::Core,
            waited: false,
            view: TxnView::default(),
            uncertain_to: None,
        };
        self.read(core, req, true, now)
    }

    /// A read with what its reader knows about transactions (008): at the mark it gives, or at
    /// the clock's.
    pub fn read_with<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        spec: ReadSpec,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        let ReadSpec { g, key, scan, backward, at, concern, view, uncertain_to } = spec;
        let ends_inside = scan.as_ref().is_none_or(|(end, _)| {
            self.ranges.get(&g).is_some_and(|r| *end <= r.end) && key < *end
        });
        let gc_floor = match at {
            Some(_) => core.state_machine(g).map_or(ZERO, RangeState::gc_threshold),
            None => ZERO,
        };
        let concern = if at.is_some() { ReadConcern::Linearizable } else { concern };
        let req = GetReq {
            g,
            key,
            scan,
            backward,
            at,
            gc_floor,
            concern,
            state: GetState::Core,
            waited: false,
            view,
            uncertain_to,
        };
        self.read(core, req, ends_inside, now)
    }

    /// A scan.
    pub fn scan<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        r: ScanReq,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        let ScanReq { g, start, end, concern, limit, backward } = r;
        // The scan stays inside the range: its start is checked with the keys, its end here.
        let ends_inside = self.ranges.get(&g).is_some_and(|r| end <= r.end) && start < end;
        let scan = Some((end, limit));
        let req = GetReq {
            g,
            key: start,
            scan,
            backward,
            at: None,
            gc_floor: ZERO,
            concern,
            state: GetState::Core,
            waited: false,
            view: TxnView::default(),
            uncertain_to: None,
        };
        self.read(core, req, ends_inside, now)
    }

    fn read<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        req: GetReq,
        ends_inside: bool,
        now: Nanos,
    ) -> ReqId {
        let id = self.new_id();
        if !ends_inside || !self.covers(req.g, &req.key) {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        }
        self.count_load(req.g, &req.key, now);
        // Faulty variant (005 T017): a node that believes it leads serves the read at once, without
        // a lease or ReadIndex, also after a partition took its leadership.
        #[cfg(teseladb_bug = "stale_lease_read")]
        if self.ranges.get(&req.g).is_some_and(|r| r.leader.is_some())
            && let Some(ts) = core.read_mark(now)
        {
            let req = GetReq { state: GetState::Admit(ts), ..req };
            self.reqs.insert(id, Req::Get(req));
            self.step(core, id, now);
            return id;
        }
        match self.clock_at(&req) {
            ClockAt::Reached => self.confirm(core, id, req, now),
            ClockAt::Behind => {
                self.reqs.insert(id, Req::Get(GetReq { state: GetState::Clock, ..req }));
            }
            ClockAt::TooFar => self.reply(id, Err(RangeError::Timeout)),
        }
        id
    }

    /// Where this node's wall clock is against the mark the client gave read `q`, if any.
    fn clock_at(&self, q: &GetReq) -> ClockAt {
        match q.at {
            Some(at) if at.physical > self.wall_ms + FUTURE_READ_MS => ClockAt::TooFar,
            Some(at) if at.physical > self.wall_ms => ClockAt::Behind,
            Some(_) | None => ClockAt::Reached,
        }
    }

    /// Asks the core to confirm the leadership for read `id`.
    fn confirm<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        q: GetReq,
        now: Nanos,
    ) {
        match core.read(q.g, q.concern.mode(), now) {
            Ok(op) => {
                self.ops.insert(op, id);
                self.reqs.insert(id, Req::Get(GetReq { state: GetState::Core, ..q }));
            }
            Err(e) => self.reply(id, Err(op_error(e))),
        }
    }

    /// A result of the core; `true` if it was one of the host's operations.
    pub fn on_done<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        op: OpId,
        r: Result<Done<S::Output>, OpError>,
    ) -> bool
    where
        S::Output: FenceOutcome,
    {
        let Some(id) = self.ops.remove(&op) else { return false };
        // Requests with an operation in the core stay until it ends.
        let req = self.reqs.remove(&id).expect("an operation's request waits for it");
        match req {
            Req::Mut(MutReq { state: MutState::Confirm(e), .. }) => {
                self.reply(id, Err(r.map_or_else(op_error, |_| e)));
            }
            Req::Mut(MutReq { state: MutState::Answer(answer), .. }) => {
                self.reply(id, r.map(|_| *answer).map_err(op_error));
            }
            Req::Mut(m) => {
                let r = r.map(|d| matches!(&d, Done::Applied(o) if fence::refused(o)));
                self.mutation_done(core, id, m, r)
            }
            Req::Purge(p) => self.purge_done(p, r.map(|_| ())),
            Req::Round(p) => self.round_done(p),
            Req::Fence(f) => self.fence_done(id, f, r),
            // Only a stream's read at a mark has an operation in the core.
            Req::Changes(f) => self.changes_confirmed(core, id, f, r.is_ok()),
            Req::Get(mut q) => match r {
                // The mark comes from the clock the core's clock rule checked (FR-006).
                Ok(_) => match q.at.or_else(|| core.read_mark(self.now)) {
                    Some(ts) => {
                        self.refresh(core, q.g);
                        q.state = GetState::Admit(ts);
                        self.reqs.insert(id, Req::Get(q));
                        self.step(core, id, Nanos::ZERO);
                    }
                    None => self.reply(id, Err(RangeError::Overloaded)),
                },
                Err(e) => self.reply(id, Err(op_error(e))),
            },
        }
        true
    }

    /// Moves every request that can move and follows leadership changes; `true` if something
    /// changed.
    pub fn poll<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        now: Nanos,
        wall_ms: u64,
    ) -> bool {
        self.wall_ms = wall_ms;
        self.now = now;
        // The bounds the state machines applied, before any step that proposes: a cut from an
        // earlier leader's term may have been applied since the embedder last followed them.
        let mut moved = self.follow_bounds(core);
        let groups: Vec<GroupId> = self.ranges.keys().copied().collect();
        for g in groups {
            moved |= self.refresh(core, g);
            moved |= self.split_step(core, g, now);
            moved |= self.genesis_step(core, g, now);
            moved |= self.merge_step(core, g, now);
        }
        moved |= self.settle_uncertain(core);
        // A frozen range runs no round: its state waits for the merge as it is.
        let led: Vec<GroupId> = self
            .ranges
            .iter()
            .filter(|(_, r)| r.leader.is_some() && !r.frozen)
            .map(|(g, _)| *g)
            .collect();
        for g in led {
            moved |= self.purge_step(core, g, now);
            moved |= self.round_step(core, g, Periodic::Gc, now);
            moved |= self.round_step(core, g, Periodic::Checksum, now);
            moved |= self.close_step(core, g, now);
            moved |= self.recount_step(core, g, now);
            moved |= self.keep_awake(core, g, now);
        }
        let ids: Vec<ReqId> = self.reqs.keys().copied().collect();
        for id in ids {
            moved |= self.step(core, id, now);
        }
        self.publish_views(core, now);
        moved
    }

    /// Publishes the views of the ranges this host leads in `views`, which the node's handle
    /// reads (005 FR-028).
    pub fn set_views(&mut self, views: crate::view::RangeViews) {
        self.views = views;
    }

    /// Counts for range `g`'s split by load (007 FR-006) `ops` reads served on other threads, of
    /// which `keys` are a sample (005 FR-028).
    pub fn count_outside(&mut self, g: GroupId, ops: u64, keys: &[Vec<u8>], now: Nanos) {
        if let Some(l) = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()) {
            l.load.record_sampled(keys, ops, now);
            l.read_outside_at = Some(now);
        }
    }

    /// Reads of range `g` are coming that its view would serve if it were published (a count the
    /// view could not give went to the core): its group is kept awake as for reads served outside
    /// the core.
    pub fn note_reads<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) {
        if let Some(l) = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()) {
            l.read_outside_at = Some(now);
        }
        self.keep_awake(core, g, now);
    }

    /// Wakes the group of range `g` if it went quiescent while reads were served outside the core
    /// within [`READS_KEEP_AWAKE`]: a quiescent leader has no lease, so its view would not be
    /// published and those reads would go to the core (H-004 of the native tests). `true` if it
    /// woke it.
    fn keep_awake<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let read =
            self.ranges.get(&g).and_then(|r| r.leader.as_ref()).and_then(|l| l.read_outside_at);
        let recent = read.is_some_and(|at| now.0.saturating_sub(at.0) < READS_KEEP_AWAKE.0);
        recent && core.is_quiesced(g) && core.wake_leader(g)
    }

    /// The views of the ranges this host leads, for reads on other threads (005 FR-028).
    pub fn views(&self) -> crate::view::RangeViews {
        self.views.clone()
    }

    /// Publishes the LSM's version and a view of each range it leads and may serve reads of on
    /// other threads for [`VIEW_SPAN`]: caught up with its term, with the lease and the clock rule
    /// holding now and at the end of the span, and no split or merge in progress (the keys may
    /// change range). The embedder takes the replies after `poll`, so a write is answered after
    /// the view that shows it.
    fn publish_views<S: StateMachine + RangeState>(&mut self, core: &MultiRaftCore<S>, now: Nanos) {
        self.lsm.borrow().publish();
        let shared = self.lsm.borrow().shared();
        let until = now + VIEW_SPAN;
        let mut views = BTreeMap::new();
        for (g, r) in &self.ranges {
            let Some(l) = r.leader.as_ref() else { continue };
            let still = matches!(l.split, split::SplitJob::Idle)
                && matches!(l.merge, merge::MergeJob::Idle)
                && !r.frozen
                && !self.moving.contains(g);
            if !still
                || !l.caught_up(core, *g)
                || !core.lease_readable(*g, now)
                || !core.lease_readable(*g, until)
            {
                continue;
            }
            let handed = self.gc_handed.iter().flatten().find(|(r, _)| r == g).map(|(_, t)| *t);
            let parts = crate::view::ViewParts {
                g: *g,
                term: l.term,
                range: (r.start.clone(), r.end.clone()),
                valid_until: until,
                gc_floor: handed.unwrap_or(ZERO),
                held: l.latches.held(),
                keys: l.latches.keys(),
                outside: l.cache.outside(),
                counts: core.state_machine(*g).and_then(RangeState::shared_counts),
            };
            let view = crate::view::RangeView::new(parts, shared.clone(), self.clock.clone());
            views.insert(*g, std::sync::Arc::new(view));
            self.views_until.insert(*g, until);
        }
        self.views.publish(views);
        self.views_until.retain(|_, u| now < *u);
    }

    /// Whether no read outside the core can still take a mark through a view of range `g`: the
    /// last one published is past its `valid_until` at `now` (005 FR-028). A merge waits for it
    /// on the right range's replicas, so the frames they send after carry a clock above every mark
    /// those reads took, and the merged range writes above them.
    pub fn views_settled(&self, g: GroupId, now: Nanos) -> bool {
        self.views_until.get(&g).is_none_or(|u| now >= *u)
    }

    /// Follows the leadership of `g`: a new term gets new latches and a read cache whose minimum
    /// is 250 ms above the node's clock when the core saw it win the election (the core raised
    /// the clock to it then, `MultiRaftCore::lease_mark`).
    /// Any change fails the requests of `g` that have nothing in the core yet (their latches and
    /// marks belong to the old state); the client retries. `true` if it changed.
    fn refresh<S: StateMachine>(&mut self, core: &MultiRaftCore<S>, g: GroupId) -> bool {
        let Some(st) = core.status(g) else { return false };
        // The core raised the clock to the cache's minimum as the replica won the term.
        let now = Hlc { physical: self.cluster_ms(), logical: 0 };
        let lease = core.lease_mark(g).unwrap_or(now);
        let range = self.ranges.get_mut(&g).expect("a range of this host");
        let term = (st.role == Role::Leader).then_some(st.hard_state.term);
        if range.leader.as_ref().map(|l| l.term) == term {
            return false;
        }
        // Faulty variant (005 T017): a leader that lost its term keeps its state and goes on serving
        // reads, as with an expired lease.
        #[cfg(teseladb_bug = "stale_lease_read")]
        if term.is_none() && range.leader.is_some() {
            return false;
        }
        let ready = st.last_index;
        let load = crate::load::Load::new(self.load_split_ops, g.0);
        range.leader = term.map(|term| Leader {
            term,
            ready,
            latches: Latches::default(),
            cache: TsCache::for_lease(lease, CACHE_INTERVALS),
            purge: Purge::Idle(0),
            gc: Round::Idle(0),
            check: Round::Idle(lease.physical + CHECKSUM_EVERY_MS),
            tally: None,
            split: split::SplitJob::Idle,
            merge: merge::MergeJob::Idle,
            load,
            close_at: Nanos::ZERO,
            log_close_at: Nanos::ZERO,
            streams_until: Nanos::ZERO,
            last_closed: (ZERO, 0),
            fencing: Vec::new(),
            recount: recount::RecountJob::default(),
            read_outside_at: None,
        });
        let lost: Vec<ReqId> = self
            .reqs
            .iter()
            .filter(|(_, r)| match r {
                Req::Mut(MutReq { g: rg, state: MutState::Latch | MutState::Read(_), .. })
                | Req::Get(GetReq {
                    g: rg,
                    state: GetState::Admit(_) | GetState::Read(_) | GetState::Scanning(..),
                    ..
                }) => *rg == g,
                // A read waiting for the core is answered by the core.
                Req::Changes(f) => f.g == g && f.state != feed::FeedState::Core,
                Req::Mut(_) | Req::Get(_) | Req::Purge(_) | Req::Round(_) | Req::Fence(_) => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            self.reqs.remove(&id);
            self.reply(id, Err(RangeError::NotLeader { hint: st.leader }));
        }
        true
    }

    /// Moves request `id` as far as it can; `true` if it moved.
    fn step<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        now: Nanos,
    ) -> bool {
        let req = self.reqs.remove(&id).expect("a request of this host");
        match req {
            Req::Mut(m) => self.step_mut(core, id, m, now),
            Req::Get(q) => self.step_get(core, id, q),
            Req::Changes(f) => self.step_changes(core, id, f),
            // The core answers for them.
            r @ (Req::Purge(_) | Req::Round(_) | Req::Fence(_)) => {
                self.reqs.insert(id, r);
                false
            }
        }
    }

    fn step_get<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut q: GetReq,
    ) -> bool {
        let mut moved = false;
        if matches!(q.state, GetState::Clock) {
            // A mark the client gave (008 FR-008, FR-012) is served once this node's wall clock
            // reaches it (its HLC may already be past it, carried by a faster node), as a mark of
            // its own would be, and the leadership is confirmed only then. A lease confirmed
            // when the read arrived may end while the clock catches up, and a leader elected
            // after it starts its read cache only 250 ms above its own clock, so it may write
            // below the mark (seed 1069 of `special-collections`).
            match self.clock_at(&q) {
                ClockAt::Reached => {
                    let now = self.now;
                    self.confirm(core, id, q, now);
                }
                ClockAt::Behind => {
                    self.reqs.insert(id, Req::Get(q));
                    return false;
                }
                ClockAt::TooFar => self.reply(id, Err(RangeError::Timeout)),
            }
            return true;
        }
        if let GetState::Admit(ts) = q.state {
            // The leadership was confirmed with the clock at or past the mark; a wall clock that
            // stepped back since waits and confirms again. The HLC takes the mark, so this
            // node's later writes go above it.
            match self.clock_at(&q) {
                ClockAt::Reached => {}
                ClockAt::Behind => {
                    self.reqs.insert(id, Req::Get(GetReq { state: GetState::Clock, ..q }));
                    return true;
                }
                ClockAt::TooFar => {
                    self.reply(id, Err(RangeError::Timeout));
                    return true;
                }
            }
            if q.at.is_some() {
                self.clock.0.update(ts);
            }
            let Some(l) = self.ranges.get_mut(&q.g).and_then(|r| r.leader.as_mut()) else {
                // The leadership went between the core's answer and the admission.
                let hint = core.status(q.g).map_or(NodeId(0), |s| s.leader);
                self.reply(id, Err(RangeError::NotLeader { hint }));
                return true;
            };
            // Below the range's threshold when the read arrived, or below the one handed to the LSM
            // since (it may have gone up while the read waited), the compaction may have dropped the
            // versions the read needs (FR-013).
            let handed = self.gc_handed.iter().flatten().find(|(r, _)| *r == q.g).map(|(_, t)| *t);
            let floor = handed.map_or(q.gc_floor, |t| t.max(q.gc_floor));
            if ts < floor {
                self.reply(id, Err(RangeError::SnapshotTooOld));
                return true;
            }
            let end =
                q.scan.as_ref().map_or_else(|| [q.key.as_slice(), &[0]].concat(), |s| s.0.clone());
            // A mark the node chose may go just below the writes in flight instead of waiting
            // for them (DV-010); a mark the client gave is read where it was given.
            let (wait, ts) = match q.at {
                None => match admit_own_read(&l.latches, &mut l.cache, &q.key, &end, ts, q.concern)
                {
                    (wait, at) if at >= floor => (wait, at),
                    _ => (admit_read(&l.latches, &mut l.cache, &q.key, &end, ts, q.concern), ts),
                },
                Some(_) => (admit_read(&l.latches, &mut l.cache, &q.key, &end, ts, q.concern), ts),
            };
            if wait != ReadWait::Free {
                if !q.waited {
                    q.waited = true;
                    self.count_wait(q.g);
                }
                self.reqs.insert(id, Req::Get(q));
                return false;
            }
            q.state = match window_of(&q).filter(|limit| *limit > ts) {
                Some(limit) => {
                    let w = Window::new(&self.lsm, (&q.key, &end), (ts, limit), q.view.own);
                    GetState::Window(ts, Box::new(w))
                }
                None => reading(&self.lsm, &q, ts),
            };
            moved = true;
        }
        if let GetState::Window(ts, w) = &mut q.state {
            let ts = *ts;
            match w.step(&self.lsm) {
                None => {
                    self.reqs.insert(id, Req::Get(q));
                    return moved;
                }
                Some(Ok(None)) => q.state = reading(&self.lsm, &q, ts),
                Some(Ok(Some(at))) => {
                    self.reply(id, Err(RangeError::ReadUncertain { ts: at }));
                    return true;
                }
                Some(Err(e)) => {
                    self.reply(id, Err(read_fault(e, ts)));
                    return true;
                }
            }
        }
        let limit = q.scan.as_ref().map_or(0, |s| s.1);
        let window = window_of(&q);
        let outcome = match &mut q.state {
            GetState::Read(ts) => {
                read_visible(&self.lsm, &q.key, (*ts, window), &q.view).map(|r| {
                    r.map(|value| Reply::Value { ts: *ts, value }).map_err(|e| read_fault(e, *ts))
                })
            }
            GetState::Scanning(scan) => {
                let at = scan.read_ts();
                scan.step(&self.lsm, &q.view, limit)
                    .map(|r| r.map(Reply::Batch).map_err(|e| read_fault(e, at)))
            }
            GetState::Clock | GetState::Core | GetState::Admit(_) | GetState::Window(..) => None,
        };
        match outcome {
            Some(r) => {
                self.reply(id, r);
                true
            }
            None => {
                self.reqs.insert(id, Req::Get(q));
                moved
            }
        }
    }
}

/// The refusal of a write request whose keys this range does not all hold. A mutation routed by
/// its first key, which the range holds, with another key in another range needs a transaction
/// across ranges, which the layer above runs: an `OutOfRange` would send it back here, by its
/// first key, until its deadline. Anything else goes to the ranges again (`OutOfRange`).
fn crossing(
    keys: &[Vec<u8>],
    covers: impl Fn(&[u8]) -> bool,
    work: &Work,
) -> Result<Reply, RangeError> {
    let first_here = keys.first().is_some_and(|k| covers(k));
    match (work, first_here, keys.iter().find(|k| !covers(k))) {
        (Work::Mutation(_), true, Some(k)) => {
            let msg =
                format!("{CROSSES_RANGES}: {k:02x?}, which needs a transaction across ranges");
            Err(RangeError::Mutation(MutationError(msg)))
        }
        _ => Err(RangeError::OutOfRange),
    }
}
