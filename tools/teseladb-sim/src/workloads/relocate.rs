//! `relocate` workload (007 T020, FR-011, FR-023): one range of documents on every node moves
//! from core to core of the nodes while clients read and write its documents and the fault plan
//! crashes nodes, partitions the network, freezes cores, slows and fills disks and moves clocks.
//! The workload plays Group 0: it records an intent (target core), the range's leader proposes
//! `Relocate`, and once the leader runs on the target core the workload completes the change and
//! gives the descriptor the new core. On each node it hands the range over as the server does
//! (plan «Cambio de núcleo»):
//!
//! - a replica that applied `Relocate` to another core stops applying; its core exports the
//!   range's two intervals, reads the files, closes the group and takes its Raft state, its state
//!   and the entries past it;
//! - on two nodes of every three the replica goes on applying after `Relocate` while its core
//!   exports the range from the state it had then, and stops at its cut: a repeat of `Relocate`
//!   that the leader proposes once it has its files, or on its own if it does not lead. Its
//!   group goes with the state of the export and the entries applied since. The other nodes
//!   stop at `Relocate`, as a version without this does;
//! - the target core writes the files, ingests them (clearing first what its LSM kept of the
//!   range) and writes the group whole to its log; once that is durable the old core removes its
//!   group, and once that removal is durable the target core opens its copy;
//! - a replica that missed the change (its node was down) hears its leader through the core
//!   that runs the range on its node, which hands it the frames of the range (the world does as
//!   the server does), applies `Relocate` there and is handed over the same way;
//! - after a restart the old copy hands the range over again, and a copy imported and not started
//!   is removed when the new hand-over arrives.
//!
//! Each document is a register of unique tags; the history goes to the progress, durability and
//! linearizability checkers.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;

use teseladb_keys::Hlc;
use teseladb_lsm::{FileRef, Lsm, LsmError};
use teseladb_multiraft::{
    Done, Durable, ExportBase, GroupDesc, GroupImport, OpError, OpId, StateMachine,
};
use teseladb_raft::Role;
use teseladb_raftlog::{Completions, Disk, FileId, IoDone};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::Command;
use teseladb_range::host::{RangeHost, ReqId, WriteReq};
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::checkers::{Violation, durability, linearizable, liveness};
use crate::history::{History, OpKind, OpRecord, OpResult};
use crate::node::{NodeOutput, NodeSm};
use crate::workloads::Workload;
use crate::workloads::carry::{self, Carried};
use crate::workloads::data_ship::Shipper;
use crate::workloads::multirange::{SetTag, as_range, doc, key_name, record_answer, remove_tree};
use crate::world::{Event, World};

const G: GroupId = GroupId(1);
const START: &[u8] = b"\x01";
const END: &[u8] = b"\x02";
/// Documents, spread over the range.
const DOCS: u64 = 8;
const CLIENTS: u32 = 4;
/// Pause between two core changes, in ms.
const MOVE_EVERY: (u64, u64) = (300, 1500);
/// Pause between two sends of the same proposal.
const RESEND_EVERY: Nanos = Nanos::from_millis(500);
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
/// Handles of the files of a hand-over, apart from the LSM's and the snapshots'.
const HANDOVER_FILES: u32 = 1 << 21;

/// A hand-over of the range on one node, from core `from` to core `to`.
enum Move {
    /// The old core exports the range's intervals; `at` is the index its copy had applied then.
    Exporting { jobs: Vec<u64>, files: Vec<FileRef>, dir: String, at: u64 },
    /// The target core removes a copy imported before a restart and never started.
    Dropping { imp: GroupImport, files: (Vec<Vec<u8>>, Vec<Carried>) },
    /// The target core empties what its LSM kept of the range.
    Clearing { imp: GroupImport, refs: Vec<FileRef>, job: u64 },
    /// The target core ingests the files.
    Ingesting { imp: GroupImport, refs: Vec<FileRef>, job: u64 },
    /// The target core writes the group to its log.
    Importing,
    /// The old core removes its group.
    Retiring,
}

/// The `relocate` workload.
pub struct RelocateWorkload {
    nodes: u32,
    cores: u16,
    history: History,
    pending: BTreeMap<(u32, ReqId), usize>,
    /// Each node's host, on the core its replica runs on.
    hosts: BTreeMap<u32, (u16, RangeHost)>,
    prefer: BTreeMap<u32, u32>,
    started: Vec<u64>,
    heal_at: Option<Nanos>,
    /// Group 0's descriptor core and the change in progress (target core and intent).
    core: u16,
    intent: Option<(u16, u64)>,
    next_intent: u64,
    next_move: Nanos,
    resend_at: Nanos,
    /// The proposals of `Relocate`: their answers are nobody's.
    proposals: BTreeSet<(u32, u16, OpId)>,
    /// The hand-overs in progress, by node: the old core, the target core and the step.
    moves: BTreeMap<u32, (u16, u16, Move)>,
    /// The state each node's export started from when its replica went on applying meanwhile.
    ahead: BTreeMap<u32, ExportBase>,
    /// Nodes whose replica went on applying and could not be exported so: it stops first.
    cold: BTreeSet<u32>,
    /// What each core made durable and nobody took yet.
    durable: BTreeMap<(u32, u16), Vec<Durable>>,
    attempt: u64,
    shipper: Shipper,
    /// Core changes completed.
    pub done: u64,
    /// Hand-overs of a replica exported while it went on applying.
    pub ahead_done: u64,
    /// Replicas that went on applying and stopped on their own, because they did not lead.
    pub cut_alone: u64,
    /// Whole files the hand-overs linked (004 FR-033).
    pub linked: u64,
    /// The range was created on every node: a replica created later gets it as a snapshot.
    born: bool,
}

impl RelocateWorkload {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(RelocateWorkload::new(nodes, cores))
    }

    fn new(nodes: u32, cores: u16) -> RelocateWorkload {
        RelocateWorkload {
            nodes,
            cores,
            history: History::default(),
            pending: BTreeMap::new(),
            hosts: BTreeMap::new(),
            prefer: BTreeMap::new(),
            started: vec![0; CLIENTS as usize],
            heal_at: None,
            core: 0,
            intent: None,
            next_intent: 0,
            next_move: Nanos::from_millis(MOVE_EVERY.1),
            resend_at: Nanos::ZERO,
            proposals: BTreeSet::new(),
            moves: BTreeMap::new(),
            ahead: BTreeMap::new(),
            cold: BTreeSet::new(),
            durable: BTreeMap::new(),
            attempt: 0,
            shipper: Shipper::new(0),
            done: 0,
            ahead_done: 0,
            cut_alone: 0,
            linked: 0,
            born: false,
        }
    }

    fn times(w: &World, node: u32) -> (Nanos, u64) {
        (w.local(node), w.nodes[node as usize].clock.wall_ms(w.now()))
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(30, 120));
        w.schedule(at, Event::Client { client });
    }

    fn voters(&self) -> Vec<NodeId> {
        (1..=u64::from(self.nodes)).map(NodeId).collect()
    }

    /// Whether the replica of `node` goes on applying after `Relocate` until its cut (007
    /// FR-011): two nodes of every three, as during an upgrade.
    fn lingers(node: u32) -> bool {
        !node.is_multiple_of(3)
    }

    /// Whether the replica of `node`, on core `from`, applied a `Relocate` to another core and
    /// goes on applying.
    fn goes_on(sm: &RangeSm, from: u16) -> bool {
        sm.relocated().is_some_and(|(to, _)| to != from) && !sm.halted()
    }

    /// Whether the replica of `node`, which goes on applying, may stop: its files are exported,
    /// or it could not be exported while running.
    fn exported(&self, node: u32) -> bool {
        match self.moves.get(&node) {
            Some((_, _, Move::Exporting { jobs, .. })) => {
                jobs.is_empty() && self.ahead.contains_key(&node)
            }
            Some(_) => false,
            None => self.cold.contains(&node),
        }
    }

    /// The core whose group of the range runs on `node`.
    fn running(&self, w: &mut World, node: u32) -> Option<u16> {
        (0..self.cores).find(|c| w.paused_core(node, *c).is_some_and(|x| x.status(G).is_some()))
    }

    fn lsm(w: &World, node: u32, core: u16) -> Option<Rc<RefCell<Lsm>>> {
        w.nodes[node as usize].cores[core as usize].lsm.clone()
    }

    /// Opens the LSM of every core of `node`: each core may take the range.
    fn open_lsms(&self, w: &mut World, node: u32) {
        let cfg = w.range_lsm.clone().expect("set at the start");
        for c in &mut w.nodes[node as usize].cores {
            if c.lsm.is_none()
                && let Ok((lsm, _)) = Lsm::recover(&cfg, &mut c.disk)
            {
                c.lsm = Some(Rc::new(RefCell::new(lsm)));
            }
        }
    }

    /// Opens the range on `node` where the server would: on the only core that has it in its log,
    /// on the old core when both have it (it hands the range over again), and on the
    /// descriptor's core when none has it (the node never made its creation durable).
    fn open(&mut self, w: &mut World, node: u32) {
        if !w.nodes[node as usize].alive || self.moves.contains_key(&node) {
            return;
        }
        if let Some(c) = self.running(w, node) {
            self.shipper.set_core(node, c);
            if self.hosts.get(&node).is_none_or(|(hc, _)| *hc != c) {
                self.open_host(w, node, c);
            }
            return;
        }
        let holders: Vec<u16> = (0..self.cores)
            .filter(|c| w.paused_core(node, *c).is_some_and(|x| x.store().group(G).is_some()))
            .collect();
        let mut chosen = None;
        for c in &holders {
            let Some(lsm) = Self::lsm(w, node, *c) else { return };
            let Ok(Some(mut sm)) = RangeSm::try_open(G, START.to_vec(), END.to_vec(), lsm) else {
                return;
            };
            sm.set_core(*c);
            sm.set_lingers(Self::lingers(node));
            // A copy imported here waits while another core holds the range.
            let imported = sm.relocated().is_some_and(|(t, _)| t == *c);
            if holders.len() == 1 || !imported {
                chosen = Some((*c, sm));
                break;
            }
        }
        let (c, sm) = match (chosen, holders.is_empty()) {
            (Some(x), _) => x,
            (None, true) => {
                let c = self.core;
                let Some(lsm) = Self::lsm(w, node, c) else { return };
                let Ok(Some(mut sm)) = RangeSm::try_open(G, START.to_vec(), END.to_vec(), lsm)
                else {
                    return;
                };
                sm.set_core(c);
                sm.set_lingers(Self::lingers(node));
                if self.born {
                    sm.expect_snapshot();
                }
                (c, sm)
            }
            (None, false) => return,
        };
        let desc = GroupDesc { group: G, voters: self.voters(), learners: vec![] };
        let Some(x) = w.cluster_core(node, c) else { return };
        if x.create_group(desc, NodeSm::Range(sm)).is_err() {
            return;
        }
        w.settle(node, c);
        self.shipper.set_core(node, c);
        self.open_host(w, node, c);
    }

    /// A host for `node` over core `core`; the requests of the one it replaces are lost.
    fn open_host(&mut self, w: &mut World, node: u32, core: u16) {
        self.lose(w, node);
        let Some(lsm) = Self::lsm(w, node, core) else { return };
        let mut host = RangeHost::new(lsm, w.nodes[node as usize].hlc.clone());
        // Half the nodes write the counted format (007 FR-039), as during an upgrade.
        host.set_counted_writes(node.is_multiple_of(2));
        host.add_range(G, START.to_vec(), END.to_vec());
        self.hosts.insert(node, (core, host));
    }

    /// The requests in flight on `node` lose their answer.
    fn lose(&mut self, w: &World, node: u32) {
        let lost: Vec<usize> =
            self.pending.iter().filter(|((n, _), _)| *n == node).map(|(_, &i)| i).collect();
        self.pending.retain(|(n, _), _| *n != node);
        let now = w.now();
        for i in lost {
            let rec = &mut self.history.ops[i];
            rec.result = OpResult::Unknown;
            rec.error = Some("NodeCrashed".into());
            rec.ended = Some(now);
        }
        self.hosts.remove(&node);
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let node = match self.prefer.get(&client) {
            Some(&n) if w.nodes[n as usize].alive && !w.rng.chance(0.2) => n,
            _ => w.rng.range(0, u64::from(self.nodes)) as u32,
        };
        let Some((core, _)) = self.hosts.get(&node) else { return };
        let core = *core;
        if !w.nodes[node as usize].alive {
            return;
        }
        let k = w.rng.range(0, DOCS);
        self.started[client as usize] += 1;
        let id = self.started[client as usize];
        let read = w.rng.chance(0.4);
        let tag = format!("c{client}-{id}");
        let rec = OpRecord {
            client,
            op_id: id,
            kind: if read { OpKind::Read } else { OpKind::Write },
            group: "docs".into(),
            key: key_name(k),
            expected: None,
            new: (!read).then(|| tag.clone()),
            invoke: w.now(),
            complete: None,
            result: OpResult::Unknown,
            node,
            error: None,
            ended: None,
            w1: false,
        };
        let (now, wall) = Self::times(w, node);
        let (_, host) = self.hosts.get_mut(&node).expect("checked above");
        let Some(c) = w.cluster_core(node, core) else { return };
        let req = if read {
            host.get(c, G, &doc(k), ReadConcern::Linearizable, now, wall)
        } else {
            let m = Box::new(SetTag(doc(k), tag));
            host.mutate(
                c,
                WriteReq { g: G, keys: vec![doc(k)], m, dedup: None, wc: MAJORITY },
                now,
                wall,
            )
        };
        self.history.ops.push(rec);
        self.pending.insert((node, req), self.history.ops.len() - 1);
        self.pump(w, node);
        w.settle(node, core);
    }

    fn pump(&mut self, w: &mut World, node: u32) {
        let (now, wall) = Self::times(w, node);
        let Some((core, host)) = self.hosts.get_mut(&node) else { return };
        let Some(c) = w.cluster_core(node, *core) else { return };
        host.poll(c, now, wall);
        host.follow_state(c);
        let _ = host.take_checksum_reports();
        let _ = host.take_splits();
        for (req, r) in host.take_replies() {
            if let Some(i) = self.pending.remove(&(node, req)) {
                record_answer(&mut self.history.ops[i], &mut self.prefer, (node, w.now()), r);
            }
        }
    }

    fn on_done(
        &mut self,
        w: &mut World,
        node: u32,
        core: u16,
        op: OpId,
        r: Result<Done<NodeOutput>, OpError>,
    ) {
        if self.proposals.remove(&(node, core, op)) {
            return;
        }
        let Some((hc, host)) = self.hosts.get_mut(&node) else { return };
        if *hc != core {
            return;
        }
        let Some(c) = w.paused_core(node, core) else { return };
        host.on_done(c, op, r);
    }

    /// Group 0's side: starts a core change now and then, has the leader propose it and completes
    /// it once the leader runs on the target core.
    fn lead(&mut self, w: &mut World) {
        let now = w.now();
        if self.cores < 2 {
            return;
        }
        let Some((target, intent)) = self.intent else {
            if now >= self.next_move && self.moves.is_empty() {
                self.next_intent += 1;
                self.intent = Some(((self.core + 1) % self.cores, self.next_intent));
            }
            return;
        };
        let alive: Vec<u32> = (0..self.nodes).filter(|n| w.nodes[*n as usize].alive).collect();
        let leader = alive.into_iter().find_map(|n| {
            let c = self.running(w, n)?;
            let x = w.cluster_core(n, c)?;
            let leads = x.status(G).is_some_and(|s| s.role == Role::Leader);
            leads.then_some((n, c))
        });
        let Some((node, core)) = leader else { return };
        // A leader's core answered above: it is not frozen, and its group is the range.
        let x = w.cluster_core(node, core).expect("a leader's core");
        let sm = x.state_machine(G).and_then(as_range).expect("the range");
        let (relocated, halted) = (sm.relocated(), sm.halted());
        // A leader that has applied nothing of its own term yet may have the intent's `Relocate`
        // in its log from an earlier one: proposed again, it would be the cut.
        let st = x.status(G).expect("the range");
        let log = x.store().group(G);
        let settled = log.is_some_and(|l| l.term(st.applied) == Some(st.hard_state.term));
        if relocated == Some((target, intent)) {
            if core == target {
                self.core = target;
                self.intent = None;
                self.cold.clear();
                self.done += 1;
                self.next_move = now + Nanos::from_millis(w.rng.range(MOVE_EVERY.0, MOVE_EVERY.1));
                return;
            }
            // It goes on applying after its `Relocate`: its cut, a repeat of it, waits for its
            // files, and one that could not be exported while running stops first.
            if halted || !self.exported(node) || now < self.resend_at {
                return;
            }
        } else if halted || !settled || now < self.resend_at {
            return;
        }
        self.resend_at = now + RESEND_EVERY;
        let payload = Command::Relocate { core: target, intent }.encode();
        let local = w.local(node);
        let x = w.cluster_core(node, core).expect("a leader's core");
        let zero = Hlc { physical: 0, logical: 0 };
        if let Ok((op, _)) =
            x.propose_at(G, zero, local, |_| Ok::<_, std::convert::Infallible>(payload))
        {
            self.proposals.insert((node, core, op));
        }
        w.settle(node, core);
    }

    /// A leader that applied `Relocate` to another core applies nothing more: it hands its
    /// leadership to the next voter, as the server does, so that a replica on the target core
    /// leads and the old one closes.
    fn step_down(&mut self, w: &mut World, node: u32) {
        let Some(c) = self.running(w, node) else { return };
        let Some(x) = w.cluster_core(node, c) else { return };
        let leads = x.status(G).is_some_and(|s| s.role == Role::Leader);
        let halted = x.state_machine(G).and_then(as_range).is_some_and(RangeSm::halted);
        if leads && halted {
            let next = NodeId(u64::from((node + 1) % self.nodes) + 1);
            x.transfer_leader(G, next);
            w.settle(node, c);
        }
    }

    /// Takes what every core of `node` made durable.
    fn collect_durable(&mut self, w: &mut World, node: u32) {
        for c in 0..self.cores {
            if let Some(x) = w.cluster_core(node, c) {
                let d = x.take_durable();
                self.durable.entry((node, c)).or_default().extend(d);
            }
        }
    }

    fn took(&mut self, node: u32, core: u16, what: Durable) -> bool {
        let list = self.durable.entry((node, core)).or_default();
        match list.iter().position(|d| *d == what) {
            Some(i) => {
                list.remove(i);
                true
            }
            None => false,
        }
    }

    /// Starts a hand-over on `node` when its replica applied `Relocate` to another core, or runs
    /// on another core than the descriptor's with no change in progress.
    fn start_move(&mut self, w: &mut World, node: u32) {
        if self.moves.contains_key(&node) {
            return;
        }
        let Some(from) = self.running(w, node) else { return };
        let Some(x) = w.cluster_core(node, from) else { return };
        // `running` found the group there: it is the range.
        let sm = x.state_machine(G).and_then(as_range).expect("the range");
        let (intervals, applied) = (sm.intervals(), sm.applied_index());
        let goes_on = Self::goes_on(sm, from);
        let to = match sm.relocated() {
            Some((t, _)) if sm.halted() || goes_on => t,
            _ => return,
        };
        // A replica that goes on applying is exported from the state of its last applied entry,
        // up to that entry's mark: what it applies meanwhile goes with its log. One that cannot
        // be exported so stops first.
        let (upto, at) = match goes_on {
            false => (NEWEST, applied),
            true if self.cold.contains(&node) => return,
            true => match x.capture_export(G) {
                Ok(base) => {
                    let from_here = (base.mark, base.index);
                    self.ahead.insert(node, base);
                    from_here
                }
                Err(_) => {
                    self.cold.insert(node);
                    return;
                }
            },
        };
        let Some(lsm) = Self::lsm(w, node, from) else { return };
        self.attempt += 1;
        let dir = format!("handover-n{node}-{}", self.attempt);
        // Every other attempt hands whole files over as links, as between the cores of a node on
        // one file system (004 FR-033); the rest exports every file, as to another node.
        let link = carry::links(self.attempt);
        let jobs = intervals
            .iter()
            .map(|(s, e)| match link {
                true => lsm.borrow_mut().export_linked(s, e, upto, dir.clone().into()),
                false => lsm.borrow_mut().export(s, e, upto, dir.clone().into()),
            })
            .collect();
        self.moves.insert(node, (from, to, Move::Exporting { jobs, files: vec![], dir, at }));
        w.pump(node, from);
    }

    /// The cut of a replica that goes on applying and does not lead: it stops on its own once
    /// its files are exported. A leader's cut goes through the log.
    fn cut(&mut self, w: &mut World, node: u32) {
        if !self.exported(node) {
            return;
        }
        let Some(from) = self.running(w, node) else { return };
        let Some(x) = w.cluster_core(node, from) else { return };
        let leads = x.status(G).is_some_and(|s| s.role == Role::Leader);
        let sm = x.state_machine(G).and_then(as_range);
        if let Some(sm) = sm.filter(|sm| !leads && Self::goes_on(sm, from)) {
            sm.cut();
            self.cut_alone += 1;
        }
    }

    /// The target core could not take the hand-over: the old core opens its group again and
    /// hands the range over anew.
    fn fail(&mut self, node: u32) {
        self.moves.remove(&node);
    }

    fn move_step(&mut self, w: &mut World, node: u32) {
        let Some((from, to, m)) = self.moves.remove(&node) else { return };
        // A frozen core moves on when it thaws.
        if w.frozen(node, from) || w.frozen(node, to) {
            self.moves.insert(node, (from, to, m));
            return;
        }
        let next = match m {
            Move::Exporting { mut jobs, mut files, dir, at } => {
                let done = &mut w.nodes[node as usize].cores[from as usize].lsm_done.exported;
                for (id, f) in std::mem::take(done) {
                    match jobs.iter().position(|j| *j == id) {
                        Some(i) => {
                            jobs.remove(i);
                            files.extend(f);
                        }
                        None => done.push((id, f)),
                    }
                }
                if !jobs.is_empty() {
                    Some(Move::Exporting { jobs, files, dir, at })
                } else {
                    self.hand(w, node, (from, to), (files, dir, at))
                }
            }
            Move::Dropping { imp, files } => {
                if self.took(node, to, Durable::Removed(G)) {
                    let lsm = Self::lsm(w, node, to).expect("open");
                    let job = lsm.borrow_mut().clear_range(G, Self::intervals());
                    let refs = self.write(w, node, to, &files);
                    w.pump(node, to);
                    refs.map(|refs| Move::Clearing { imp, refs, job })
                } else {
                    Some(Move::Dropping { imp, files })
                }
            }
            Move::Clearing { imp, refs, job } => {
                let done = &mut w.nodes[node as usize].cores[to as usize].lsm_done.cleared;
                match done.iter().position(|i| *i == job) {
                    None => Some(Move::Clearing { imp, refs, job }),
                    Some(i) => {
                        done.remove(i);
                        Some(self.ingest(w, node, to, imp, refs))
                    }
                }
            }
            Move::Ingesting { imp, refs, job } => {
                let done = &mut w.nodes[node as usize].cores[to as usize].lsm_done.ingested;
                let mine = done.iter().position(|(i, _)| *i == job).map(|i| done.remove(i).1);
                match mine {
                    None => Some(Move::Ingesting { imp, refs, job }),
                    Some(Ok(())) => {
                        let c = w.cluster_core(node, to).expect("powered on and not frozen");
                        let r = c.import_group(G, imp);
                        w.settle(node, to);
                        r.ok().map(|()| Move::Importing)
                    }
                    Some(Err(LsmError::IngestOverlap)) => {
                        let lsm = Self::lsm(w, node, to).expect("open");
                        let job = lsm.borrow_mut().clear_range(G, Self::intervals());
                        w.pump(node, to);
                        Some(Move::Clearing { imp, refs, job })
                    }
                    Some(Err(_)) => None,
                }
            }
            Move::Importing => {
                if self.took(node, to, Durable::Imported(G)) {
                    let c = w.cluster_core(node, from).expect("powered on and not frozen");
                    let r = c.retire_group(G);
                    w.settle(node, from);
                    match r {
                        Ok(()) => Some(Move::Retiring),
                        Err(_) => None,
                    }
                } else {
                    Some(Move::Importing)
                }
            }
            Move::Retiring => {
                if self.took(node, from, Durable::Removed(G)) {
                    let lsm = Self::lsm(w, node, from).expect("open");
                    lsm.borrow_mut().clear_range(G, Self::intervals());
                    w.pump(node, from);
                    None
                } else {
                    Some(Move::Retiring)
                }
            }
        };
        match next {
            Some(m) => {
                self.moves.insert(node, (from, to, m));
            }
            None => self.fail(node),
        }
    }

    fn intervals() -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![teseladb_range::snapshot::local_interval(G), (START.to_vec(), END.to_vec())]
    }

    /// The old core's files are exported: it reads them, closes its group if it still runs, and
    /// hands everything to the target core.
    fn hand(
        &mut self,
        w: &mut World,
        node: u32,
        (from, to): (u16, u16),
        (files, dir, at): (Vec<FileRef>, String, u64),
    ) -> Option<Move> {
        let c = w.cluster_core(node, from).expect("powered on and not frozen");
        let sm = c.state_machine(G).and_then(as_range);
        let halted = sm.is_some_and(RangeSm::halted);
        let moves = sm.is_some_and(|sm| sm.relocated().is_some_and(|(t, _)| t == to));
        let whole = sm.is_some_and(|sm| sm.applied_index() == at);
        let imp = match self.ahead.get(&node).cloned() {
            // Exported while it ran: it goes once it stopped at its cut, with the entries it
            // applied since. A range whose target moved meanwhile, or whose log no longer has
            // those entries, is exported again as it is then.
            Some(_) if !moves => None,
            Some(_) if !halted => return Some(Move::Exporting { jobs: vec![], files, dir, at }),
            Some(base) => match c.export_group_from(G, base) {
                Ok(imp) => {
                    self.ahead_done += 1;
                    Some(imp)
                }
                // Something of the group is in flight with the disk: it goes on the next turn.
                Err(OpError::Busy) => {
                    return Some(Move::Exporting { jobs: vec![], files, dir, at });
                }
                Err(_) => None,
            },
            // Restored from its snapshot, its state does not move it: it runs on there. A
            // halted copy that installed a snapshot while its export ran holds a newer state
            // than its files: it is exported again, as the server does (H-046 of the native
            // tests).
            None if !(halted && whole) => None,
            None => match c.export_group(G) {
                Ok(imp) => Some(imp),
                Err(_) => return Some(Move::Exporting { jobs: vec![], files, dir, at }),
            },
        };
        if self.ahead.remove(&node).is_some() {
            c.release_export(G);
        }
        let Some(imp) = imp else {
            remove_tree(&mut w.nodes[node as usize].cores[from as usize].disk, &dir);
            return None;
        };
        if let Some((_, host)) = self.hosts.get_mut(&node) {
            host.remove_range(G);
        }
        w.settle(node, from);
        let disk = &mut w.nodes[node as usize].cores[from as usize].disk;
        let carried = carry::read(disk, &files);
        self.linked += carried.1.len() as u64;
        remove_tree(disk, &dir);
        let target = w.cluster_core(node, to).expect("powered on and not frozen");
        if target.store().group(G).is_some() {
            // A copy imported before a restart and never started: it goes first.
            let r = target.retire_group(G);
            w.settle(node, to);
            return r.ok().map(|()| Move::Dropping { imp, files: carried });
        }
        let refs = self.write(w, node, to, &carried)?;
        let lsm = Self::lsm(w, node, to).expect("open");
        if lsm.borrow().flushed_index(G) > 0 {
            let job = lsm.borrow_mut().clear_range(G, Self::intervals());
            w.pump(node, to);
            return Some(Move::Clearing { imp, refs, job });
        }
        Some(self.ingest(w, node, to, imp, refs))
    }

    /// Writes the files of a hand-over on core `to` of `node`; `None` if a write failed.
    fn write(
        &mut self,
        w: &mut World,
        node: u32,
        to: u16,
        (written, linked): &(Vec<Vec<u8>>, Vec<Carried>),
    ) -> Option<Vec<FileRef>> {
        self.attempt += 1;
        let dir = format!("incoming-handover-{}", self.attempt);
        let c = &mut w.nodes[node as usize].cores[to as usize];
        let (ops, refs) = carry::write(written, linked, Path::new(&dir), FileId(HANDOVER_FILES));
        let done: Vec<IoDone> = ops.into_iter().map(|op| c.disk.execute(op)).collect();
        if done.iter().any(|d| matches!(d, IoDone::Err { .. })) {
            remove_tree(&mut c.disk, &dir);
            return None;
        }
        Some(refs)
    }

    fn ingest(
        &mut self,
        w: &mut World,
        node: u32,
        to: u16,
        imp: GroupImport,
        refs: Vec<FileRef>,
    ) -> Move {
        let lsm = Self::lsm(w, node, to).expect("open");
        let job = lsm.borrow_mut().ingest_range(refs.clone(), Self::intervals(), (G, imp.index));
        w.pump(node, to);
        Move::Ingesting { imp, refs, job }
    }

    /// Everything that moves after a core's completions.
    fn step(&mut self, w: &mut World) {
        self.lead(w);
        for node in 0..self.nodes {
            if !w.nodes[node as usize].alive {
                continue;
            }
            self.collect_durable(w, node);
            self.step_down(w, node);
            self.move_step(w, node);
            self.start_move(w, node);
            self.cut(w, node);
            self.open(w, node);
            self.pump(w, node);
        }
        self.shipper.step(w);
    }
}

impl Workload for RelocateWorkload {
    fn name(&self) -> &'static str {
        "relocate"
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
        // Every node is up at the start.
        for node in 0..self.nodes {
            self.open_lsms(w, node);
            self.open(w, node);
        }
        self.born = true;
        for client in 0..CLIENTS {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if self.pending.values().all(|&i| self.history.ops[i].client != client) {
            self.act(w, client);
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        for (node, core, op, r) in std::mem::take(&mut w.events) {
            self.on_done(w, node, core, op, r);
        }
        self.step(w);
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.on_completions(w, node, 0, Completions::default());
        self.lose(w, node);
        self.moves.remove(&node);
        self.ahead.remove(&node);
        self.cold.remove(&node);
        self.durable.retain(|(n, _), _| *n != node);
        self.proposals.retain(|(n, _, _)| *n != node);
        self.shipper.on_crash(node);
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.open_lsms(w, node);
        for core in 0..self.cores {
            w.settle(node, core);
        }
        self.open(w, node);
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        self.heal_at = Some(w.now());
    }

    fn history(&self) -> Option<&History> {
        Some(&self.history)
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        liveness::check(&self.history, self.heal_at, liveness::WINDOW)?;
        durability::check(&self.history)?;
        linearizable::check(&self.history)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_multiraft::StateMachine;

    use super::*;

    /// A `relocate` world of three nodes of `cores` cores after `secs` seconds without faults.
    fn world(cores: u16, secs: u64) -> (World, RelocateWorkload) {
        let mut wl = RelocateWorkload::new(3, cores);
        let mut w = World::with_clocks(5, 3, cores, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    fn freeze(w: &mut World, node: u32, core: u16) {
        w.nodes[node as usize].cores[core as usize].frozen_until = w.now() + Nanos::from_secs(1);
    }

    /// Without faults every core change completes, the replicas that go on applying after
    /// `Relocate` are handed over with the export they took meanwhile, and those of them that
    /// do not lead stop on their own (007 FR-011).
    #[test]
    fn replicas_that_go_on_are_exported_while_they_run_and_followers_stop_alone() {
        let (mut w, mut wl) = world(2, 20);
        assert!(wl.done >= 4, "{} core changes", wl.done);
        assert!(wl.ahead_done >= wl.done, "{} of {}", wl.ahead_done, wl.done);
        assert!(wl.cut_alone > 0, "a replica that does not lead stops on its own");
        assert!(wl.linked > 0, "no hand-over linked a whole file (004 FR-033)");
        assert!(wl.finish(&mut w).is_ok());
    }

    #[test]
    fn lsms_open_once_and_one_that_cannot_recover_stays_shut() {
        let (mut w, wl) = world(2, 1);
        let open = w.nodes[0].cores[0].lsm.clone().expect("open");
        w.nodes[0].cores[1].lsm = None;
        // Its files damaged: it does not recover.
        let disk = &mut w.nodes[0].cores[1].disk;
        for f in disk.list(Path::new("range-lsm")).expect("its folder") {
            disk.corrupt(&f, 0);
        }
        wl.open_lsms(&mut w, 0);
        let same = w.nodes[0].cores[0].lsm.as_ref().is_some_and(|l| Rc::ptr_eq(l, &open));
        assert!(same, "an open LSM is kept");
        assert!(w.nodes[0].cores[1].lsm.is_none(), "a full disk keeps it shut");
    }

    #[test]
    fn a_node_down_moving_or_already_open_is_left_as_it_is() {
        let (mut w, mut wl) = world(2, 1);
        let host_core = |wl: &RelocateWorkload, n| wl.hosts.get(&n).map(|(c, _)| *c);
        let before = host_core(&wl, 0);
        wl.open(&mut w, 0);
        assert_eq!(host_core(&wl, 0), before, "its host stays");
        w.nodes[1].alive = false;
        wl.hosts.remove(&1);
        wl.open(&mut w, 1);
        w.nodes[1].alive = true;
        assert_eq!(host_core(&wl, 1), None);
        wl.moves.insert(1, (0, 1, Move::Retiring));
        wl.open(&mut w, 1);
        wl.moves.remove(&1);
        assert_eq!(host_core(&wl, 1), None);
    }

    #[test]
    fn a_restarted_replica_opens_again_where_its_lsm_and_core_let_it() {
        let (mut w, mut wl) = world(2, 1);
        let core = wl.running(&mut w, 2).expect("running");
        // Back from a crash, before the workload opens anything: its log has the range.
        wl.on_crash(&mut w, 2);
        w.crash_node(2);
        w.start_node(2).expect("started");
        assert_eq!(wl.running(&mut w, 2), None);
        // No LSM open yet: nothing opens.
        wl.open(&mut w, 2);
        assert_eq!(wl.running(&mut w, 2), None);
        // Its core frozen: nothing opens until it thaws.
        wl.open_lsms(&mut w, 2);
        freeze(&mut w, 2, core);
        wl.open(&mut w, 2);
        assert_eq!(wl.running(&mut w, 2), None);
        w.nodes[2].cores[core as usize].frozen_until = Nanos::ZERO;
        wl.open(&mut w, 2);
        assert_eq!(wl.running(&mut w, 2), Some(core));
        assert!(wl.hosts.contains_key(&2));
        // A host over a core whose LSM is shut cannot open.
        let lsm = w.nodes[2].cores[core as usize].lsm.take();
        wl.open_host(&mut w, 2, core);
        assert!(!wl.hosts.contains_key(&2));
        w.nodes[2].cores[core as usize].lsm = lsm;
    }

    #[test]
    fn a_node_that_never_held_the_range_creates_it_on_the_descriptors_core() {
        let (mut w, mut wl) = world(2, 1);
        let core = wl.running(&mut w, 2).expect("running");
        w.cluster_core(2, core).expect("up").remove_group(G).expect("removed");
        let now = w.now();
        crate::drive_until(&mut w, &mut wl, now, |_| false);
        wl.hosts.remove(&2);
        let lsm = w.nodes[2].cores[wl.core as usize].lsm.take();
        wl.open(&mut w, 2);
        assert_eq!(wl.running(&mut w, 2), None, "no LSM on the descriptor's core");
        w.nodes[2].cores[wl.core as usize].lsm = lsm;
        wl.open(&mut w, 2);
        assert_eq!(wl.running(&mut w, 2), Some(wl.core));
        let sm = w.paused_core(2, wl.core).and_then(|c| c.state_machine(G)).and_then(as_range);
        assert!(sm.is_some_and(RangeSm::needs_snapshot), "born: it waits for a snapshot");
    }

    #[test]
    fn requests_and_answers_a_node_cannot_take_are_dropped() {
        let (mut w, mut wl) = world(2, 1);
        let ops = wl.history.ops.len();
        // Every node down while its host stays: no request goes.
        for n in 0..3 {
            w.nodes[n].alive = false;
        }
        for client in 0..CLIENTS {
            wl.act(&mut w, client);
        }
        for n in 0..3 {
            w.nodes[n].alive = true;
        }
        assert_eq!(wl.history.ops.len(), ops);
        // Answers to requests nobody waits for any more.
        for client in 0..CLIENTS {
            wl.act(&mut w, client);
        }
        wl.pending.clear();
        let until = w.now() + Nanos::from_millis(500);
        crate::drive_until(&mut w, &mut wl, until, |_| false);
        // Answers of a node without host, of another core, or of a core that is gone.
        let r = || Err(OpError::Timeout);
        let (hc, host) = wl.hosts.remove(&0).expect("a host");
        wl.on_done(&mut w, 0, hc, OpId(1), r());
        wl.hosts.insert(0, (hc, host));
        wl.on_done(&mut w, 0, 1 - hc, OpId(1), r());
        let cluster = w.nodes[0].cores[hc as usize].cluster.take();
        wl.on_done(&mut w, 0, hc, OpId(1), r());
        w.nodes[0].cores[hc as usize].cluster = cluster;
    }

    #[test]
    fn one_core_never_moves_and_a_move_needs_an_open_lsm() {
        let (mut w, mut wl) = world(1, 1);
        wl.next_move = Nanos::ZERO;
        wl.lead(&mut w);
        assert!(wl.intent.is_none(), "no other core");
        let (mut w, mut wl) = world(2, 3);
        // A halted replica that applied `Relocate` starts no hand-over without its LSM.
        let until = Nanos::from_secs(20);
        let halted = |w: &mut World| {
            (0..3u32).find(|n| {
                (0..2).any(|c| {
                    w.cluster_core(*n, c)
                        .and_then(|x| x.state_machine(G))
                        .and_then(as_range)
                        .is_some_and(RangeSm::halted)
                })
            })
        };
        assert!(crate::drive_until(&mut w, &mut wl, until, |w| halted(w).is_some()));
        let n = halted(&mut w).expect("halted");
        wl.moves.remove(&n);
        let from = wl.running(&mut w, n).expect("running");
        let lsm = w.nodes[n as usize].cores[from as usize].lsm.take();
        wl.start_move(&mut w, n);
        w.nodes[n as usize].cores[from as usize].lsm = lsm;
        assert!(!wl.moves.contains_key(&n));
    }

    #[test]
    fn a_running_replica_without_host_gets_one_and_a_frozen_core_holds_its_move() {
        let (mut w, mut wl) = world(2, 1);
        let core = wl.running(&mut w, 0).expect("running");
        wl.hosts.remove(&0);
        wl.open(&mut w, 0);
        assert_eq!(wl.hosts.get(&0).map(|(c, _)| *c), Some(core));
        // Either core frozen: the move waits, as it is.
        for frozen in [0, 1] {
            wl.moves.insert(0, (0, 1, Move::Retiring));
            freeze(&mut w, 0, frozen);
            wl.move_step(&mut w, 0);
            assert!(matches!(wl.moves.get(&0), Some((0, 1, Move::Retiring))));
            w.nodes[0].cores[frozen as usize].frozen_until = Nanos::ZERO;
        }
        wl.moves.remove(&0);
    }
}
