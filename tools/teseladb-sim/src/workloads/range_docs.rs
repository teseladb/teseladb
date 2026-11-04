//! `range-docs` workload (005 T016, FR-019, SC-001): every node holds a replica of one range of
//! documents over its core's LSM, with its `RangeHost`. Clients read documents (with the lease or
//! ReadIndex, `majority`, `linearizable` and `local`) and insert, compare-and-swap, increment,
//! replace and delete them (`w: "majority"`) through the node they believe leads, while the fault
//! plan crashes nodes, partitions the network, freezes cores, slows and fills disks and moves
//! clocks. A replica that falls behind the log receives the range by export and ingestion
//! (FR-012). Each document is a register of unique tags: the history goes to the progress,
//! durability and linearizability checkers.
//!
//! Every write is a retryable write (FR-011): it carries a key unique to the operation and, when
//! its answer is lost (`Timeout`, `Unavailable`, or its node crashed), the client sends it once
//! more with the same key, as a driver does. The range answers the retry with the original's
//! result if the original was applied, so the history op takes the retry's answer; a write
//! applied twice would show as a cas from its own tag.
//!
//! `follower-reads` (007 T025, FR-012) is the same workload with a share of reads sent to any
//! replica without its leader, at the highest closed mark it can read at or at the mark of the
//! client's last read. They are stale by design, so they stay out of the linearizability history:
//! the mvcc rules judge them. A read at a mark never misses a version the majority acknowledged at
//! or below it, a later write never goes at or below it, and a read again at a mark sees what an
//! earlier one saw there, whichever replica answers.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use teseladb_keys::Hlc;
use teseladb_lsm::{FileRef, LsmError};
use teseladb_multiraft::{Done, GroupDesc, OpError, OpId, StateMachine};
use teseladb_raftlog::{Completions, Disk, FileId, IoOp, SimDisk};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::checksum::Report;
use teseladb_range::command::{DedupKey, Put};
use teseladb_range::host::{
    Mutation, MutationError, RangeError, RangeHost, ReadSpec, Reply, ReqId, WriteReq,
};
use teseladb_range::mutate::Outcome;
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_range::rollback::{ROLLBACK_DIR, RollbackDoc, read_rollback_file};
use teseladb_range::snapshot::{Assembler, local_interval, split, write_ops};
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::checkers::{Violation, durability, linearizable, liveness};
use crate::history::{CAS_MISMATCH, History, OpKind, OpRecord, OpResult};
use crate::node::{NodeOutput, SmKind};
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The range.
const G: GroupId = GroupId(1);
const START: &[u8] = b"\x01";
const END: &[u8] = b"\x02";
/// Documents: 8 keep each one under the linearizability checker's 300 operations in the
/// `heavy` plan (with 4, 8 seeds in 5 000 went over).
const DOCS: u64 = 8;
/// Clients.
const CLIENTS: u32 = 4;
/// Every timestamp is above this one.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
/// `w: 1` with the journal (D-034): acknowledged once on the leader's disk, before the majority has
/// it, so a new leader may roll it back; it then has to be in a rollback file (FR-010). Without the
/// journal a crash before the leader's fsync loses it with no file, in RefDb too.
const W1: WriteConcern = WriteConcern { w: W::Nodes(1), j: true };
/// Share of the writes that go to the document their client last read.
const READ_THEN_WRITE: f64 = 0.5;
/// In `follower-reads`, share of the turns that read on any replica without the leader, and of
/// those, the share at the mark of the client's last read of the document.
const FOLLOWER_SHARE: f64 = 0.3;
const FOLLOWER_AT_MARK: f64 = 0.5;
/// Share of the writes that go with [`W1`].
const W1_SHARE: f64 = 0.1;
/// While a leader at the edge of the clock margin is cut off (`LeaderClockEdge`, heavy plan), its
/// clients go on reading through it until its lease ends, and then write the documents they read
/// there through the leader that follows it, as soon as there is one: a write below those reads
/// shows at once (005 FR-005, variant `write_below_read`). Share of the turns that do so.
const EDGE_SHARE: f64 = 0.8;
/// How long after the cut the clients write the documents read through the cut-off leader.
const EDGE_WRITE_WINDOW: Nanos = Nanos::from_millis(600);
/// Least and greatest wait, in ms, between a client's turns meanwhile; the other turns do nothing
/// then, so that the documents stay within the linearizability checker's operations.
const EDGE_TURN_MS: (u64, u64) = (5, 15);
/// `error` of the history op of a write acknowledged with [`W1`]: an `info`, since it may be
/// rolled back.
pub use crate::history::W1_ACKNOWLEDGED;
/// Handles of the files of an incoming snapshot.
const INCOMING_FILES: u32 = 1 << 20;
/// Least and greatest wait, in ms, before a write whose answer was lost goes again; the retry
/// leaves at the client's first turn after it.
const RETRY_DELAY_MS: (u64, u64) = (5, 40);
/// How long a retry looks for the leader. A retry refused with `NotLeader` was refused before
/// any proposal, so that attempt did nothing: sending it on, to the leader the refusal names or,
/// with none known yet, again a little later, is the driver's server selection waiting for a
/// primary (RefDb retries a write once, but only sends it to a primary, waiting up to
/// `serverSelectionTimeoutMS`), not another retry. Without it most retries land on a follower
/// while the range elects the leader that replaces the one that lost the answer, and say
/// nothing. Two seconds cover several elections and fit in the simulated run.
const RETRY_SELECTION: Nanos = Nanos::from_secs(2);

/// How often the workload asks the leader for a checksum (FR-014, on demand): the periodic one
/// comes every 10 minutes of cluster time, beyond a simulated run.
const CHECK_EVERY: Nanos = Nanos::from_secs(2);
/// How long a replica marked for rebuild is down while it is replaced (FR-014).
const REBUILD_DOWN: Nanos = Nanos::from_millis(50);

/// The key of a client's operation (FR-011): the client is the session, its operation counter the
/// transaction number, and the write its only statement.
fn dedup_key(client: u32, op_id: u64) -> DedupKey {
    DedupKey {
        lsid: u128::from(client),
        txn_number: i64::try_from(op_id).expect("a counter far below i64::MAX"),
        stmt_id: 0,
    }
}

/// Retries of a run (FR-011).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetryStats {
    /// Writes whose answer was lost and that went again.
    pub retried: u64,
    /// Attempts sent for them (one per retry, plus those that followed a `NotLeader`).
    pub sent: u64,
    /// Retries still waiting for their turn when the run ended: a write lost near the end has
    /// none sent.
    pub unsent: u64,
    /// Retried writes that got a definite answer (applied, or failed on their expected value),
    /// which closed their history op.
    pub definite: u64,
    /// Of those, the ones answered from the record of their applied original.
    pub from_record: u64,
    /// Retried writes without an expected value (an increment, a replacement or a deletion)
    /// acknowledged with `w: 1`.
    pub w1_blind: u64,
    /// Of those, the ones whose history op names the value the answer found as its predecessor.
    pub w1_blind_named: u64,
}

/// A write in flight, kept until it ends so that it can go again with the same key.
struct Write {
    change: Change,
    /// Its write concern, the same on a retry.
    wc: WriteConcern,
    /// It already went again: one more lost answer leaves it uncertain.
    retried: bool,
    /// Until when its retry looks for the leader (set when it is lost).
    select_until: Nanos,
}

/// A write whose answer was lost, waiting for its retry.
struct Retry {
    /// Its history op.
    op: usize,
    /// Not before this instant.
    due: Nanos,
    /// The node that lost the answer, or refused the retry.
    from: u32,
    /// The leader a refusal named, if any.
    to: Option<u32>,
}

fn doc(k: u64) -> Vec<u8> {
    format!("\x01d{k}").into_bytes()
}

fn key_name(k: u64) -> String {
    format!("k{k}")
}

fn group_name() -> String {
    format!("r{}", G.0)
}

/// Removes directory `dir` with the files in it, durably.
fn remove_tree(disk: &mut SimDisk, dir: &str) {
    let dir = Path::new(dir);
    for path in disk.list(dir).unwrap_or_default() {
        disk.execute(IoOp::Remove { path });
    }
    disk.execute(IoOp::SyncDir { path: dir.to_path_buf() });
    disk.execute(IoOp::RemoveDir { path: dir.to_path_buf() });
    disk.execute(IoOp::SyncDir { path: dir.parent().map(Path::to_path_buf).unwrap_or_default() });
}

/// A change of one document, which holds `tag|count`: the tag names the write that produced
/// it (unique, known to the client before it sends), the count is what increments add to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    /// The tag goes from `expected` (`None`: absent) to `new`; another tag fails.
    Cas { expected: Option<String>, new: String },
    /// A new tag and the count plus one (from 0 when absent).
    Inc { new: String },
    /// A new tag and the count back to 0, whatever it held.
    Replace { new: String },
    /// The document goes, if it is there.
    Delete,
}

/// The tag and count a document holds.
fn parse(held: &[u8]) -> (String, u64) {
    let s = String::from_utf8_lossy(held);
    let (tag, count) = s.split_once('|').unwrap_or((&s, "0"));
    (tag.to_owned(), count.parse().unwrap_or(0))
}

/// A change of the document at `key`; its result is the tag it found (empty: absent).
struct DocChange {
    key: Vec<u8>,
    change: Change,
}

impl Mutation for DocChange {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let held = current[0].map(|(_, v)| parse(v));
        let held_tag = held.as_ref().map(|(t, _)| t.clone());
        let count = held.as_ref().map_or(0, |(_, c)| *c);
        let put = |tag: &str, count: u64| Put {
            key: self.key.clone(),
            value_type: 1,
            value: format!("{tag}|{count}").into_bytes(),
        };
        let (puts, deletes) = match &self.change {
            Change::Cas { expected, .. } if *expected != held_tag => {
                return Err(MutationError(held_tag.unwrap_or_default()));
            }
            Change::Cas { new, .. } => (vec![put(new, count)], vec![]),
            Change::Inc { new } => (vec![put(new, count + 1)], vec![]),
            Change::Replace { new } => (vec![put(new, 0)], vec![]),
            Change::Delete if held.is_some() => (vec![], vec![self.key.clone()]),
            Change::Delete => (vec![], vec![]),
        };
        let found = held_tag.unwrap_or_default().into_bytes();
        Ok(Outcome { puts, deletes, change: found.clone(), result: found })
    }
}

/// A snapshot on its way to a replica that fell behind the log.
enum Transfer {
    /// A replica that has the data (`leader`) exports the range's two intervals into `dir`.
    Exporting { leader: u32, exports: Vec<u64>, files: Vec<FileRef>, dir: String },
    /// The replica ingests them.
    Ingesting { id: u64, files: Vec<FileRef> },
    /// The replica held old data of the range: it empties it first (`Lsm::clear_range`).
    Clearing { id: u64, files: Vec<FileRef> },
}

/// The `range-docs` workload.
pub struct RangeDocsWorkload {
    nodes: u32,
    core: u16,
    history: History,
    /// Request in flight `(node, request)` → its record.
    pending: BTreeMap<(u32, ReqId), usize>,
    hosts: BTreeMap<u32, RangeHost>,
    prefer: BTreeMap<u32, u32>,
    seen: BTreeMap<(u32, u64), Option<String>>,
    started: Vec<u64>,
    heal_at: Option<Nanos>,
    /// Snapshots `(index, transfer)` on their way to each replica.
    transfers: BTreeMap<u32, (u64, Transfer)>,
    /// Snapshots waiting for a replica to export them.
    waiting: BTreeMap<u32, u64>,
    /// Transfers started: each one writes its files in its own directory.
    attempt: u64,
    /// Document of each client's last answered read, until its next write.
    last_read: BTreeMap<u32, u64>,
    /// Mark and tag of each client's last read of each document.
    marks: BTreeMap<(u32, u64), (Hlc, Option<String>, u32)>,
    /// Reads answered, by document: when the answer came, at what mark and the tag it saw.
    served: BTreeMap<u64, Vec<(Nanos, Hlc, Option<String>)>>,
    /// Mark of each version whose write was acknowledged, by tag.
    versions: BTreeMap<String, Hlc>,
    /// Writes acknowledged by the majority, by document: when the answer came, their mark and the
    /// tag they wrote (none for a delete).
    committed: BTreeMap<u64, Vec<(Nanos, Hlc, Option<String>)>>,
    /// When the write of each tag began (its first attempt).
    begun: BTreeMap<String, Nanos>,
    /// Reads again at one of those marks, in flight: `(client, document, mark, tag read then,
    /// node that answered then)`.
    rereads: BTreeMap<(u32, ReqId), (u32, u64, Hlc, Option<String>, u32)>,
    /// The first read at a mark that did not see what an earlier one saw there.
    mvcc: Option<Violation>,
    /// Writes in flight, by history op.
    writes: BTreeMap<usize, Write>,
    /// Retries waiting, by client.
    retries: BTreeMap<u32, Retry>,
    /// Retries sent and answered, shared with whoever asked for them.
    stats: Rc<Cell<RetryStats>>,
    /// When the workload next asks for a checksum.
    next_check: Nanos,
    /// Checksums of the replicas on their way to the range's leader `(from, report)`.
    reports: Vec<(u32, Report)>,
    /// Replicas replaced because their checksum differed from the majority's.
    rebuilt: u64,
    /// Writes acknowledged with [`W1`]: `(client, op) → (node that acknowledged, when)`.
    w1: BTreeMap<(u32, u64), (u32, Nanos)>,
    /// Exports whose replica crashed, until their source writes them: `(source, exports, dir)`.
    abandoned: Vec<(u32, Vec<u64>, String)>,
    /// Exports whose source crashed while writing them, removed when it is back.
    orphans: Vec<(u32, String)>,
    /// The workload's name: `range-docs`, or `follower-reads` with reads on followers.
    name: &'static str,
    /// Reads on followers without a mark in flight: their client and document.
    follower_reads: Option<BTreeMap<(u32, ReqId), (u32, u64)>>,
    /// Reads on followers answered with a version or its absence.
    follower_served: u64,
    /// Documents read through the leader at the edge of the clock margin cut off at that time.
    edge_docs: (Nanos, Vec<u64>),
}

/// A client's turn while a leader at the edge of the clock margin is cut off.
#[derive(Clone, Copy)]
enum EdgeTurn {
    /// A read through the cut-off leader, during its lease.
    Read(u32),
    /// A write of a document read through it, through the leader that follows it.
    Write(u32, u64),
}

impl RangeDocsWorkload {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Self::counting(nodes, cores, Rc::default())
    }

    /// `follower-reads`: the workload with reads on followers (007 FR-012).
    pub fn follower_reads(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(RangeDocsWorkload {
            name: "follower-reads",
            follower_reads: Some(BTreeMap::new()),
            ..Self::new(nodes, cores, Rc::default())
        })
    }

    /// The workload, adding its retries to `stats`.
    pub fn counting(nodes: u32, cores: u16, stats: Rc<Cell<RetryStats>>) -> Box<dyn Workload> {
        Box::new(Self::new(nodes, cores, stats))
    }

    fn new(nodes: u32, cores: u16, stats: Rc<Cell<RetryStats>>) -> RangeDocsWorkload {
        RangeDocsWorkload {
            nodes,
            core: (G.0 % u64::from(cores)) as u16,
            history: History::default(),
            pending: BTreeMap::new(),
            hosts: BTreeMap::new(),
            prefer: BTreeMap::new(),
            seen: BTreeMap::new(),
            started: vec![0; CLIENTS as usize],
            heal_at: None,
            transfers: BTreeMap::new(),
            waiting: BTreeMap::new(),
            attempt: 0,
            marks: BTreeMap::new(),
            last_read: BTreeMap::new(),
            served: BTreeMap::new(),
            versions: BTreeMap::new(),
            committed: BTreeMap::new(),
            begun: BTreeMap::new(),
            rereads: BTreeMap::new(),
            mvcc: None,
            writes: BTreeMap::new(),
            retries: BTreeMap::new(),
            stats,
            next_check: CHECK_EVERY,
            reports: Vec::new(),
            rebuilt: 0,
            w1: BTreeMap::new(),
            abandoned: Vec::new(),
            orphans: Vec::new(),
            name: "range-docs",
            follower_reads: None,
            follower_served: 0,
            edge_docs: (Nanos::ZERO, Vec::new()),
        }
    }

    fn next_turn(w: &mut World, client: u32) {
        let edge = w.edge_cut_at.is_some_and(|(_, at)| w.now() < at + EDGE_WRITE_WINDOW);
        let (lo, hi) = if edge { EDGE_TURN_MS } else { (30, 120) };
        let at = w.now() + Nanos::from_millis(w.rng.range(lo, hi));
        w.schedule(at, Event::Client { client });
    }

    /// The turn of a client while a leader at the edge of the clock margin is cut off: a read
    /// through it until its lease ends, then a write of a document read there through the leader
    /// that follows it, within [`EDGE_WRITE_WINDOW`] of the cut.
    fn edge_turn(&mut self, w: &mut World) -> Option<EdgeTurn> {
        let (edge, at) = w.edge_cut_at?;
        if self.edge_docs.0 != at {
            self.edge_docs = (at, Vec::new());
        }
        let now = w.now();
        let lease = teseladb_raft::RaftConfig::new(NodeId(1)).lease_dur;
        if now < at + lease {
            return Some(EdgeTurn::Read(edge));
        }
        if now >= at + EDGE_WRITE_WINDOW || self.edge_docs.1.is_empty() {
            return None;
        }
        let next = (0..self.nodes).find(|&n| {
            n != edge
                && w.nodes[n as usize].alive
                && w.cluster_core(n, self.core)
                    .and_then(|c| c.status(G))
                    .is_some_and(|s| s.role == teseladb_raft::Role::Leader)
        })?;
        let k = self.edge_docs.1[w.rng.range(0, self.edge_docs.1.len() as u64) as usize];
        Some(EdgeTurn::Write(next, k))
    }

    fn busy(&self, client: u32) -> bool {
        self.pending.values().any(|&i| self.history.ops[i].client == client)
            || self.rereads.values().any(|r| r.0 == client)
            || self.follower_reads.iter().flatten().any(|(_, (c, _))| *c == client)
            || self.retries.contains_key(&client)
    }

    fn open_host(&mut self, w: &World, node: u32) {
        let n = &w.nodes[node as usize];
        let lsm =
            n.cores[self.core as usize].lsm.clone().expect("a core with a range opens its LSM");
        let mut host = RangeHost::new(lsm, n.hlc.clone());
        host.add_range(G, START.to_vec(), END.to_vec());
        self.hosts.insert(node, host);
    }

    fn times(w: &World, node: u32) -> (Nanos, u64) {
        (w.local(node), w.nodes[node as usize].clock.wall_ms(w.now()))
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let in_window = w.edge_cut_at.is_some_and(|(_, at)| w.now() < at + EDGE_WRITE_WINDOW);
        let edge = self.edge_turn(w).filter(|_| w.rng.chance(EDGE_SHARE));
        if in_window && edge.is_none() {
            return;
        }
        // Mostly the node it believes leads; sometimes any node, as clients that reach several
        // nodes do (a partition then has clients on both sides).
        let wander = w.rng.chance(0.2);
        let node = match (edge, self.prefer.get(&client)) {
            (Some(EdgeTurn::Read(n) | EdgeTurn::Write(n, _)), _) => n,
            (None, Some(&n)) if w.nodes[n as usize].alive && !wander => n,
            _ => w.rng.range(0, u64::from(self.nodes)) as u32,
        };
        // A frozen core takes no request: the client waits for its next turn.
        if !w.nodes[node as usize].alive || w.frozen(node, self.core) {
            return;
        }
        let k = w.rng.range(0, DOCS);
        if let Some(turn) = edge {
            return self.act_on(w, client, node, Some(turn), k);
        }
        if self.follower_reads.is_some() && w.rng.chance(FOLLOWER_SHARE) {
            return self.read_on_follower(w, client, node, k);
        }
        // Sometimes a document again at the mark of the client's last read of it: the same
        // version has to come back (FR-007, variant `write_below_read`).
        if w.rng.chance(0.1)
            && let Some((ts, tag, first)) = self.marks.get(&(client, k)).cloned()
        {
            let (now, wall) = Self::times(w, node);
            let host = self.hosts.get_mut(&node).expect("a live node has its host");
            let c = w.cluster_core(node, self.core).expect("alive");
            let req = host.get_at(c, G, &doc(k), ts, now, wall);
            self.rereads.insert((node, req), (client, k, ts, tag, first));
            self.pump(w, node);
            w.settle(node, self.core);
            return;
        }
        self.act_on(w, client, node, None, k);
    }

    /// A read or a write of document `k` (or, in an [`EdgeTurn`], the one it names) through
    /// `node`.
    fn act_on(&mut self, w: &mut World, client: u32, node: u32, edge: Option<EdgeTurn>, k: u64) {
        // The draws first: the core borrows the world below. Through a cut-off leader only its
        // lease reads answer.
        let read = match edge {
            Some(EdgeTurn::Read(_)) => {
                Some(if w.rng.chance(0.5) { ReadConcern::Majority } else { ReadConcern::Local })
            }
            Some(EdgeTurn::Write(..)) => None,
            None => w.rng.chance(0.4).then(|| match w.rng.range(0, 3) {
                0 => ReadConcern::Majority,
                1 => ReadConcern::Linearizable,
                _ => ReadConcern::Local,
            }),
        };
        // A write often updates the document its client just read, as an application reads a
        // document and then changes it.
        let k = match (edge, self.last_read.remove(&client)) {
            (Some(EdgeTurn::Write(_, j)), _) => j,
            (None, Some(j)) if read.is_none() && w.rng.chance(READ_THEN_WRITE) => j,
            _ => k,
        };
        if matches!(edge, Some(EdgeTurn::Read(_))) && !self.edge_docs.1.contains(&k) {
            self.edge_docs.1.push(k);
        }
        self.started[client as usize] += 1;
        let id = self.started[client as usize];
        let rec = OpRecord {
            client,
            op_id: id,
            kind: OpKind::Read,
            group: group_name(),
            key: key_name(k),
            expected: None,
            new: None,
            invoke: w.now(),
            complete: None,
            result: OpResult::Unknown,
            node,
            error: None,
            ended: None,
            w1: false,
        };
        // A compare-and-swap starts mostly from what its client last saw, sometimes from absent.
        let new = format!("c{client}-{id}");
        self.begun.entry(new.clone()).or_insert(w.now());
        let change = match w.rng.range(0, 10) {
            0..=4 => {
                let seen = self.seen.get(&(client, k)).cloned().flatten();
                let expected = if w.rng.chance(0.2) { None } else { seen };
                Change::Cas { expected, new: new.clone() }
            }
            5..=6 => Change::Inc { new: new.clone() },
            7..=8 => Change::Replace { new: new.clone() },
            _ => Change::Delete,
        };
        let wc = if w.rng.chance(W1_SHARE) { W1 } else { MAJORITY };
        let (now, wall) = Self::times(w, node);
        let core = self.core;
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let c = w.cluster_core(node, core).expect("alive");
        let (req, rec) = match read {
            Some(concern) => (host.get(c, G, &doc(k), concern, now, wall), rec),
            None => {
                // Until its answer says on what value it acted, only a cas has a known
                // predecessor.
                let rec = match &change {
                    Change::Cas { expected, new } => OpRecord {
                        kind: OpKind::Cas,
                        expected: expected.clone(),
                        new: Some(new.clone()),
                        ..rec
                    },
                    Change::Inc { new } | Change::Replace { new } => {
                        OpRecord { kind: OpKind::Write, new: Some(new.clone()), ..rec }
                    }
                    Change::Delete => OpRecord { kind: OpKind::Write, new: None, ..rec },
                };
                let write = Self::write_req(k, change.clone(), client, id, wc);
                self.writes.insert(
                    self.history.ops.len(),
                    Write { change, wc, retried: false, select_until: Nanos::ZERO },
                );
                (host.mutate(c, write, now, wall), rec)
            }
        };
        let rec = OpRecord { w1: rec.kind != OpKind::Read && wc == W1, ..rec };
        self.history.ops.push(rec);
        self.pending.insert((node, req), self.history.ops.len() - 1);
        self.pump(w, node);
        w.settle(node, core);
    }

    /// The request of a write of document `k`, with the key of the client's operation.
    fn write_req(k: u64, change: Change, client: u32, op_id: u64, wc: WriteConcern) -> WriteReq {
        WriteReq {
            g: G,
            keys: vec![doc(k)],
            m: Box::new(DocChange { key: doc(k), change }),
            dedup: Some(dedup_key(client, op_id)),
            wc,
        }
    }

    /// Op `i` got no answer from `from` (`error`): a write goes again once, later; anything
    /// else, or a write whose retry got none either, stays uncertain and is given up now.
    fn lost(&mut self, w: &mut World, i: usize, from: u32, error: String) {
        let now = w.now();
        let rec = &mut self.history.ops[i];
        match self.writes.get_mut(&i) {
            Some(write) if !write.retried => {
                write.retried = true;
                write.select_until = now + RETRY_SELECTION;
                let mut stats = self.stats.get();
                stats.retried += 1;
                self.stats.set(stats);
                let due = now + Nanos::from_millis(w.rng.range(RETRY_DELAY_MS.0, RETRY_DELAY_MS.1));
                self.retries.insert(rec.client, Retry { op: i, due, from, to: None });
            }
            _ => {
                self.writes.remove(&i);
                rec.error = Some(error);
                rec.ended = Some(now);
            }
        }
    }

    /// Sends the waiting write of `client` again, with the same key: to the leader a refusal
    /// named if it is alive, else to a live node other than `from` (the one it believes leads if
    /// it can); with no other node alive, to `from`; with none, at a later turn.
    fn retry(&mut self, w: &mut World, client: u32) {
        let Retry { op: i, from, to, .. } = self.retries.remove(&client).expect("a waiting retry");
        // A frozen core takes no request either: the write goes to another node or waits.
        let alive: Vec<u32> = (0..self.nodes)
            .filter(|&n| w.nodes[n as usize].alive && !w.frozen(n, self.core))
            .collect();
        let others: Vec<u32> = alive.iter().copied().filter(|&n| n != from).collect();
        let node = match (to, self.prefer.get(&client)) {
            (Some(n), _) if alive.contains(&n) => n,
            (_, Some(n)) if others.contains(n) => *n,
            _ if !others.is_empty() => others[w.rng.range(0, others.len() as u64) as usize],
            _ if alive.contains(&from) => from,
            _ => {
                self.retries.insert(client, Retry { op: i, due: w.now(), from, to });
                return;
            }
        };
        let rec = &self.history.ops[i];
        let k: u64 = rec.key[1..].parse().expect("a key of ours");
        let (change, wc) = (self.writes[&i].change.clone(), self.writes[&i].wc);
        let write = Self::write_req(k, change, client, rec.op_id, wc);
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let c = w.cluster_core(node, self.core).expect("alive");
        let req = host.mutate(c, write, now, wall);
        self.pending.insert((node, req), i);
        let mut stats = self.stats.get();
        stats.sent += 1;
        self.stats.set(stats);
        self.pump(w, node);
        w.settle(node, self.core);
    }

    /// Moves the host of `node` and records its answers.
    fn pump(&mut self, w: &mut World, node: u32) {
        let (now, wall) = Self::times(w, node);
        let core = self.core;
        let Some(host) = self.hosts.get_mut(&node) else { return };
        let Some(c) = w.cluster_core(node, core) else { return };
        host.poll(c, now, wall);
        // The embedder follows the state machines after each poll: GC thresholds to the LSM and
        // this replica's checksums (FR-013, FR-014).
        host.follow_state(c);
        self.reports.extend(host.take_checksum_reports().into_iter().map(|r| (node, r)));
        for (req, r) in host.take_replies() {
            self.answer(w, node, req, r);
        }
    }

    /// The node whose core leads the range, if any is alive.
    fn leader(&self, w: &mut World) -> Option<u32> {
        (0..self.nodes).find(|&n| {
            w.nodes[n as usize].alive
                && w.cluster_core(n, self.core)
                    .and_then(|c| c.status(G))
                    .is_some_and(|s| s.role == teseladb_raft::Role::Leader)
        })
    }

    /// Asks the leader for a checksum every [`CHECK_EVERY`] (FR-014, on demand).
    fn check(&mut self, w: &mut World) {
        if w.now() < self.next_check {
            return;
        }
        self.next_check = w.now() + CHECK_EVERY;
        let Some(l) = self.leader(w) else { return };
        let (Some(host), Some(c)) = (self.hosts.get_mut(&l), w.cluster_core(l, self.core)) else {
            return;
        };
        // A host that does not know it leads yet refuses; the next round asks again.
        if host.check(c, G).is_ok() {
            self.pump(w, l);
            w.settle(l, self.core);
        }
    }

    /// Hands the replicas' checksums to the range's leader (lost if it cannot be reached, as a
    /// frame would be) and replaces every replica its tally marks (FR-014).
    fn route_checksums(&mut self, w: &mut World) {
        let reports = std::mem::take(&mut self.reports);
        let leader = self.leader(w);
        for (from, r) in reports {
            if let Some(l) = leader.filter(|&l| l == from || w.net.reachable(from, l))
                && let Some(host) = self.hosts.get_mut(&l)
            {
                host.on_checksum_report(&r);
            }
        }
        let marked: Vec<u32> = self
            .hosts
            .values_mut()
            .flat_map(|h| h.take_rebuilds())
            .filter(|r| r.range == G)
            .map(|r| (r.node.0 - 1) as u32)
            .collect();
        for node in marked {
            self.rebuild(w, node);
        }
    }

    /// Replaces the replica on `node` (FR-014): it goes down, its range's data goes with it, and
    /// after its restart it gets the range again as a snapshot from a healthy replica (FR-012),
    /// as after any restart that finds the range's data missing. Its log and vote stay.
    fn rebuild(&mut self, w: &mut World, node: u32) {
        if !w.nodes[node as usize].alive {
            return;
        }
        self.rebuilt += 1;
        self.on_crash(w, node);
        w.crash_node(node);
        let dir = w.range_lsm.as_ref().expect("a range world has its LSM").dir.clone();
        let disk = &mut w.nodes[node as usize].cores[self.core as usize].disk;
        for path in disk.list(&dir).unwrap_or_default() {
            disk.execute(IoOp::Remove { path });
        }
        disk.execute(IoOp::SyncDir { path: dir });
        let at = w.now() + REBUILD_DOWN;
        w.schedule(at, Event::Restart { node });
    }

    /// A read of document `k` on `node` without its leader, for `client`: at the mark of the
    /// client's last read of it (a read again there, which has to see the same version), or at the
    /// highest closed mark the replica can read at.
    fn read_on_follower(&mut self, w: &mut World, client: u32, node: u32, k: u64) {
        let again =
            self.marks.get(&(client, k)).cloned().filter(|_| w.rng.chance(FOLLOWER_AT_MARK));
        let (now, wall) = Self::times(w, node);
        let host = self.hosts.get_mut(&node).expect("a live node has its host");
        let c = w.cluster_core(node, self.core).expect("alive");
        let spec = ReadSpec {
            g: G,
            key: doc(k),
            scan: None,
            at: again.as_ref().map(|(ts, ..)| *ts),
            concern: ReadConcern::Local,
            view: TxnView::default(),
            uncertain_to: None,
        };
        let req = host.read_follower(c, spec, None, now, wall);
        match again {
            Some((ts, tag, first)) => {
                self.rereads.insert((node, req), (client, k, ts, tag, first));
            }
            None => {
                let reads = self.follower_reads.as_mut().expect("follower-reads");
                reads.insert((node, req), (client, k));
            }
        }
        self.pump(w, node);
        w.settle(node, self.core);
    }

    fn answer(&mut self, w: &mut World, node: u32, req: ReqId, r: Result<Reply, RangeError>) {
        let now = w.now();
        let follower = self.follower_reads.as_mut().and_then(|f| f.remove(&(node, req)));
        if let Some((_, k)) = follower {
            // A refusal (no closed mark covers it yet) says nothing.
            if let Ok(Reply::Value { ts, value }) = r {
                let tag = value.map(|(_, v)| parse(&v).0);
                if let Some(t) = &tag {
                    self.seen_below_reads(k, t, ts);
                }
                self.served.entry(k).or_default().push((now, ts, tag));
                self.follower_served += 1;
            }
            return;
        }
        if let Some((client, k, ts, tag, first)) = self.rereads.remove(&(node, req)) {
            // A refusal or a lost answer says nothing about the version at that mark.
            if let Ok(Reply::Value { value, .. }) = r {
                let again = value.map(|(_, v)| parse(&v).0);
                if again != tag {
                    let detail = format!(
                        "{} {}: at {ts:?} client {client} read {tag:?} from n{first} and then \
                         {again:?} from n{node}",
                        group_name(),
                        key_name(k)
                    );
                    self.mvcc.get_or_insert(Violation::safety("mvcc", detail));
                }
            }
            return;
        }
        let i = self.pending.remove(&(node, req)).expect("a request of ours");
        // A refusal of a retry says nothing of the original, which may have been applied; one
        // that names another leader sends it there.
        let retried = self.writes.get(&i).is_some_and(|wr| wr.retried);
        if let Err(RangeError::NotLeader { hint }) = &r
            && let Some(write) = self.writes.get_mut(&i).filter(|wr| wr.retried)
            && now < write.select_until
        {
            let to = (hint.0 != 0).then(|| hint.0 as u32 - 1);
            let client = self.history.ops[i].client;
            if let Some(n) = to {
                self.prefer.insert(client, n);
            }
            let due = now + Nanos::from_millis(w.rng.range(RETRY_DELAY_MS.0, RETRY_DELAY_MS.1));
            self.retries.insert(client, Retry { op: i, due, from: node, to });
            return;
        }
        if let Err(e) = &r
            && (matches!(e, RangeError::Timeout | RangeError::Unavailable)
                || (retried && !matches!(e, RangeError::Mutation(_))))
        {
            if let RangeError::NotLeader { hint } = e
                && hint.0 != 0
            {
                self.prefer.insert(self.history.ops[i].client, hint.0 as u32 - 1);
            }
            self.lost(w, i, node, format!("{e:?}"));
            return;
        }
        let wc = self.writes.get(&i).map(|wr| wr.wc);
        if self.writes.remove(&i).is_some_and(|wr| wr.retried) {
            let mut stats = self.stats.get();
            stats.definite += 1;
            // A fresh evaluation describes the change with the tag it found, as its result; the
            // record of an applied original gives the result alone.
            if let Ok(Reply::Mutated { change, result, .. }) = &r
                && change != result
            {
                stats.from_record += 1;
            }
            self.stats.set(stats);
        }
        let (invoke, k): (Nanos, u64) =
            (self.history.ops[i].invoke, self.history.ops[i].key[1..].parse().expect("ours"));
        match &r {
            Ok(Reply::Mutated { ts, result, .. }) => {
                // A delete that found nothing wrote no version.
                let new = self.history.ops[i].new.clone();
                if let Some(tag) = &new {
                    self.versions.insert(tag.clone(), *ts);
                }
                if wc != Some(W1) {
                    self.above_reads(k, invoke, *ts);
                    self.above_writes(k, invoke, *ts);
                    if new.is_some() || !result.is_empty() {
                        self.committed.entry(k).or_default().push((now, *ts, new));
                    }
                }
            }
            Ok(Reply::Value { ts, value }) => {
                let tag = value.as_ref().map(|(_, v)| parse(v).0);
                if let Some(t) = &tag {
                    self.seen_below_reads(k, t, *ts);
                }
                self.read_above_writes(k, invoke, *ts);
                self.served.entry(k).or_default().push((now, *ts, tag));
            }
            _ => {}
        }
        let rec = &mut self.history.ops[i];
        let client = rec.client;
        let mut seen = None;
        match r {
            Ok(Reply::Mutated { result: change, .. }) => {
                let blind = rec.kind == OpKind::Write;
                // It acted on the tag it found: a cas from it.
                let held = String::from_utf8(change).expect("our tags");
                rec.kind = OpKind::Cas;
                rec.expected = (!held.is_empty()).then_some(held);
                rec.result = OpResult::Ok(None);
                seen = Some(rec.new.clone());
                if wc == Some(W1) {
                    // Acknowledged before the majority has it: it may be rolled back.
                    rec.result = OpResult::Unknown;
                    rec.error = Some(W1_ACKNOWLEDGED.into());
                    rec.ended = Some(now);
                    if retried && blind {
                        // A stale leader may have answered this attempt from a state its
                        // rollback takes back, while the original was applied by the majority
                        // over another value: the value found is not known to precede it.
                        rec.kind = OpKind::Write;
                        rec.expected = None;
                        let mut stats = self.stats.get();
                        stats.w1_blind += 1;
                        stats.w1_blind_named += u64::from(rec.kind == OpKind::Cas);
                        self.stats.set(stats);
                    }
                    self.w1.insert((client, rec.op_id), (node, now));
                    if let Some(s) = seen {
                        self.seen.insert((client, k), s);
                    }
                    return;
                }
            }
            Ok(Reply::Value { ts, value }) => {
                let v = value.map(|(_, v)| parse(&v).0);
                rec.result = OpResult::Ok(v.clone());
                self.marks.insert((client, k), (ts, v.clone(), node));
                self.last_read.insert(client, k);
                seen = Some(v);
            }
            Ok(Reply::Batch(_) | Reply::Changes { .. }) => {
                unreachable!("the workload does not scan nor read change streams")
            }
            Ok(Reply::Intents { .. } | Reply::Record(_) | Reply::Fenced { .. }) => {
                unreachable!("the workload runs no transactions")
            }
            Err(RangeError::Mutation(MutationError(held))) => {
                rec.result = OpResult::Fail;
                rec.error = Some(CAS_MISMATCH.into());
                seen = Some((!held.is_empty()).then_some(held));
            }
            // Refused before its proposal, or its entry was replaced: it never takes effect.
            Err(e) => {
                if let RangeError::NotLeader { hint } = e
                    && hint.0 != 0
                {
                    self.prefer.insert(client, hint.0 as u32 - 1);
                }
                rec.result = OpResult::Rejected;
                rec.error = Some(format!("{e:?}"));
            }
        }
        rec.complete = Some(now);
        if let Some(s) = seen {
            self.seen.insert((client, k), s);
        }
    }

    /// FR-004, FR-005: a write of document `k` acknowledged by the majority, whose operation began
    /// at `invoke`, has its mark `ts` above every read of `k` answered before then; otherwise a
    /// read at that mark would see the write appear after it was answered. A write acknowledged
    /// with `w: 1` is left out: it may be rolled back and never be seen.
    fn above_reads(&mut self, k: u64, invoke: Nanos, ts: Hlc) {
        let before = self.served.get(&k).into_iter().flatten().filter(|(at, ..)| *at < invoke);
        if let Some((at, read, _)) = before.max_by_key(|(_, m, _)| *m)
            && ts <= *read
        {
            let detail = format!(
                "r1 {}: a write begun at {} ms got mark {ts:?}, at or below the mark {read:?} of a \
                 read answered at {} ms",
                key_name(k),
                invoke.as_millis(),
                at.as_millis()
            );
            self.mvcc.get_or_insert(Violation::safety("mvcc", detail));
        }
    }

    /// FR-004: a write of document `k` acknowledged by the majority, whose operation began at
    /// `invoke`, has its mark `ts` above every write of `k` the majority acknowledged before then;
    /// otherwise the older version would hide it from every read above both marks. A node whose
    /// clock fell behind after a restart writes that way.
    fn above_writes(&mut self, k: u64, invoke: Nanos, ts: Hlc) {
        let before = self.committed.get(&k).into_iter().flatten().filter(|(at, ..)| *at < invoke);
        if let Some((at, older, tag)) = before.max_by_key(|(_, m, _)| *m)
            && ts <= *older
        {
            let detail = format!(
                "r1 {}: a write begun at {} ms got mark {ts:?}, at or below the mark {older:?} of \
                 {tag:?}, acknowledged at {} ms",
                key_name(k),
                invoke.as_millis(),
                at.as_millis()
            );
            self.mvcc.get_or_insert(Violation::safety("mvcc", detail));
        }
    }

    /// FR-004, FR-009: a read of document `k` begun at `invoke` has its mark `ts` at or above
    /// every write of `k` the majority acknowledged before then; a read below one of them cannot
    /// see it and answers a version already replaced.
    fn read_above_writes(&mut self, k: u64, invoke: Nanos, ts: Hlc) {
        let before = self.committed.get(&k).into_iter().flatten().filter(|(at, ..)| *at < invoke);
        if let Some((at, newer, tag)) = before.max_by_key(|(_, m, _)| *m)
            && ts < *newer
        {
            let detail = format!(
                "r1 {}: a read begun at {} ms got mark {ts:?}, below the mark {newer:?} of {tag:?}, \
                 acknowledged at {} ms",
                key_name(k),
                invoke.as_millis(),
                at.as_millis()
            );
            self.mvcc.get_or_insert(Violation::safety("mvcc", detail));
        }
    }

    /// FR-004, FR-005, whatever became of the write's answer: a read at `ts` sees the version of
    /// tag `tag` of document `k`, so that version's mark is at or below `ts`. A read of `k`
    /// answered at a mark at or above `ts` before that write began could not have seen it: the
    /// version appeared below a mark a read was already answered at. This catches a write whose
    /// answer was lost (a timeout, a node that died), which [`RangeDocsWorkload::above_reads`] cannot see.
    fn seen_below_reads(&mut self, k: u64, tag: &str, ts: Hlc) {
        let Some(&begun) = self.begun.get(tag) else { return };
        let before = self.served.get(&k).into_iter().flatten();
        if let Some((at, read, _)) =
            before.filter(|(at, m, _)| *at < begun && *m >= ts).min_by_key(|(at, ..)| *at)
        {
            let detail = format!(
                "r1 {}: a read at {ts:?} sees {tag}, written by a write begun at {} ms, below the \
                 mark {read:?} of a read answered at {} ms",
                key_name(k),
                begun.as_millis(),
                at.as_millis()
            );
            self.mvcc.get_or_insert(Violation::safety("mvcc", detail));
        }
    }

    /// FR-004, FR-005, over the whole run: a read at mark `m` of document `k` that saw a version
    /// older than a write acknowledged by the majority at a mark at or below `m` missed it, which
    /// only happens when the write took its mark below that read after the read was answered. It
    /// catches it also for a retried write, whose operation began before the read and so escapes
    /// [`RangeDocsWorkload::above_reads`] and [`RangeDocsWorkload::seen_below_reads`].
    fn missed_versions(&self) -> Result<(), Violation> {
        for (k, reads) in &self.served {
            let writes = self.committed.get(k).map_or(&[][..], Vec::as_slice);
            for (at, m, seen) in reads {
                let Some(seen_at) = seen.as_ref().and_then(|t| self.versions.get(t)) else {
                    continue;
                };
                if let Some((_, w, tag)) = writes.iter().find(|(_, w, _)| seen_at < w && w <= m) {
                    let detail = format!(
                        "r1 {}: a read at {m:?} answered at {} ms saw {seen:?}, of mark \
                         {seen_at:?}, and not {tag:?}, acknowledged by the majority at {w:?}",
                        key_name(*k),
                        at.as_millis()
                    );
                    return Err(Violation::safety("mvcc", detail));
                }
            }
        }
        Ok(())
    }

    /// A live replica other than `except` whose range applied `index`: its LSM holds the range up
    /// to there, and what it holds beyond applies again as the same versions. It need not lead:
    /// a replica waiting for data does not vote, so waiting for a leader could wait forever.
    fn source(&self, w: &mut World, except: u32, index: u64) -> Option<u32> {
        (0..self.nodes).find(|&n| {
            n != except
                && w.nodes[n as usize].alive
                && !self.transfers.contains_key(&n)
                && w.cluster_core(n, self.core)
                    .and_then(|c| c.state_machine(G).map(|sm| sm.applied_index()))
                    .is_some_and(|applied| applied >= index)
        })
    }

    /// Moves the snapshots on their way: exports on a replica that has the data, ingestion on the
    /// one waiting, and its core told once the data is there (FR-012).
    fn transfer(&mut self, w: &mut World) {
        for (node, core, g, index) in std::mem::take(&mut w.snapshot_waits) {
            if g == G && core == self.core && !self.transfers.contains_key(&node) {
                self.waiting.insert(node, index);
            }
        }
        for (node, index) in std::mem::take(&mut self.waiting) {
            let Some(leader) = self.source(w, node, index) else {
                self.waiting.insert(node, index);
                continue;
            };
            let lsm = w.nodes[leader as usize].cores[self.core as usize].lsm.clone().expect("open");
            let (local_start, local_end) = local_interval(G);
            // A directory per attempt: an earlier export to the same replica may still be there.
            self.attempt += 1;
            let dir = format!("export-n{node}-{index}-{}", self.attempt);
            let local =
                lsm.borrow_mut().export(&local_start, &local_end, NEWEST, dir.clone().into());
            let data = lsm.borrow_mut().export(START, END, NEWEST, dir.clone().into());
            let exports = vec![local, data];
            let t = Transfer::Exporting { leader, exports, files: vec![], dir };
            self.transfers.insert(node, (index, t));
            w.pump(leader, self.core);
        }
        let nodes: Vec<u32> = self.transfers.keys().copied().collect();
        for node in nodes {
            self.transfer_step(w, node);
        }
        self.drop_abandoned(w);
    }

    /// Removes the exports whose replica crashed once their source finished writing them.
    fn drop_abandoned(&mut self, w: &mut World) {
        let core = self.core as usize;
        for (leader, mut exports, dir) in std::mem::take(&mut self.abandoned) {
            let c = &mut w.nodes[leader as usize].cores[core];
            c.lsm_done.exported.retain(|(id, _)| match exports.iter().position(|e| e == id) {
                Some(i) => {
                    exports.remove(i);
                    false
                }
                None => true,
            });
            if exports.is_empty() {
                remove_tree(&mut c.disk, &dir);
            } else {
                self.abandoned.push((leader, exports, dir));
            }
        }
    }

    fn transfer_step(&mut self, w: &mut World, node: u32) {
        let core = self.core as usize;
        let (index, t) = self.transfers.remove(&node).expect("a transfer");
        let next = match t {
            Transfer::Exporting { leader, mut exports, mut files, dir } => {
                let done = &mut w.nodes[leader as usize].cores[core].lsm_done.exported;
                for (id, f) in std::mem::take(done) {
                    if let Some(i) = exports.iter().position(|&e| e == id) {
                        exports.remove(i);
                        files.extend(f);
                    } else {
                        done.push((id, f));
                    }
                }
                if !exports.is_empty() {
                    Some(Transfer::Exporting { leader, exports, files, dir })
                } else {
                    // Over the wire in chunks, written on the replica and ingested.
                    let disk = &mut w.nodes[leader as usize].cores[core].disk;
                    let bytes: Vec<Vec<u8>> = files
                        .iter()
                        .map(|f| disk.read_all(&f.path).expect("an exported file"))
                        .collect();
                    // Read: the export goes from the source.
                    remove_tree(disk, &dir);
                    let mut asm = Assembler::new(G, index);
                    let mut whole = None;
                    for c in split(G, index, &bytes) {
                        whole = asm.add(c).expect("in order");
                    }
                    let c = &mut w.nodes[node as usize].cores[core];
                    self.attempt += 1;
                    let dir = format!("incoming-{index}-{}", self.attempt);
                    let (ops, refs) =
                        write_ops(&whole.expect("whole"), Path::new(&dir), FileId(INCOMING_FILES));
                    for op in ops {
                        c.disk.execute(op);
                    }
                    let lsm = c.lsm.clone().expect("open");
                    let intervals = vec![local_interval(G), (START.to_vec(), END.to_vec())];
                    let id = lsm.borrow_mut().ingest_range(refs.clone(), intervals, (G, index));
                    w.pump(node, self.core);
                    Some(Transfer::Ingesting { id, files: refs })
                }
            }
            // A frozen core takes the ingested data when it thaws.
            Transfer::Ingesting { id, files } if w.frozen(node, self.core) => {
                Some(Transfer::Ingesting { id, files })
            }
            Transfer::Ingesting { id, files } => {
                let done = &mut w.nodes[node as usize].cores[core].lsm_done.ingested;
                let mine = done.iter().position(|(i, _)| *i == id).map(|i| done.remove(i).1);
                match mine {
                    None => Some(Transfer::Ingesting { id, files }),
                    Some(Ok(())) => {
                        // A transfer ends with its replica's crash: the core is there.
                        let now = w.local(node);
                        let c = w.cluster_core(node, self.core).expect("powered on, not frozen");
                        c.snapshot_data_ready(G, now);
                        w.settle(node, self.core);
                        None
                    }
                    // Old data of the range is still here: it goes first.
                    Some(Err(LsmError::IngestOverlap)) => {
                        let lsm = w.nodes[node as usize].cores[core].lsm.clone().expect("open");
                        let intervals = vec![local_interval(G), (START.to_vec(), END.to_vec())];
                        let id = lsm.borrow_mut().clear_range(G, intervals);
                        w.pump(node, self.core);
                        Some(Transfer::Clearing { id, files })
                    }
                    // A full disk refused it (a stopped LSM restarts the node): it starts over.
                    Some(Err(_)) => {
                        self.waiting.insert(node, index);
                        None
                    }
                }
            }
            Transfer::Clearing { id, files } => {
                let done = &mut w.nodes[node as usize].cores[core].lsm_done.cleared;
                match done.iter().position(|i| *i == id) {
                    None => Some(Transfer::Clearing { id, files }),
                    Some(i) => {
                        done.remove(i);
                        let lsm = w.nodes[node as usize].cores[core].lsm.clone().expect("open");
                        let intervals = vec![local_interval(G), (START.to_vec(), END.to_vec())];
                        let id =
                            lsm.borrow_mut().ingest_range(files.clone(), intervals, (G, index));
                        w.pump(node, self.core);
                        Some(Transfer::Ingesting { id, files })
                    }
                }
            }
        };
        if let Some(t) = next {
            self.transfers.insert(node, (index, t));
        }
    }
}

impl Workload for RangeDocsWorkload {
    fn name(&self) -> &'static str {
        self.name
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
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
        w.add_group(GroupDesc { group: G, voters, learners: vec![] }, SmKind::Range(START, END));
        for node in 0..self.nodes {
            if w.nodes[node as usize].alive {
                self.open_host(w, node);
            }
        }
        for client in 0..CLIENTS {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client == 0 {
            self.check(w);
        }
        if self.retries.get(&client).is_some_and(|r| r.due <= w.now()) {
            self.retry(w, client);
        } else if !self.busy(client) {
            self.act(w, client);
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        for (node, core, op, r) in std::mem::take(&mut w.events) {
            self.on_done(w, node, core, op, r);
        }
        for node in 0..self.nodes {
            if w.nodes[node as usize].alive {
                self.pump(w, node);
            }
        }
        self.route_checksums(w);
        self.transfer(w);
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.on_completions(w, node, 0, Completions::default());
        // What was in flight on it gets no answer: a write goes again, anything else stays
        // unknown, given up now.
        let lost: Vec<usize> =
            self.pending.iter().filter(|((n, _), _)| *n == node).map(|(_, &i)| i).collect();
        self.pending.retain(|(n, _), _| *n != node);
        for i in lost {
            self.lost(w, i, node, "NodeCrashed".into());
        }
        self.rereads.retain(|(n, _), _| *n != node);
        if let Some(f) = self.follower_reads.as_mut() {
            f.retain(|(n, _), _| *n != node);
        }
        self.hosts.remove(&node);
        // An export for it that its source is still writing goes once written.
        if let Some((_, Transfer::Exporting { leader, exports, dir, .. })) =
            self.transfers.remove(&node)
        {
            self.abandoned.push((leader, exports, dir));
        }
        self.waiting.remove(&node);
        // The exports the crashed node was writing stop with it: they go when it is back.
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.abandoned).into_iter().partition(|(l, ..)| *l == node);
        self.abandoned = kept;
        self.orphans.extend(gone.into_iter().map(|(l, _, dir)| (l, dir)));
        // A snapshot the crashed node was exporting is asked for again.
        let lost: Vec<(u32, u64, String)> = self
            .transfers
            .iter()
            .filter_map(|(n, (index, t))| match t {
                Transfer::Exporting { leader, dir, .. } if *leader == node => {
                    Some((*n, *index, dir.clone()))
                }
                _ => None,
            })
            .collect();
        for (n, index, dir) in lost {
            self.transfers.remove(&n);
            self.orphans.push((node, dir));
            self.waiting.insert(n, index);
        }
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        let (mine, others): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.orphans).into_iter().partition(|(n, _)| *n == node);
        self.orphans = others;
        for (_, dir) in mine {
            remove_tree(&mut w.nodes[node as usize].cores[self.core as usize].disk, &dir);
        }
        for core in 0..w.nodes[node as usize].cores.len() as u16 {
            w.settle(node, core);
        }
        self.open_host(w, node);
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        self.heal_at = Some(w.now());
    }

    fn history(&self) -> Option<&History> {
        Some(&self.history)
    }

    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        let mut stats = self.stats.get();
        stats.unsent = self.retries.len() as u64;
        self.stats.set(stats);
        if let Some(v) = self.mvcc.take() {
            return Err(v);
        }
        self.missed_versions()?;
        liveness::check(&self.history, self.heal_at, liveness::WINDOW)?;
        // The leaders close marks: a run where no follower ever read means they do not.
        if self.follower_reads.is_some() && self.follower_served == 0 {
            return Err(Violation::liveness("liveness", "no read on a follower was served"));
        }
        // A write acknowledged by the majority and lost comes before a `w: 1` one missing from
        // the rollback files: the second check judges losses as the first one does, and a lost
        // update (two increments that did not wait for each other) loses both kinds.
        durability::check(&self.history)?;
        self.check_rollback(w)?;
        linearizable::check(&self.history)
    }
}

impl RangeDocsWorkload {
    /// Every write acknowledged with [`W1`] that is lost, as the durability checker judges it, is
    /// in a rollback file of some replica (FR-010, D-034): the replica whose log a later leader
    /// overwrote wrote it there. A write acknowledged by a node that is out for good (its disk
    /// damaged) is not judged: that node never rolls back.
    fn check_rollback(&self, w: &mut World) -> Result<(), Violation> {
        let acknowledged = |o: &OpRecord| {
            self.w1
                .get(&(o.client, o.op_id))
                .filter(|(node, _)| !w.nodes[*node as usize].lost)
                .map(|(_, at)| *at)
        };
        let lost = durability::lost(&self.history, acknowledged)?;
        if lost.is_empty() {
            return Ok(());
        }
        let dir = Path::new(ROLLBACK_DIR).join(format!("range-{}", G.0));
        let mut rolled_back: Vec<(Vec<u8>, String)> = Vec::new();
        for n in &mut w.nodes {
            let disk = &mut n.cores[self.core as usize].disk;
            for path in disk.list(&dir).unwrap_or_default() {
                let bytes = disk.read_all(&path).expect("a listed file");
                let docs = read_rollback_file(&bytes).map_err(|e| {
                    let detail = format!("{}: unreadable at {}", path.display(), e.offset);
                    Violation::safety("rollback", detail)
                })?;
                rolled_back.extend(docs.into_iter().filter_map(|d| match d {
                    RollbackDoc::Version { key, value, .. } => Some((key, parse(&value).0)),
                    RollbackDoc::Raw { .. } => None,
                }));
            }
        }
        for (op, read) in lost {
            let k: u64 = op.key[1..].parse().expect("a key of ours");
            let tag = op.new.clone().unwrap_or_default();
            if !rolled_back.contains(&(doc(k), tag.clone())) {
                let detail = format!(
                    "{} {}: write {}/{} of {tag} was acknowledged with w: 1, is not in the value \
                     read by {}/{} and is in no rollback file",
                    group_name(),
                    op.key,
                    op.client,
                    op.op_id,
                    read.client,
                    read.op_id
                );
                return Err(Violation::safety("rollback", detail));
            }
        }
        Ok(())
    }

    fn on_done(
        &mut self,
        w: &mut World,
        node: u32,
        core: u16,
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
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn workload() -> RangeDocsWorkload {
        RangeDocsWorkload::new(3, 2, Rc::new(Cell::new(RetryStats::default())))
    }

    /// A `range-docs` world of three nodes of two cores after `secs` seconds without faults.
    fn world(secs: u64) -> (World, RangeDocsWorkload) {
        let mut wl = workload();
        let mut w = World::with_clocks(9, 3, 2, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    fn mark(physical: u64) -> Hlc {
        Hlc { physical, logical: 0 }
    }

    fn at(ms: u64) -> Nanos {
        Nanos::from_millis(ms)
    }

    #[test]
    fn a_write_at_or_below_an_earlier_read_or_write_breaks_mvcc() {
        let mut wl = workload();
        wl.served.insert(1, vec![(at(10), mark(50), None)]);
        wl.above_reads(1, at(20), mark(60));
        assert!(wl.mvcc.is_none(), "above the read");
        wl.above_reads(1, at(20), mark(50));
        assert!(wl.mvcc.take().is_some());
        wl.committed.insert(1, vec![(at(10), mark(50), Some("a".into()))]);
        wl.above_writes(1, at(20), mark(51));
        assert!(wl.mvcc.is_none());
        wl.above_writes(1, at(20), mark(50));
        assert!(wl.mvcc.take().is_some());
        wl.read_above_writes(1, at(20), mark(50));
        assert!(wl.mvcc.is_none(), "a read at the write's mark sees it");
        wl.read_above_writes(1, at(20), mark(49));
        assert!(wl.mvcc.take().is_some());
    }

    #[test]
    fn a_version_seen_below_a_read_answered_before_its_write_breaks_mvcc() {
        let mut wl = workload();
        wl.served.insert(1, vec![(at(10), mark(50), None)]);
        // A tag whose write is not known: nothing to judge.
        wl.seen_below_reads(1, "x", mark(40));
        assert!(wl.mvcc.is_none());
        wl.begun.insert("x".into(), at(20));
        wl.seen_below_reads(1, "x", mark(60));
        assert!(wl.mvcc.is_none(), "above the read");
        wl.seen_below_reads(1, "x", mark(40));
        assert!(wl.mvcc.take().is_some());
    }

    #[test]
    fn a_read_that_missed_a_version_below_its_mark_fails_the_run() {
        let mut wl = workload();
        wl.versions.insert("old".into(), mark(10));
        wl.served.insert(1, vec![(at(5), mark(50), Some("old".into())), (at(6), mark(9), None)]);
        wl.committed.insert(1, vec![(at(4), mark(60), Some("new".into()))]);
        assert!(wl.missed_versions().is_ok(), "the newer one is above the read");
        wl.committed.get_mut(&1).expect("set").push((at(4), mark(30), Some("mid".into())));
        assert!(wl.missed_versions().is_err());
        let mut w = World::with_clocks(1, 3, 2, false, true, wl.clocks());
        wl.mvcc = Some(Violation::safety("mvcc", "test"));
        assert!(wl.finish(&mut w).is_err());
    }

    #[test]
    fn a_read_again_at_a_mark_that_sees_another_version_breaks_mvcc() {
        let (mut w, mut wl) = world(1);
        let req: ReqId = u64::MAX;
        wl.rereads.insert((0, req), (0, 1, mark(5), Some("a".into()), 1));
        let value = Some((1, b"b|1".to_vec()));
        wl.answer(&mut w, 0, req, Ok(Reply::Value { ts: mark(5), value }));
        assert!(wl.mvcc.take().is_some());
    }

    #[test]
    fn a_replica_marked_for_rebuild_goes_down_without_its_data_and_comes_back() {
        let (mut w, mut wl) = world(1);
        w.nodes[2].alive = false;
        wl.rebuild(&mut w, 2);
        assert_eq!(wl.rebuilt, 0, "a node that is down is left");
        w.nodes[2].alive = true;
        wl.rebuild(&mut w, 1);
        assert_eq!(wl.rebuilt, 1);
        assert!(!w.nodes[1].alive && !wl.hosts.contains_key(&1));
        let back = |w: &mut World| w.nodes[1].alive;
        let until = w.now() + Nanos::from_secs(1);
        assert!(crate::drive_until(&mut w, &mut wl, until, back));
    }

    #[test]
    fn checks_and_answers_a_node_cannot_take_are_dropped() {
        let (mut w, mut wl) = world(1);
        // No leader: no check.
        let alive: Vec<bool> = w.nodes.iter().map(|n| n.alive).collect();
        for n in &mut w.nodes {
            n.alive = false;
        }
        wl.next_check = Nanos::ZERO;
        wl.check(&mut w);
        for (n, a) in w.nodes.iter_mut().zip(alive) {
            n.alive = a;
        }
        // A leader without host: no check either.
        let l = wl.leader(&mut w).expect("a leader");
        let host = wl.hosts.remove(&l).expect("a host");
        wl.next_check = Nanos::ZERO;
        wl.check(&mut w);
        wl.hosts.insert(l, host);
        // Answers of another core, of a node without host, of a core that is gone.
        let core = wl.core;
        let r = || Err(OpError::Timeout);
        wl.on_done(&mut w, 0, 1 - core, OpId(1), r());
        let host = wl.hosts.remove(&0).expect("a host");
        wl.on_done(&mut w, 0, core, OpId(1), r());
        wl.hosts.insert(0, host);
        let cluster = w.nodes[0].cores[core as usize].cluster.take();
        wl.on_done(&mut w, 0, core, OpId(1), r());
        w.nodes[0].cores[core as usize].cluster = cluster;
    }

    #[test]
    fn a_crashed_source_s_exports_are_asked_for_again_and_removed_once_it_is_back() {
        let (mut w, mut wl) = world(1);
        let exporting = |leader| Transfer::Exporting {
            leader,
            exports: vec![u64::MAX],
            files: vec![],
            dir: format!("export-test-{leader}"),
        };
        wl.transfers.insert(1, (5, exporting(0)));
        wl.transfers.insert(2, (5, Transfer::Ingesting { id: u64::MAX, files: vec![] }));
        wl.on_crash(&mut w, 0);
        w.crash_node(0);
        assert_eq!(wl.waiting.get(&1), Some(&5));
        assert!(matches!(wl.transfers.get(&2), Some((5, Transfer::Ingesting { .. }))));
        assert_eq!(wl.orphans, vec![(0, "export-test-0".to_owned())]);
    }

    #[test]
    fn a_crash_drops_the_follower_reads_and_exports_of_the_node_and_its_restart_their_files() {
        let (mut w, mut wl) = world(1);
        // A follower read in flight keeps its client busy until the crash drops it.
        wl.follower_reads = Some(BTreeMap::from([((2, 7), (0, 1)), ((1, 7), (1, 1))]));
        assert!(wl.busy(0));
        // An export for node 2 its source 0 still writes, and an abandoned one of each source.
        let exporting = Transfer::Exporting {
            leader: 0,
            exports: vec![u64::MAX],
            files: vec![],
            dir: "export-test-2".into(),
        };
        wl.transfers.insert(2, (5, exporting));
        // Node 0 waits for an export of node 1, which goes on until node 1 crashes.
        let from_1 = Transfer::Exporting {
            leader: 1,
            exports: vec![u64::MAX],
            files: vec![],
            dir: "export-test-0".into(),
        };
        wl.transfers.insert(0, (6, from_1));
        wl.abandoned.push((1, vec![u64::MAX], "export-test-1".into()));
        wl.on_crash(&mut w, 2);
        w.crash_node(2);
        assert!(!wl.busy(0), "its read is gone");
        assert!(wl.busy(1), "a read on another node stays");
        assert_eq!(wl.abandoned.len(), 2, "the export for it goes once written");
        wl.on_crash(&mut w, 1);
        w.crash_node(1);
        let orphans = vec![(1, "export-test-1".to_owned()), (1, "export-test-0".to_owned())];
        assert_eq!(wl.orphans, orphans);
        assert_eq!(wl.waiting.get(&0), Some(&6), "asked again");
        // Back, node 1 removes its orphans and leaves those of others.
        wl.orphans.push((0, "export-test-0".into()));
        w.start_node(1).expect("started");
        wl.on_restart(&mut w, 1).expect("restarted");
        assert_eq!(wl.orphans, vec![(0, "export-test-0".to_owned())]);
    }

    #[test]
    fn abandoned_exports_go_once_their_source_wrote_them_all() {
        let (mut w, mut wl) = world(1);
        let core = wl.core as usize;
        wl.abandoned.push((0, vec![1, 2], "export-test".into()));
        w.nodes[0].cores[core].lsm_done.exported = vec![(1, vec![]), (9, vec![])];
        wl.drop_abandoned(&mut w);
        assert_eq!(wl.abandoned, vec![(0, vec![2], "export-test".to_owned())]);
        assert_eq!(w.nodes[0].cores[core].lsm_done.exported.len(), 1, "another's stays");
        w.nodes[0].cores[core].lsm_done.exported.push((2, vec![]));
        wl.drop_abandoned(&mut w);
        assert!(wl.abandoned.is_empty());
    }

    #[test]
    fn a_transfer_waits_for_its_own_exports_a_thawed_core_and_a_source() {
        let (mut w, mut wl) = world(1);
        let core = wl.core;
        // Another export's result stays where it is.
        let exporting = Transfer::Exporting {
            leader: 0,
            exports: vec![u64::MAX],
            files: vec![],
            dir: "export-test".into(),
        };
        wl.transfers.insert(2, (5, exporting));
        w.nodes[0].cores[core as usize].lsm_done.exported.push((7, vec![]));
        wl.transfer_step(&mut w, 2);
        assert!(matches!(wl.transfers.get(&2), Some((5, Transfer::Exporting { .. }))));
        assert_eq!(w.nodes[0].cores[core as usize].lsm_done.exported.len(), 1);
        // A frozen core takes its ingestion when it thaws.
        wl.transfers.insert(2, (5, Transfer::Ingesting { id: 3, files: vec![] }));
        w.nodes[2].cores[core as usize].frozen_until = w.now() + Nanos::from_secs(1);
        wl.transfer_step(&mut w, 2);
        assert!(matches!(wl.transfers.get(&2), Some((5, Transfer::Ingesting { id: 3, .. }))));
        w.nodes[2].cores[core as usize].frozen_until = Nanos::ZERO;
        wl.transfers.remove(&2);
        // A replica that waits while every other one is busy or behind keeps waiting; waits of
        // another range, of another core or of a replica already served are left.
        wl.transfers.insert(1, (5, Transfer::Ingesting { id: 4, files: vec![] }));
        w.snapshot_waits = vec![
            (2, core, G, u64::MAX),
            (2, core, GroupId(G.0 + 1), 1),
            (2, 1 - core, G, 1),
            (1, core, G, 1),
        ];
        wl.transfer(&mut w);
        assert_eq!(wl.waiting.get(&2), Some(&u64::MAX));
        assert!(!wl.waiting.contains_key(&1));
        assert!(wl.source(&mut w, 2, 0).is_some_and(|n| n != 1), "a busy node is no source");
        wl.waiting.clear();
        wl.transfers.clear();
    }

    #[test]
    fn a_node_down_at_the_start_gets_no_host() {
        let mut wl = workload();
        let mut w = World::with_clocks(9, 3, 2, false, true, wl.clocks());
        w.nodes[1].alive = false;
        wl.start(&mut w);
        assert!(wl.hosts.contains_key(&0) && !wl.hosts.contains_key(&1));
    }

    #[test]
    fn a_follower_reads_run_where_no_follower_read_was_served_fails_liveness() {
        let (mut w, mut wl) = world(1);
        // A node without host has nothing to move.
        let host = wl.hosts.remove(&0).expect("a host");
        wl.pump(&mut w, 0);
        wl.hosts.insert(0, host);
        wl.follower_reads = Some(BTreeMap::new());
        let v = wl.finish(&mut w).expect_err("no read on a follower");
        assert!(v.liveness && v.detail.contains("follower"), "{}", v.detail);
    }
}
