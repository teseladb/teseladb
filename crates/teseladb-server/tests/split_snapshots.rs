//! Snapshots of ranges that split while a replica joins, under lost, late and reordered frames and
//! partitions of the joining node (007 FR-005, FR-007, FR-012, D-067). Two nodes hold a range
//! that takes writes and splits again and again; a third node joins every range as a learner and
//! gets each one as a snapshot whose data it asks the other replicas for (`Transfers`), as the
//! server does. A range's data travels as it is when it is sent, so a snapshot from before the
//! range's last split would arrive without the part that split gave away: the joining replica
//! ingested it over the whole old interval (clearing the halves it already had) and replayed the
//! split over nothing. Every seed must end with every acknowledged write on every node.
//!
//! The run is deterministic for its seed; `TESELADB_SPLIT_SEEDS` sets how many seeds run.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig};
use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, OpId, PeerFrame, StateMachine};
use teseladb_range::command::{Command, Put};
use teseladb_range::sm::{RangeSm, split_value};
use teseladb_server::coresm::CoreSm;
use teseladb_server::forward::Request;
use teseladb_server::transfer::Transfers;
use teseladb_raft::{ConfChange, ConfChangeKind, Role};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const TICK: Nanos = Nanos::from_millis(10);
const WALL0: u64 = 1_800_000_000_000;
/// Seeds run when `TESELADB_SPLIT_SEEDS` does not say.
const SEEDS: u64 = 40;
/// Ticks with writes, splits, the join and faults; then ticks without faults to settle.
const BUSY_TICKS: u32 = 600;
const SETTLE_TICKS: u32 = 4_000;
/// The joining node.
const JOINER: NodeId = NodeId(3);
const FOUNDERS: [NodeId; 2] = [NodeId(1), NodeId(2)];
/// Share of frames lost, and the longest delay of one, in ticks, while faults run.
const LOSS: f64 = 0.08;
const MAX_DELAY_TICKS: u64 = 4;
/// A range splits once it holds this many acknowledged documents, up to this many ranges.
const SPLIT_DOCS: usize = 6;
const MAX_RANGES: usize = 12;
/// Value type of the documents.
const DOC: u8 = 3;
/// The newest mark.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// xorshift64*: the test's only source of randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

struct Node {
    core: MultiRaftCore<CoreSm>,
    disk: MemDisk,
    lsm: Rc<RefCell<Lsm>>,
    t: Transfers,
}

/// What the test asked a range's leader for.
#[derive(Clone, Debug)]
enum Asked {
    Write(Vec<u8>),
    Split,
    Genesis,
    Learner,
}

/// A split that was applied: its parent, cut and new range.
#[derive(Clone, Debug)]
struct Cut {
    parent: GroupId,
    at: Vec<u8>,
    new: GroupId,
}

struct Run {
    rng: Rng,
    now: Nanos,
    nodes: BTreeMap<NodeId, Node>,
    /// Frames on their way: `(due tick, order, from, to, frame)`.
    wire: Vec<(u64, u64, NodeId, NodeId, PeerFrame)>,
    order: u64,
    tick: u64,
    faults: bool,
    /// Until which tick the joining node is cut off.
    cut_off_until: u64,
    /// Every range and its bounds, as the applied splits left them.
    table: BTreeMap<GroupId, (Vec<u8>, Vec<u8>)>,
    cuts: Vec<Cut>,
    /// Splits proposed and not known to be applied or dropped yet.
    proposed: BTreeMap<GroupId, (Vec<u8>, GroupId)>,
    next_range: u64,
    /// Operations in flight, by range: one at a time.
    ops: BTreeMap<GroupId, (NodeId, OpId, Asked)>,
    /// Documents acknowledged, by key.
    acked: BTreeSet<Vec<u8>>,
    next_doc: u64,
    joining: bool,
}

fn value_of(key: &[u8]) -> Vec<u8> {
    [b"v-", key].concat()
}

impl Run {
    fn new(seed: u64) -> Run {
        let mut nodes = BTreeMap::new();
        for id in [FOUNDERS[0], FOUNDERS[1], JOINER] {
            let mut disk = MemDisk::new();
            let cfg = LsmConfig {
                memtable_bytes: 8 << 10,
                block_bytes: 4096,
                target_file_bytes: 16 << 10,
                cache_bytes: 1 << 20,
                ..LsmConfig::default()
            };
            let (lsm, _) = Lsm::recover(&cfg, &mut disk).expect("an empty LSM");
            let lsm = Rc::new(RefCell::new(lsm));
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk)
                .expect("an empty store");
            let cfg = CoreConfig {
                snapshot_entries: 8,
                op_timeout: Nanos::from_secs(3),
                instance: id.0 * 1_000 + seed,
                ..CoreConfig::new(id, 0)
            };
            let core = MultiRaftCore::new(cfg, store, seed ^ id.0);
            let t = Transfers::new(id, PathBuf::from("transfer"), id.0);
            nodes.insert(id, Node { core, disk, lsm, t });
        }
        let mut run = Run {
            rng: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
            now: Nanos::ZERO,
            nodes,
            wire: Vec::new(),
            order: 0,
            tick: 0,
            faults: true,
            cut_off_until: 0,
            table: BTreeMap::new(),
            cuts: Vec::new(),
            proposed: BTreeMap::new(),
            next_range: DATA_RANGE.0 + 1,
            ops: BTreeMap::new(),
            acked: BTreeSet::new(),
            next_doc: 0,
            joining: false,
        };
        run.table.insert(DATA_RANGE, (DATA_START.to_vec(), DATA_END.to_vec()));
        for id in FOUNDERS {
            run.open(id, DATA_RANGE, &FOUNDERS, &[]);
        }
        run
    }

    /// Opens range `g` on `id` with the bounds of the table; `false` if it has to wait.
    fn open(&mut self, id: NodeId, g: GroupId, voters: &[NodeId], learners: &[NodeId]) -> bool {
        let (start, end) = self.table[&g].clone();
        let n = self.nodes.get_mut(&id).expect("a node");
        let mut sm = match RangeSm::try_open(g, start, end, n.lsm.clone()) {
            Ok(Some(sm)) => sm,
            Ok(None) => {
                n.lsm.borrow_mut().run_io(&mut n.disk, self.now);
                return false;
            }
            Err(e) => panic!("{id:?} cannot open {g:?}: {e}"),
        };
        // A replica opened with nothing of its range gets it as a snapshot, as the server's do;
        // here the first range has two founders (the server's has one), which take its log.
        if g != DATA_RANGE {
            sm.expect_snapshot();
        }
        let desc = GroupDesc { group: g, voters: voters.to_vec(), learners: learners.to_vec() };
        n.core.create_group(desc, CoreSm::Range(sm)).expect("a new group");
        true
    }

    fn sm(&self, id: NodeId, g: GroupId) -> Option<&RangeSm> {
        self.nodes[&id].core.state_machine(g).and_then(CoreSm::range)
    }

    fn leader(&self, g: GroupId) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.core.status(g).is_some_and(|s| s.role == Role::Leader))
            .max_by_key(|(_, n)| n.core.status(g).map(|s| s.hard_state.term))
            .map(|(id, _)| *id)
    }

    // ----- the cluster -----

    fn pump(&mut self, id: NodeId) {
        let now = self.now;
        for _ in 0..10_000 {
            let n = self.nodes.get_mut(&id).expect("a node");
            let mut moved = false;
            while let Some(op) = n.core.poll_io(now) {
                let d = n.disk.execute(op);
                n.core.on_io_done(d, now);
                moved = true;
            }
            let op = n.lsm.borrow_mut().poll_io(now);
            let mut c = n.lsm.borrow_mut().take_completions();
            if let Some(op) = op {
                let d = n.disk.execute(op);
                let more = n.lsm.borrow_mut().on_io_done(d, now);
                c.exported.extend(more.exported);
                c.ingested.extend(more.ingested);
                c.cleared.extend(more.cleared);
                moved = true;
            }
            let lsm = n.lsm.clone();
            for (eid, files) in c.exported {
                n.t.on_exported(eid, files);
            }
            for (iid, r) in c.ingested {
                let ready = n.t.on_ingested(&n.core, &mut lsm.borrow_mut(), iid, r, now);
                // The core restores the range, which reads the LSM: no borrow is held then.
                if let Some(g) = ready {
                    n.core.snapshot_data_ready(g, now);
                }
            }
            for cid in c.cleared {
                n.t.on_cleared(&n.core, &mut lsm.borrow_mut(), cid);
            }
            if let Some(op) = n.t.poll_io() {
                let d = n.disk.execute(op);
                n.t.on_io_done(&mut n.core, &mut lsm.borrow_mut(), d, now);
                moved = true;
            }
            n.core.take_rolled_back();
            n.core.rollback_durable();
            let a = n.core.drain_actions();
            for (g, index) in a.snapshot_data {
                let sm = n.core.state_machine(g).and_then(CoreSm::range);
                let snap = n.core.awaited_snapshot(g);
                let end = sm.zip(snap).and_then(|(sm, s)| sm.snapshot_end(s)).unwrap_or_default();
                n.t.want(g, index, end, now);
                moved = true;
            }
            for f in a.forwarded {
                moved = true;
                if let Ok(Request::Fetch(fetch)) = Request::decode(&f.body) {
                    n.t.on_fetch(&mut n.core, &mut lsm.borrow_mut(), (f.from, f.req), fetch, now);
                }
            }
            let mut ended = Vec::new();
            for (op, r) in a.done {
                moved = true;
                if !n.t.on_done(&mut n.core, &mut lsm.borrow_mut(), op, &r, now) {
                    ended.push((op, r.is_ok()));
                }
            }
            for (to, frame) in a.send {
                moved = true;
                self.send(id, to, frame);
            }
            for (op, ok) in ended {
                self.ended(id, op, ok);
            }
            if !moved {
                return;
            }
        }
        panic!("{id:?} never settles");
    }

    fn send(&mut self, from: NodeId, to: NodeId, frame: PeerFrame) {
        let cut_off = self.tick < self.cut_off_until && (from == JOINER || to == JOINER);
        if self.faults && (cut_off || self.rng.chance(LOSS)) {
            return;
        }
        let delay = if self.faults { self.rng.below(MAX_DELAY_TICKS + 1) } else { 0 };
        self.order += 1;
        self.wire.push((self.tick + delay, self.order, from, to, frame));
    }

    fn deliver(&mut self) {
        let tick = self.tick;
        let (due, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.wire).into_iter().partition(|(at, ..)| *at <= tick);
        self.wire = later;
        let mut due = due;
        // Late frames arrive in any order among those due.
        due.sort_by_key(|(at, order, ..)| (*at, *order));
        if self.faults && due.len() > 1 {
            let i = self.rng.below(due.len() as u64) as usize;
            let j = self.rng.below(due.len() as u64) as usize;
            due.swap(i, j);
        }
        for (_, _, from, to, frame) in due {
            let now = self.now;
            self.nodes.get_mut(&to).expect("a node").core.on_peer_message(from, frame, now);
            self.pump(to);
        }
    }

    fn step(&mut self) {
        self.tick += 1;
        self.now = self.now + TICK;
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in &ids {
            let now = self.now;
            let replicas = self.replicas();
            let n = self.nodes.get_mut(id).expect("a node");
            n.core.on_tick(now, WALL0 + now.as_millis());
            n.t.forget_unawaited(&n.core);
            n.t.tick(&mut n.core, &replicas, now);
            for g in n.t.take_stale() {
                n.core.reject_awaited(g);
            }
            // The memtable goes to disk now and then, so that the log is released.
            if self.tick.is_multiple_of(10) {
                n.lsm.borrow_mut().flush();
            }
            self.pump(*id);
        }
        self.deliver();
        self.follow_cuts();
        self.duties();
    }

    fn replicas(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    // ----- splits as the server's replicas follow them -----

    /// Every node opens the ranges its parents' applied splits created, and the joining node
    /// opens the others once their parent no longer holds their start there.
    fn follow_cuts(&mut self) {
        // A split proposed is known applied once its leader's parent ends at or before its cut.
        let seen: Vec<(GroupId, Vec<u8>, GroupId)> = self
            .proposed
            .iter()
            .filter(|(p, (at, _))| {
                self.nodes
                    .keys()
                    .any(|id| self.sm(*id, **p).is_some_and(|sm| sm.bounds().1 == at.as_slice()))
            })
            .map(|(p, (at, new))| (*p, at.clone(), *new))
            .collect();
        for (parent, at, new) in seen {
            self.proposed.remove(&parent);
            let end = self.table[&parent].1.clone();
            self.table.insert(parent, (self.table[&parent].0.clone(), at.clone()));
            self.table.insert(new, (at.clone(), end));
            self.cuts.push(Cut { parent, at, new });
        }
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in ids {
            for c in self.cuts.clone() {
                let applied =
                    self.sm(id, c.parent).is_some_and(|sm| sm.bounds().1 <= c.at.as_slice());
                if applied && self.sm(id, c.new).is_none() {
                    let conf = self.nodes[&id].core.conf(c.parent).cloned().unwrap_or_default();
                    self.open(id, c.new, &conf.voters, &conf.learners);
                }
            }
        }
        if !self.joining {
            return;
        }
        let groups: Vec<GroupId> = self.table.keys().copied().collect();
        for g in groups {
            if self.sm(JOINER, g).is_some() {
                continue;
            }
            let start = self.table[&g].0.clone();
            let uncut = self.nodes[&JOINER].core.group_ids().iter().any(|p| {
                self.sm(JOINER, *p).is_some_and(|sm| {
                    sm.bounds().0 < start.as_slice() && start.as_slice() < sm.bounds().1
                })
            });
            if !uncut {
                self.open(JOINER, g, &FOUNDERS, &[JOINER]);
            }
        }
    }

    /// The leaders' duties: one operation at a time per range, proposed only once the leader
    /// applied its whole log, so that no write ever lands past a split that cut its key away.
    fn duties(&mut self) {
        let groups: Vec<GroupId> = self.table.keys().copied().collect();
        for g in groups {
            if self.ops.contains_key(&g) {
                continue;
            }
            let Some(l) = self.leader(g) else { continue };
            let Some(st) = self.nodes[&l].core.status(g) else { continue };
            if st.applied < st.last_index || self.proposed.contains_key(&g) {
                continue;
            }
            let Some(sm) = self.sm(l, g) else { continue };
            let (start, end) = (sm.bounds().0.to_vec(), sm.bounds().1.to_vec());
            if (start.clone(), end.clone()) != self.table[&g] {
                continue;
            }
            if let Some((parent, at_index)) = sm.born() {
                let cmd = Command::SplitGenesis { parent, at_index, end };
                self.propose(l, g, &cmd, Asked::Genesis);
                continue;
            }
            if self.joining && self.add_learner(l, g) {
                continue;
            }
            if !self.faults {
                continue;
            }
            let docs: Vec<&Vec<u8>> = self.acked.range(start.clone()..end.clone()).collect();
            if docs.len() >= SPLIT_DOCS && self.table.len() < MAX_RANGES && self.rng.chance(0.3) {
                let at = docs[docs.len() / 2].clone();
                let new = GroupId(self.next_range);
                self.next_range += 1;
                let cmd = Command::Split {
                    at: at.clone(),
                    new_range: new,
                    records: vec![],
                    right_live: 0,
                };
                if self.propose(l, g, &cmd, Asked::Split) {
                    self.proposed.insert(g, (at, new));
                }
                continue;
            }
            // A new document in this range.
            self.next_doc += 1;
            let key =
                [&start[..], format!("{:08}", self.rng.below(100_000_000)).as_bytes()].concat();
            if key >= end || self.acked.contains(&key) {
                continue;
            }
            let put = Put { key: key.clone(), value_type: DOC, value: value_of(&key) };
            let cmd = Command::Write {
                puts: vec![put],
                deletes: vec![],
                dedup: None,
                live_delta: 0,
                docs: None,
            };
            self.propose(l, g, &cmd, Asked::Write(key));
        }
    }

    /// The leader of `g` adds the joining node as a learner once its log was compacted past its
    /// last split, as the server's replicas do (D-067); `true` if it is busy with that.
    fn add_learner(&mut self, l: NodeId, g: GroupId) -> bool {
        let conf = self.nodes[&l].core.conf(g).cloned().unwrap_or_default();
        if conf.learners.contains(&JOINER) || conf.voters.contains(&JOINER) {
            return false;
        }
        let n = self.nodes.get_mut(&l).expect("a node");
        let floor = n.core.state_machine(g).map_or(0, StateMachine::snapshot_floor);
        let saved = n.core.store().group(g).map_or(0, |log| log.snapshot_index);
        if saved == 0 || saved < floor {
            let ready = n.core.state_machine(g).and_then(CoreSm::range).is_some_and(|sm| {
                sm.applied_index() > 0 && (sm.last_write() > 0 || !sm.from_split())
            });
            if ready && saved == 0 {
                n.core.snapshot_now(g);
            }
            return true;
        }
        let cc = ConfChange { kind: ConfChangeKind::AddLearner, node: JOINER };
        if let Ok(op) = n.core.propose_conf_change(g, cc, self.now) {
            self.ops.insert(g, (l, op, Asked::Learner));
        }
        true
    }

    fn propose(&mut self, l: NodeId, g: GroupId, cmd: &Command, asked: Asked) -> bool {
        let now = self.now;
        let n = self.nodes.get_mut(&l).expect("a node");
        match n.core.propose(g, &cmd.encode(), now) {
            Ok(op) => {
                self.ops.insert(g, (l, op, asked));
                true
            }
            Err(_) => false,
        }
    }

    fn ended(&mut self, id: NodeId, op: OpId, ok: bool) {
        let Some(g) = self.ops.iter().find(|(_, (n, o, _))| *n == id && *o == op).map(|(g, _)| *g)
        else {
            return;
        };
        let (_, _, asked) = self.ops.remove(&g).expect("found above");
        if let (Asked::Write(key), true) = (asked, ok) {
            self.acked.insert(key);
        }
    }

    // ----- the check -----

    /// Every node's ranges cover the data space, and every acknowledged document is in its LSM
    /// in the range that holds its key there.
    fn check(&mut self, seed: u64) {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in &ids {
            let mut bounds: Vec<(Vec<u8>, Vec<u8>)> = self.nodes[id]
                .core
                .group_ids()
                .iter()
                .filter_map(|g| self.sm(*id, *g))
                .map(|sm| (sm.bounds().0.to_vec(), sm.bounds().1.to_vec()))
                .collect();
            bounds.sort();
            let chained = bounds.windows(2).all(|w| w[0].1 == w[1].0);
            let whole = bounds.first().is_some_and(|b| b.0 == DATA_START)
                && bounds.last().is_some_and(|b| b.1 == DATA_END);
            assert!(chained && whole, "seed {seed}: {id:?} holds {bounds:?}");
            let (now, splits) = (self.now, self.cuts.len());
            let n = self.nodes.get_mut(id).expect("a node");
            for key in &self.acked {
                let found = loop {
                    let view = n.lsm.borrow().view();
                    let got = view.get(&mut n.lsm.borrow_mut(), key, NEWEST);
                    match got {
                        Lookup::Hit(f) => break split_value(&f.value).map(|(_, v)| v.to_vec()),
                        Lookup::NotFound => break None,
                        Lookup::Failed(e) => panic!("seed {seed}: {id:?}: {e}"),
                        Lookup::Miss(_) => {
                            n.lsm.borrow_mut().run_io(&mut n.disk, now);
                        }
                    }
                };
                assert_eq!(
                    found,
                    Some(value_of(key)),
                    "seed {seed}: {id:?} lost {:?} ({} splits, {} documents)",
                    String::from_utf8_lossy(key),
                    splits,
                    self.acked.len()
                );
            }
        }
    }

    /// Whether every replica of every range is there and caught up with its leader.
    fn settled(&self) -> bool {
        self.table.keys().all(|g| {
            let Some(l) = self.leader(*g) else { return false };
            let target = self.nodes[&l].core.status(*g).map_or(0, |s| s.applied);
            let conf = self.nodes[&l].core.conf(*g).cloned().unwrap_or_default();
            conf.learners.contains(&JOINER)
                && self.nodes.iter().all(|(id, n)| {
                    !n.t.waiting()
                        && self.sm(*id, *g).is_some_and(|sm| sm.applied_index() >= target)
                })
        }) && self.ops.is_empty()
    }
}

impl Run {
    /// What keeps the run from settling, for the failure message.
    fn unsettled(&self) -> String {
        let mut out = format!("ops {:?}; ", self.ops.keys().collect::<Vec<_>>());
        for g in self.table.keys() {
            let leader = self.leader(*g);
            let target =
                leader.and_then(|l| self.nodes[&l].core.status(*g)).map_or(0, |s| s.applied);
            let conf = leader.and_then(|l| self.nodes[&l].core.conf(*g).cloned());
            let applied: Vec<Option<u64>> = self
                .nodes
                .keys()
                .map(|id| self.sm(*id, *g).map(StateMachine::applied_index))
                .collect();
            let snap = leader.map(|l| {
                let c = &self.nodes[&l].core;
                (c.snapshot_index(*g), c.state_machine(*g).map(StateMachine::snapshot_floor))
            });
            let parked = self.nodes[&JOINER]
                .core
                .awaited_snapshot(*g)
                .and_then(|b| b.get(8..16))
                .map(|b| b.to_vec());
            let raft: Vec<_> = self
                .nodes
                .values()
                .map(|n| {
                    n.core.status(*g).map(|s| {
                        (s.role, s.leader, s.hard_state.commit, s.last_index, s.progress.clone())
                    })
                })
                .collect();
            out += &format!(
                "{g:?} raft {raft:?} leader {leader:?} at {target} conf {conf:?} applied {applied:?} snap {snap:?} parked {parked:?}; "
            );
        }
        let waiting: Vec<Option<GroupId>> = self.nodes.values().map(|n| n.t.sink_group()).collect();
        out + &format!("waiting {waiting:?}")
    }
}

fn run(seed: u64) {
    let mut r = Run::new(seed);
    let join_at = 100 + r.rng.below(200);
    for tick in 0..BUSY_TICKS {
        if u64::from(tick) == join_at {
            r.joining = true;
        }
        if r.joining && r.cut_off_until <= r.tick && r.rng.chance(0.02) {
            r.cut_off_until = r.tick + 20 + r.rng.below(40);
        }
        r.step();
    }
    r.faults = false;
    r.cut_off_until = 0;
    for _ in 0..SETTLE_TICKS {
        r.step();
        if r.settled() {
            break;
        }
    }
    assert!(r.settled(), "seed {seed}: never settled: {}", r.unsettled());
    assert!(!r.cuts.is_empty(), "seed {seed}: no split");
    r.check(seed);
}

#[test]
fn snapshots_across_splits_lose_nothing_under_faults() {
    let seeds =
        std::env::var("TESELADB_SPLIT_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(SEEDS);
    let first = std::env::var("TESELADB_SPLIT_FIRST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1u64);
    for seed in first..first + seeds {
        run(seed);
    }
}
