//! What the transaction workloads share (008 T004, FR-016): every node holds a replica of two
//! ranges on one core, `[01, 01 6d)` and `[01 6d, 03)` (the second one holds the transaction
//! records too), with its `RangeHost`. Each client runs one transaction at a time as a script of
//! reads and writes with the coordinator of `teseladb-txn`, which lives with the client. A read
//! goes to the leader of its key's range at the transaction's read mark; one that meets an intent
//! pushes its transaction above the mark and reads again knowing its state, and resolves the
//! intents of a finished one (008 FR-003, FR-005). Requests in flight on a node that crashes get
//! `Unavailable`, an uncertain answer.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_multiraft::{Done, GroupDesc, OpError, OpId};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::Put;
use teseladb_range::host::{
    Mutation, MutationError, RangeError, RangeHost, ReadSpec, Reply, ReqId, TxnOp, TxnReq, WriteReq,
};
use teseladb_range::intents::{IntentOp, Push};
use teseladb_range::mutate::Outcome as Change;
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_txn::coordinator::{Coordinator, Event, Outcome, ReqNo, Route};
use teseladb_txn::read::{lazy_resolve, learn, push_for};
use teseladb_txn::reaper::Reaper;
use teseladb_txn::recovery::{Decision, Recovery, client_txn_id};
use teseladb_types::{GroupId, Nanos, NodeId};

use teseladb_raftlog::Completions;

use crate::checkers::Violation;
use crate::node::{NodeOutput, SmKind};
use crate::workloads::Workload;
use crate::workloads::data_ship::Shipper;
use crate::world::{Event as WorldEvent, World};

/// The two ranges' first keys.
const START: &[u8] = b"\x01";
/// Where the second range starts.
pub const CUT: &[u8] = b"\x01m";
const END: &[u8] = b"\x03";
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };

/// One step of a client's transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Reads a key at the transaction's mark.
    Read(Vec<u8>),
    /// Writes a value computed from what the transaction read so far.
    Write(Vec<u8>),
    /// Asks the script for the steps that follow, from what the transaction read so far.
    Expand,
    /// Commits.
    Commit,
    /// Writes the key outside the transaction with the value it has (008 T032): a plain write
    /// that waits for the transactions whose intents it meets, pushing them as abandoned, and
    /// never fails because of them. A transaction with intents must not run it: it would wait
    /// for itself.
    Touch(Vec<u8>),
    /// A plain write of a document mutation of `teseladb-wire` (an encoded `Bound`), outside the
    /// transaction, routed by its first key: its answer goes to [`Script::mutated`] (015 T002).
    Mutate(Vec<u8>),
}

/// Name of the check that no plain write fails because of an intent (008 FR-002, T032).
pub const PLAIN_WRITES: &str = "plain_writes";
/// Checker of the commits recovered on another node (008): a recovery that answered «not
/// committed» for a transaction that committed, or «committed» for one that did not.
pub const TXN_RECOVERY: &str = "txn_recovery";
/// Share of the transactions whose commit another client recovers while they run, as a driver
/// that sends `commitTransaction` again to another router (008): the recovery may come before the
/// record exists, while it runs or after it ended.
const RECOVERY_RACE: f64 = 0.1;
/// How often the reaper reads the records (008 FR-007 and record GC), in the run's time.
const REAP_EVERY: Nanos = Nanos::from_millis(500);
/// How long a client transaction's record stays after its last activity: RefDb's 30 minutes
/// would outlast a run, so the reaper also removes records by age within it; most go earlier,
/// once their session's next transaction exists. The checkers run with them gone.
const RECORD_TTL_MS: u64 = 10_000;
/// How far the reaper's clock may be from the run's real time when it judges a record's age: the
/// heavy plan jumps clocks by up to 2 s.
const CLOCK_SLACK: Nanos = Nanos::from_millis(3_000);

/// A plain write that rewrites key `.0` with the value it has.
struct Touch(Vec<u8>);

impl Mutation for Touch {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Change, MutationError> {
        let puts = current
            .first()
            .copied()
            .flatten()
            .map(|(value_type, value)| Put {
                key: self.0.clone(),
                value_type,
                value: value.to_vec(),
            })
            .into_iter()
            .collect();
        Ok(Change { puts, deletes: Vec::new(), change: Vec::new(), result: Vec::new() })
    }
}

/// What a workload decides for its transactions.
pub trait Script {
    /// The steps of a client's next transaction.
    fn plan(&mut self, w: &mut World, client: u32) -> Vec<Step>;
    /// The value a transaction writes at `key`, from what it read (`None`: deletes it).
    fn value(
        &mut self,
        client: u32,
        key: &[u8],
        read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>>;
    /// The steps that follow a [`Step::Expand`], from what the transaction read so far.
    fn expand(&mut self, _client: u32, _read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<Step> {
        Vec::new()
    }
    /// The answer to a [`Step::Mutate`] of a client: the mutation's result, or the range's error.
    fn mutated(&mut self, _client: u32, _r: Result<Vec<u8>, RangeError>) {}
    /// The fault window ended.
    fn healed(&mut self) {}
    /// The value type a write at `key` stores.
    fn value_type(&self, _key: &[u8]) -> u8 {
        1
    }
    /// A transaction ended: what it read and wrote, in order, and how it ended.
    fn ended(&mut self, client: u32, done: Finished);
    /// At the end of the run: the workload's checks.
    fn finish(&mut self) -> Result<(), Violation>;
}

/// A read or a write of a transaction: key and value (`None`: absent, or deleted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// A read.
    Read(Vec<u8>, Option<Vec<u8>>),
    /// A write.
    Write(Vec<u8>, Option<Vec<u8>>),
}

/// A transaction as it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// Its reads and writes, in order.
    pub ops: Vec<Op>,
    /// Its read mark.
    pub read_ts: Hlc,
    /// How it ended.
    pub outcome: Outcome,
    /// When it began, in the run's real time.
    pub began: Nanos,
    /// When its commit was acknowledged, in the run's real time.
    pub acked: Option<Nanos>,
    /// Whether the clocks had stayed within the uncertainty window of one another from the start
    /// of the run until it began: only then does it take part in the real-time order (008 FR-008).
    pub in_window: bool,
}

/// Keys below the cut ([`CUT`] unless the workload sets another) are in the first range, the
/// rest in the second.
struct Ranges(GroupId, GroupId, &'static [u8]);

impl Route for Ranges {
    fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        Some(if key < self.2 { self.0 } else { self.1 })
    }
}

/// What a client waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Waiting {
    Nothing,
    Read(Vec<u8>),
    Push(Vec<u8>),
    Written,
    Ended,
    /// A plain write of the key.
    Touch(Vec<u8>),
    /// A plain write of a document mutation.
    Mutated,
}

struct Client {
    coord: Coordinator,
    node: u32,
    steps: Vec<Step>,
    at: usize,
    view: TxnView,
    read: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    done: Finished,
    waiting: Waiting,
    ended: bool,
}

/// Who waits for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asker {
    Coord(u32, ReqNo),
    Read(u32),
    Push(u32),
    /// A client's plain write.
    Touch(u32),
    /// The push of the owner of an intent a client's plain write met.
    TouchPush(u32),
    /// A client's plain write of a document mutation.
    Mutate(u32),
    /// A lazy resolution nobody waits for.
    Nobody,
    /// A recovery of a transaction's decision, and its request.
    Recover(u64, ReqNo),
    /// The reaper's read of the records.
    ReapScan,
    /// A request of the reaper.
    Reap(ReqNo),
}

/// The driver of a transaction workload.
pub struct TxnDriver {
    nodes: u32,
    core: u16,
    ranges: (GroupId, GroupId),
    /// Where the second range starts.
    cut: &'static [u8],
    hosts: BTreeMap<u32, RangeHost>,
    pending: BTreeMap<(u32, ReqId), Asker>,
    /// Plain writes that failed because of an intent.
    plain_failures: Vec<String>,
    clients: BTreeMap<u32, Client>,
    next_txn: u128,
    /// Transactions that committed, aborted or whose end is unknown.
    pub counts: (u64, u64, u64),
    /// The data of the snapshots the replicas wait for (005 FR-012).
    shipper: Shipper,
    /// Whether two clocks have been further apart than the uncertainty window since the start of
    /// the run: a jump of the `heavy` plan. The marks taken after it promise no real-time order,
    /// and the timestamps a jumped clock gave stay in the ranges' clocks after it comes back.
    clocks_apart: bool,
    /// Recoveries of transactions' decisions in flight, by number, with the transaction's session
    /// and number.
    recoveries: BTreeMap<u64, (Recovery, (u128, i64))>,
    next_recovery: u64,
    /// What the recoveries found, and how each transaction that wrote ended (committed or not;
    /// unknown ends are left out), by session and number.
    recovered: BTreeMap<(u128, i64), Decision>,
    ended: BTreeMap<(u128, i64), bool>,
    /// The reaper of the records' range, and when it next reads them (`None` while it does).
    reaper: Reaper,
    next_reap: Option<Nanos>,
    /// Transactions whose records the reaper removed: a recovery after that finds none and
    /// answers as for a number the session never began, as a `refrouter` does once the session
    /// expired, so the checker leaves those recoveries out.
    forgotten: std::collections::BTreeSet<teseladb_range::intents::TxnId>,
    /// When each transaction began and, if it did, ended; and each session's last number.
    began: BTreeMap<(u128, i64), Nanos>,
    ended_at: BTreeMap<(u128, i64), Nanos>,
    last_number: BTreeMap<u128, i64>,
    /// The first record the reaper removed without a rule that allows it.
    early: Option<String>,
}

impl TxnDriver {
    /// The driver over `nodes` nodes of `cores` cores: both ranges on one core.
    pub fn new(nodes: u32, cores: u16) -> TxnDriver {
        let first = GroupId(1);
        let second = GroupId(1 + u64::from(cores));
        TxnDriver {
            nodes,
            core: (first.0 % u64::from(cores)) as u16,
            ranges: (first, second),
            cut: CUT,
            hosts: BTreeMap::new(),
            pending: BTreeMap::new(),
            plain_failures: Vec::new(),
            clients: BTreeMap::new(),
            next_txn: 1,
            counts: (0, 0, 0),
            shipper: Shipper::new((first.0 % u64::from(cores)) as u16),
            clocks_apart: false,
            recoveries: BTreeMap::new(),
            next_recovery: 0,
            recovered: BTreeMap::new(),
            ended: BTreeMap::new(),
            reaper: Reaper::with_session_ttl(RECORD_TTL_MS),
            next_reap: Some(REAP_EVERY),
            forgotten: std::collections::BTreeSet::new(),
            began: BTreeMap::new(),
            ended_at: BTreeMap::new(),
            last_number: BTreeMap::new(),
            early: None,
        }
    }

    fn route(&self) -> Ranges {
        Ranges(self.ranges.0, self.ranges.1, self.cut)
    }

    fn open_host(&mut self, w: &World, node: u32) {
        let n = &w.nodes[node as usize];
        let lsm =
            n.cores[self.core as usize].lsm.clone().expect("a core with a range opens its LSM");
        let mut host = RangeHost::new(lsm, n.hlc.clone());
        host.add_range(self.ranges.0, START.to_vec(), self.cut.to_vec());
        host.add_range(self.ranges.1, self.cut.to_vec(), END.to_vec());
        self.hosts.insert(node, host);
    }

    /// Creates the ranges and opens the hosts.
    pub fn start(&mut self, w: &mut World) {
        w.range_lsm = Some(teseladb_lsm::LsmConfig {
            dir: "range-lsm".into(),
            memtable_bytes: 16 << 10,
            block_bytes: 4096,
            target_file_bytes: 64 << 10,
            l1_bytes: 256 << 10,
            cache_bytes: 64 << 10,
            ..teseladb_lsm::LsmConfig::default()
        });
        let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
        let (a, b) = self.ranges;
        w.add_group(
            GroupDesc { group: a, voters: voters.clone(), learners: vec![] },
            SmKind::Range(START, self.cut),
        );
        w.add_group(GroupDesc { group: b, voters, learners: vec![] }, SmKind::Range(self.cut, END));
        for node in 0..self.nodes {
            if w.nodes[node as usize].alive {
                self.open_host(w, node);
            }
        }
    }

    /// Whether the client has a transaction running.
    pub fn busy(&self, client: u32) -> bool {
        self.clients.contains_key(&client)
    }

    /// Notes whether two clocks are further apart than the uncertainty window now.
    fn watch_clocks(&mut self, w: &World) {
        let offsets = w.nodes.iter().map(|n| n.clock.offset_ns());
        let (lo, hi) = offsets.fold((i64::MAX, i64::MIN), |(lo, hi), o| (lo.min(o), hi.max(o)));
        let window = Nanos::from_millis(teseladb_txn::uncertainty::UNCERTAINTY_MS).0;
        if i128::from(hi) - i128::from(lo) > i128::from(window) {
            self.clocks_apart = true;
        }
    }

    fn times(w: &World, node: u32) -> (Nanos, u64) {
        (w.local(node), w.nodes[node as usize].clock.wall_ms(w.now()))
    }

    /// The node whose core leads `g`, if one is alive.
    fn leader(&self, w: &mut World, g: GroupId) -> Option<u32> {
        (0..self.nodes).find(|&n| {
            w.nodes[n as usize].alive
                && self.hosts.contains_key(&n)
                && w.cluster_core(n, self.core)
                    .and_then(|c| c.status(g))
                    .is_some_and(|s| s.role == teseladb_raft::Role::Leader)
        })
    }

    /// Starts a transaction of `client` on a live node with the script's steps.
    pub fn begin(&mut self, w: &mut World, client: u32, steps: Vec<Step>) {
        let node = w.rng.range(0, u64::from(self.nodes)) as u32;
        if !w.nodes[node as usize].alive || steps.is_empty() {
            return;
        }
        self.watch_clocks(w);
        let read_ts = Hlc { physical: w.wall_ms(node), logical: 0 };
        // A client's session, and one number per transaction, which gives its identifier.
        let owner = (u128::from(client) + 1, self.next_txn as i64);
        self.next_txn += 1;
        let txn = client_txn_id(owner.0, owner.1);
        let coord = Coordinator::new(txn, owner, read_ts, w.local(node));
        self.began.insert(owner, w.now());
        self.last_number.insert(owner.0, owner.1);
        if w.rng.chance(RECOVERY_RACE) {
            self.recover(owner);
        }
        let view = TxnView { own: Some(txn), known: BTreeMap::new() };
        let done = Finished {
            ops: Vec::new(),
            read_ts,
            outcome: Outcome::Aborted,
            began: w.now(),
            acked: None,
            in_window: !self.clocks_apart,
        };
        let c = Client {
            coord,
            node,
            steps,
            at: 0,
            view,
            read: BTreeMap::new(),
            done,
            waiting: Waiting::Nothing,
            ended: false,
        };
        self.clients.insert(client, c);
    }

    /// Sends `op` for `g` to its leader; with none, the asker gets `NotLeader` at once.
    fn send(
        &mut self,
        w: &mut World,
        g: GroupId,
        op: TxnOp,
        asker: Asker,
    ) -> Option<(Asker, Result<Reply, RangeError>)> {
        let Some(node) = self.leader(w, g) else {
            return Some((asker, Err(RangeError::NotLeader { hint: NodeId(0) })));
        };
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let c = w.cluster_core(node, self.core).expect("alive");
        let id = host.txn(c, TxnReq { g, op, wc: MAJORITY }, now, wall);
        self.pending.insert((node, id), asker);
        None
    }

    fn read(
        &mut self,
        w: &mut World,
        client: u32,
        key: Vec<u8>,
    ) -> Option<(Asker, Result<Reply, RangeError>)> {
        let g = self.route().range_of(&key).expect("every key has a range");
        // It waits for this read before anything else: an answer that comes at once (no leader)
        // fails the transaction instead of letting the script go on without the value.
        let c = self.clients.get_mut(&client).expect("running");
        c.waiting = Waiting::Read(key.clone());
        let Some(node) = self.leader(w, g) else {
            return Some((Asker::Read(client), Err(RangeError::NotLeader { hint: NodeId(0) })));
        };
        let c = self.clients.get_mut(&client).expect("running");
        let spec = ReadSpec {
            g,
            key,
            scan: None,
            at: Some(c.coord.read_ts()),
            concern: ReadConcern::Majority,
            view: c.view.clone(),
            // A mark from the coordinator's clock: its window (008 FR-008). A version in it fails
            // the transaction, which the client repeats.
            uncertain_to: Some(teseladb_txn::uncertainty::window_end(c.coord.read_ts())),
        };
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let core = w.cluster_core(node, self.core).expect("alive");
        let id = host.read_with(core, spec, now, wall);
        self.pending.insert((node, id), Asker::Read(client));
        None
    }

    /// Sends a client's plain write of `key` to the range's leader.
    fn touch(
        &mut self,
        w: &mut World,
        client: u32,
        key: Vec<u8>,
    ) -> Option<(Asker, Result<Reply, RangeError>)> {
        let g = self.route().range_of(&key).expect("every key has a range");
        let Some(node) = self.leader(w, g) else {
            return Some((Asker::Touch(client), Err(RangeError::NotLeader { hint: NodeId(0) })));
        };
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let core = w.cluster_core(node, self.core).expect("alive");
        let m = Box::new(Touch(key.clone()));
        let req = WriteReq { g, keys: vec![key], m, dedup: None, wc: MAJORITY };
        let id = host.mutate(core, req, now, wall);
        self.pending.insert((node, id), Asker::Touch(client));
        None
    }

    /// Sends a client's document mutation `bound` (encoded) to the leader of its first key's range.
    fn mutate(
        &mut self,
        w: &mut World,
        client: u32,
        bound: &[u8],
    ) -> Option<(Asker, Result<Reply, RangeError>)> {
        let refused =
            |e: String| Some((Asker::Mutate(client), Err(RangeError::Mutation(MutationError(e)))));
        let bound = match teseladb_wire::commands::mutation::Bound::decode(bound) {
            Ok(b) => b,
            Err(e) => return refused(e.errmsg),
        };
        let keys = bound.latch_keys();
        let Some(first) = keys.first() else { return refused("a mutation without keys".into()) };
        let g = self.route().range_of(first).expect("every key has a range");
        let Some(node) = self.leader(w, g) else {
            return Some((Asker::Mutate(client), Err(RangeError::NotLeader { hint: NodeId(0) })));
        };
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let core = w.cluster_core(node, self.core).expect("alive");
        let req = WriteReq { g, keys, m: Box::new(bound), dedup: None, wc: MAJORITY };
        let id = host.mutate(core, req, now, wall);
        self.pending.insert((node, id), Asker::Mutate(client));
        None
    }

    /// The answer to a client's document mutation: the script takes it and the client goes on.
    fn mutate_done(
        &mut self,
        w: &mut World,
        client: u32,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        if c.waiting != Waiting::Mutated {
            return;
        }
        let result = r.and_then(|reply| match reply {
            Reply::Mutated { result, .. } => Ok(result),
            other => Err(RangeError::Mutation(MutationError(format!("{other:?}")))),
        });
        script.mutated(client, result);
        c.waiting = Waiting::Nothing;
        self.advance(w, client, script);
    }

    /// The answer to a client's plain write: done, a wait for the owner of an intent, or a
    /// failure because of it, which the check reports.
    fn touch_done(
        &mut self,
        w: &mut World,
        client: u32,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let Waiting::Touch(key) = c.waiting.clone() else { return };
        match r {
            Err(RangeError::IntentConflict(owner)) => {
                let g = self.route().range_of(&owner.record_key).expect("every key has a range");
                let push = Push::AbortIfAbandoned { intent_ts: owner.ts };
                let op = TxnOp::PushTxn { txn: owner.txn, push };
                if let Some((asker, r)) = self.send(w, g, op, Asker::TouchPush(client)) {
                    self.answer(w, asker, r, script);
                }
                return;
            }
            Err(RangeError::WriteConflict) => {
                let detail = format!("t={:?} client {client} key {key:?}", w.now());
                self.plain_failures.push(detail);
            }
            // Done, or refused for another reason: the write is given up, and nothing checked.
            Ok(_) | Err(_) => {}
        }
        c.waiting = Waiting::Nothing;
        self.advance(w, client, script);
    }

    /// The owner of the intent a client's plain write met was pushed: a finished one has its
    /// intent resolved; then the write goes again.
    fn touch_pushed(
        &mut self,
        w: &mut World,
        client: u32,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        let Some(c) = self.clients.get(&client) else { return };
        let Waiting::Touch(key) = c.waiting.clone() else { return };
        if let Ok(Reply::Record(rec)) = r
            && let Some(op) = lazy_resolve(&rec, vec![key.clone()])
        {
            let g = self.route().range_of(&key).expect("every key has a range");
            let _ = self.send(w, g, op, Asker::Nobody);
        }
        if let Some((asker, r)) = self.touch(w, client, key) {
            self.answer(w, asker, r, script);
        }
    }

    /// Moves a client's transaction on: its next step, once the last one is answered.
    fn advance(&mut self, w: &mut World, client: u32, script: &mut dyn Script) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        if c.waiting != Waiting::Nothing {
            return;
        }
        let Some(step) = c.steps.get(c.at).cloned() else { return };
        c.at += 1;
        match step {
            Step::Read(key) => {
                if let Some((asker, r)) = self.read(w, client, key) {
                    self.answer(w, asker, r, script);
                }
            }
            Step::Write(key) => {
                let value = script.value(client, &key, &c.read);
                let value_type = script.value_type(&key);
                let op = value
                    .clone()
                    .map_or(IntentOp::Delete, |v| IntentOp::Put { value_type, value: v });
                c.done.ops.push(Op::Write(key.clone(), value));
                let route = Ranges(self.ranges.0, self.ranges.1, self.cut);
                match c.coord.write(&route, vec![(key, op)]) {
                    Ok(_) => c.waiting = Waiting::Written,
                    Err(_) => self.fail(w, client),
                }
            }
            Step::Expand => {
                let more = script.expand(client, &c.read);
                let at = c.at;
                c.steps.splice(at..at, more);
                return self.advance(w, client, script);
            }
            Step::Touch(key) => {
                c.waiting = Waiting::Touch(key.clone());
                if let Some((asker, r)) = self.touch(w, client, key) {
                    self.answer(w, asker, r, script);
                }
                return;
            }
            Step::Mutate(bound) => {
                c.waiting = Waiting::Mutated;
                if let Some((asker, r)) = self.mutate(w, client, &bound) {
                    self.answer(w, asker, r, script);
                }
                return;
            }
            Step::Commit => {
                let clock = Hlc { physical: w.wall_ms(c.node), logical: 0 };
                c.waiting = Waiting::Ended;
                c.coord.commit(clock);
            }
        }
        self.flush(w, client, script);
    }

    /// Aborts a client's transaction after a failed step.
    fn fail(&mut self, _w: &mut World, client: u32) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        c.waiting = Waiting::Ended;
        c.coord.abort();
    }

    /// Sends the coordinator's requests and handles its events.
    fn flush(&mut self, w: &mut World, client: u32, script: &mut dyn Script) {
        loop {
            let Some(c) = self.clients.get_mut(&client) else { return };
            let reqs = c.coord.take_requests();
            let events = c.coord.take_events();
            if reqs.is_empty() && events.is_empty() {
                return;
            }
            let mut now_answers = Vec::new();
            for r in reqs {
                if let Some(a) = self.send(w, r.g, r.op, Asker::Coord(client, r.id)) {
                    now_answers.push(a);
                }
            }
            for e in events {
                self.event(w, client, e, script);
            }
            for (asker, r) in now_answers {
                self.answer(w, asker, r, script);
            }
        }
    }

    fn event(&mut self, w: &mut World, client: u32, e: Event, script: &mut dyn Script) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let mut settled = None;
        match e {
            Event::Written { result: Ok(()), .. } => {
                c.waiting = Waiting::Nothing;
                self.advance(w, client, script);
            }
            Event::Written { result: Err(_), .. } => self.fail(w, client),
            Event::Ended(outcome) => {
                c.ended = true;
                c.done.outcome = outcome.clone();
                let owner = c.coord.session();
                // A transaction that only read has no decision to recover: its client's
                // `recoveryToken` is empty (008).
                let wrote = c.done.ops.iter().any(|o| matches!(o, Op::Write(..)));
                let committed = match &outcome {
                    Outcome::Committed(_) => Some(true),
                    Outcome::Aborted | Outcome::Failed(_) => Some(false),
                    Outcome::Unknown(_) => None,
                };
                settled = wrote.then_some((owner, committed));
                match outcome {
                    Outcome::Committed(_) => {
                        c.done.acked = Some(w.now());
                        self.counts.0 += 1;
                    }
                    Outcome::Aborted | Outcome::Failed(_) => self.counts.1 += 1,
                    Outcome::Unknown(_) => self.counts.2 += 1,
                }
            }
            Event::Resolved => {}
        }
        if let Some((owner, _)) = settled {
            self.ended_at.insert(owner, w.now());
        }
        if let Some((owner, committed)) = settled {
            match committed {
                Some(c) => {
                    self.ended.insert(owner, c);
                }
                // Its client asks again on another node, as a driver does.
                None => self.recover(owner),
            }
        }
        self.retire(client, script);
    }

    /// A client whose transaction ended is done with it; the resolutions its coordinator sent
    /// go on without it, and readers resolve what they leave (008 FR-005).
    fn retire(&mut self, client: u32, script: &mut dyn Script) {
        if self.clients.get(&client).is_some_and(|c| c.ended) {
            let c = self.clients.remove(&client).expect("checked");
            self.pending
                .retain(|_, a| !matches!(a, Asker::Read(cl) | Asker::Push(cl) if *cl == client));
            script.ended(client, c.done);
        }
    }

    /// The answer to a request.
    fn answer(
        &mut self,
        w: &mut World,
        asker: Asker,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        match asker {
            Asker::Nobody => {}
            Asker::Coord(client, no) => {
                if let Some(c) = self.clients.get_mut(&client) {
                    c.coord.on_reply(&Ranges(self.ranges.0, self.ranges.1, self.cut), no, r);
                    self.flush(w, client, script);
                }
            }
            Asker::ReapScan => self.reap_scanned(w, r),
            Asker::Reap(no) => {
                self.reaper.on_reply(&self.route(), no, r);
                self.reap_send(w);
            }
            Asker::Recover(id, no) => {
                if let Some((rec, _)) = self.recoveries.get_mut(&id) {
                    rec.on_reply(no, r);
                }
            }
            Asker::Read(client) => self.read_done(w, client, r, script),
            Asker::Push(client) => self.push_done(w, client, r, script),
            Asker::Touch(client) => self.touch_done(w, client, r, script),
            Asker::TouchPush(client) => self.touch_pushed(w, client, r, script),
            Asker::Mutate(client) => self.mutate_done(w, client, r, script),
        }
    }

    fn read_done(
        &mut self,
        w: &mut World,
        client: u32,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let Waiting::Read(key) = c.waiting.clone() else { return };
        match r {
            Ok(Reply::Value { value, .. }) => {
                let v = value.map(|(_, v)| v);
                c.read.insert(key.clone(), v.clone());
                c.done.ops.push(Op::Read(key, v));
                c.waiting = Waiting::Nothing;
                self.advance(w, client, script);
            }
            Err(RangeError::ReadIntent { owner, .. }) => {
                c.waiting = Waiting::Push(key);
                let op = push_for(&owner, c.coord.read_ts());
                let g = self.route().range_of(&owner.record_key).expect("every key has a range");
                if let Some((asker, r)) = self.send(w, g, op, Asker::Push(client)) {
                    self.answer(w, asker, r, script);
                }
            }
            Ok(_) | Err(_) => {
                c.waiting = Waiting::Nothing;
                self.fail(w, client);
                self.flush(w, client, script);
            }
        }
    }

    fn push_done(
        &mut self,
        w: &mut World,
        client: u32,
        r: Result<Reply, RangeError>,
        script: &mut dyn Script,
    ) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let Waiting::Push(key) = c.waiting.clone() else { return };
        let Ok(Reply::Record(rec)) = r else {
            c.waiting = Waiting::Nothing;
            self.fail(w, client);
            return self.flush(w, client, script);
        };
        c.view.known.insert(rec.txn, learn(&rec));
        if let Some(op) = lazy_resolve(&rec, vec![key.clone()]) {
            let g = self.route().range_of(&key).expect("every key has a range");
            let _ = self.send(w, g, op, Asker::Nobody);
        }
        if let Some((asker, r)) = self.read(w, client, key) {
            self.answer(w, asker, r, script);
        }
    }

    /// Delivers the hosts' answers and lets the coordinators beat.
    pub fn pump(&mut self, w: &mut World, script: &mut dyn Script) {
        self.watch_clocks(w);
        self.shipper.step(w);
        for node in 0..self.nodes {
            if !w.nodes[node as usize].alive {
                continue;
            }
            let (now, wall) = Self::times(w, node);
            let core = self.core;
            let Some(host) = self.hosts.get_mut(&node) else { continue };
            let Some(c) = w.cluster_core(node, core) else { continue };
            host.poll(c, now, wall);
            host.follow_state(c);
            let _ = host.take_checksum_reports();
            for (req, r) in host.take_replies() {
                if let Some(asker) = self.pending.remove(&(node, req)) {
                    self.answer(w, asker, r, script);
                }
            }
        }
        self.step_recoveries(w);
        self.reap(w);
        let clients: Vec<u32> = self.clients.keys().copied().collect();
        for client in clients {
            let route = self.route();
            if let Some(c) = self.clients.get_mut(&client) {
                let (now, wall) = (w.local(c.node), w.wall_ms(c.node));
                c.coord.tick(&route, now, wall);
            }
            self.flush(w, client, script);
            self.advance(w, client, script);
        }
    }

    /// A core's operation finished.
    pub fn on_done(
        &mut self,
        w: &mut World,
        (node, core): (u32, u16),
        op: OpId,
        r: Result<Done<NodeOutput>, OpError>,
    ) {
        if core != self.core {
            return;
        }
        let Some(host) = self.hosts.get_mut(&node) else { return };
        // The core answered before it froze, if it is frozen now: the answer is not lost.
        let Some(c) = w.paused_core(node, core) else { return };
        host.on_done(c, op, r);
    }

    /// A node crashes: what it had in flight gets an uncertain answer.
    pub fn on_crash(&mut self, w: &mut World, node: u32, script: &mut dyn Script) {
        self.hosts.remove(&node);
        self.shipper.on_crash(node);
        let lost: Vec<Asker> =
            self.pending.iter().filter(|((n, _), _)| *n == node).map(|(_, a)| *a).collect();
        self.pending.retain(|(n, _), _| *n != node);
        for asker in lost {
            self.answer(w, asker, Err(RangeError::Unavailable), script);
        }
    }

    /// A node is back.
    pub fn on_restart(&mut self, w: &mut World, node: u32) {
        for core in 0..w.nodes[node as usize].cores.len() as u16 {
            w.settle(node, core);
        }
        self.open_host(w, node);
    }

    /// Reads the records on their range's leader when it is time.
    fn reap(&mut self, w: &mut World) {
        if self.next_reap.is_none_or(|at| w.now() < at) {
            return;
        }
        let g = self.ranges.1;
        let Some(node) = self.leader(w, g) else { return };
        self.next_reap = None;
        let spec = ReadSpec {
            g,
            key: vec![teseladb_range::intents::TXN_SPACE],
            scan: Some((vec![teseladb_range::intents::TXN_SPACE + 1], usize::MAX)),
            at: None,
            concern: ReadConcern::Local,
            view: TxnView::default(),
            uncertain_to: None,
        };
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let core = w.cluster_core(node, self.core).expect("alive");
        let id = host.read_with(core, spec, now, wall);
        self.pending.insert((node, id), Asker::ReapScan);
    }

    /// The records read: the reaper looks at them at the reading node's clock.
    fn reap_scanned(&mut self, w: &mut World, r: Result<Reply, RangeError>) {
        self.next_reap = Some(w.now() + REAP_EVERY);
        let Ok(Reply::Batch(rows)) = r else { return };
        let records = rows
            .iter()
            .filter_map(|(_, (_, v))| teseladb_range::intents::TxnRecord::decode_row(v).ok());
        let node = self.leader(w, self.ranges.1).unwrap_or(0);
        let now = Hlc { physical: w.wall_ms(node), logical: 0 };
        self.reaper.scan(&self.route(), now, records);
        self.reap_send(w);
    }

    /// Sends what the reaper asks; an answer that comes at once goes back to it.
    fn reap_send(&mut self, w: &mut World) {
        loop {
            for txn in self.reaper.take_forgotten() {
                self.check_removal(w, txn);
                self.forgotten.insert(txn);
            }
            let reqs = self.reaper.take_requests();
            if reqs.is_empty() {
                return;
            }
            for q in reqs {
                if let Some((_, answer)) = self.send(w, q.g, q.op, Asker::Reap(q.id)) {
                    self.reaper.on_reply(&self.route(), q.id, answer);
                }
            }
        }
    }

    /// Record GC (008): a client transaction's record goes only once its session has a later
    /// transaction or its session's time has passed since the transaction's last known activity.
    fn check_removal(&mut self, w: &World, txn: teseladb_range::intents::TxnId) {
        if self.early.is_some() {
            return;
        }
        let Some(&owner) = self.began.keys().find(|&&(l, n)| client_txn_id(l, n) == txn) else {
            return;
        };
        let later = self.last_number.get(&owner.0).is_some_and(|n| *n > owner.1);
        let last = self.ended_at.get(&owner).or(self.began.get(&owner)).copied();
        let ttl = Nanos::from_millis(RECORD_TTL_MS);
        let expired = last.is_some_and(|at| w.now() + CLOCK_SLACK >= at + ttl);
        if !later && !expired {
            self.early = Some(format!(
                "the record of transaction {} of session {} went at {} ms with no later \
                 transaction of its session and its session's time not over",
                owner.1,
                owner.0,
                w.now().as_millis()
            ));
        }
    }

    /// Starts the recovery of the decision on transaction `owner` (session and number).
    fn recover(&mut self, owner: (u128, i64)) {
        let id = self.next_recovery;
        self.next_recovery += 1;
        self.recoveries.insert(id, (Recovery::new(owner.0, owner.1), owner));
    }

    /// Sends what the recoveries ask and keeps what they found.
    fn step_recoveries(&mut self, w: &mut World) {
        let route = self.route();
        let ids: Vec<u64> = self.recoveries.keys().copied().collect();
        for id in ids {
            let Some((rec, owner)) = self.recoveries.get_mut(&id) else { continue };
            if let Some(d) = rec.decision() {
                let owner = *owner;
                self.recoveries.remove(&id);
                // A record removed by a rule that allows it is found by no recovery after: it
                // answers «unknown», as a `refrouter` does once the session expired.
                let gone = self.forgotten.contains(&client_txn_id(owner.0, owner.1));
                if !(gone && d == Decision::Unknown) {
                    self.recovered.insert(owner, d);
                }
                continue;
            }
            rec.tick(&route);
            for r in rec.take_requests() {
                if let Some((_, answer)) = self.send(w, r.g, r.op, Asker::Recover(id, r.id))
                    && let Some((rec, _)) = self.recoveries.get_mut(&id)
                {
                    rec.on_reply(r.id, answer);
                }
            }
        }
    }

    /// FR-009, the commit recovered on another node: every recovery agrees with how its
    /// transaction ended, where both are known.
    pub fn check_recoveries(&self) -> Result<(), Violation> {
        if let Some(detail) = &self.early {
            return Err(Violation {
                checker: TXN_RECOVERY,
                detail: detail.clone(),
                liveness: false,
            });
        }
        for (owner, d) in &self.recovered {
            let Some(&committed) = self.ended.get(owner) else { continue };
            if matches!(d, Decision::Committed(_)) != committed {
                let detail = format!(
                    "transaction {} of session {}: the recovery found {d:?}, and it {}",
                    owner.1,
                    owner.0,
                    if committed { "committed" } else { "did not commit" }
                );
                return Err(Violation { checker: TXN_RECOVERY, detail, liveness: false });
            }
        }
        Ok(())
    }

    /// Schedules a client's next turn.
    pub fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(30, 120));
        w.schedule(at, WorldEvent::Client { client });
    }
}

/// A transaction workload: the driver and the workload's script.
pub struct TxnWorkload<S: Script> {
    name: &'static str,
    driver: TxnDriver,
    script: S,
    clients: u32,
}

impl<S: Script> TxnWorkload<S> {
    /// The workload `name` with `clients` clients over `nodes` nodes of `cores` cores.
    pub fn new(name: &'static str, (nodes, cores): (u32, u16), clients: u32, script: S) -> Self {
        TxnWorkload { name, driver: TxnDriver::new(nodes, cores), script, clients }
    }

    /// The workload with its second range starting at `cut`.
    pub fn with_cut(mut self, cut: &'static [u8]) -> Self {
        self.driver.cut = cut;
        self
    }

    /// Transactions committed, aborted and of unknown end so far.
    pub fn counts(&self) -> (u64, u64, u64) {
        self.driver.counts
    }
}

impl<S: Script> Workload for TxnWorkload<S> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        self.driver.start(w);
        for client in 0..self.clients {
            TxnDriver::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.driver.busy(client) {
            let steps = self.script.plan(w, client);
            self.driver.begin(w, client, steps);
            self.driver.pump(w, &mut self.script);
        }
        TxnDriver::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        for (node, core, op, r) in std::mem::take(&mut w.events) {
            self.driver.on_done(w, (node, core), op, r);
        }
        self.driver.pump(w, &mut self.script);
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.on_completions(w, node, 0, Completions::default());
        self.driver.on_crash(w, node, &mut self.script);
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.driver.on_restart(w, node);
        Ok(())
    }

    fn on_heal(&mut self, _w: &mut World) {
        self.script.healed();
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        if let Some(first) = self.driver.plain_failures.first() {
            let n = self.driver.plain_failures.len();
            return Err(Violation {
                checker: PLAIN_WRITES,
                detail: format!("{n} plain writes failed because of an intent; the first: {first}"),
                liveness: false,
            });
        }
        self.driver.check_recoveries()?;
        self.script.finish()
    }
}
