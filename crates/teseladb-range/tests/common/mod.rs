//! The range host on three replicas over `MemDisk`, in logical time, for the tests that
//! drive leaders, partitions and slow disks by hand. Each test binary uses part of it.
#![allow(dead_code)]

pub use std::cell::RefCell;
pub use std::collections::{BTreeMap, BTreeSet, VecDeque};
pub use std::rc::Rc;

pub use teseladb_hlc::Hlc;
pub use teseladb_lsm::{Lsm, LsmConfig};
pub use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock};
pub use teseladb_range::ack::{W, WriteConcern};
pub use teseladb_range::command::{DedupKey, Put};
pub use teseladb_range::host::{
    Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, ScanReq, WriteReq,
};
pub use teseladb_range::mutate::Outcome;
pub use teseladb_range::read::ReadConcern;
pub use teseladb_range::sm::RangeSm;
pub use teseladb_raft::Role;
pub use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
pub use teseladb_types::{GroupId, Nanos, NodeId};

pub const G: GroupId = GroupId(7);
pub const START: &[u8] = b"\x01a";
pub const END: &[u8] = b"\x01z";
pub const TICK: Nanos = Nanos::from_millis(10);
pub const WALL0: u64 = 1_800_000_000_000;
pub const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };

pub struct Node {
    pub disk: MemDisk,
    pub core: MultiRaftCore<RangeSm>,
    pub host: RangeHost,
    pub replies: Vec<(ReqId, Result<Reply, RangeError>)>,
    /// What the core handed out to roll back, as its rollback files hold it (005 FR-010).
    pub rolled: Vec<(teseladb_types::GroupId, teseladb_raftlog::LogEntry)>,
    /// The node's HLC, shared by its core and its host.
    pub clock: NodeClock,
}

pub struct Cluster {
    pub nodes: BTreeMap<NodeId, Node>,
    pub now: Nanos,
    pub cut: BTreeSet<(NodeId, NodeId)>,
    /// Nodes whose disk does not answer.
    pub slow: BTreeSet<NodeId>,
    /// Milliseconds every wall clock is ahead of the monotonic time (cluster time moved on).
    pub wall_ahead: u64,
    /// Milliseconds one node's wall clock is off the others' (a jump of that node's clock).
    pub skew: BTreeMap<NodeId, i64>,
}

pub fn id(i: u64) -> NodeId {
    NodeId(i)
}

impl Cluster {
    pub fn new(lsm: LsmConfig, pending: usize) -> Cluster {
        let mut c = Cluster {
            nodes: BTreeMap::new(),
            now: Nanos::ZERO,
            cut: BTreeSet::new(),
            slow: BTreeSet::new(),
            wall_ahead: 0,
            skew: BTreeMap::new(),
        };
        let voters: Vec<NodeId> = (1..=3).map(id).collect();
        for i in 1..=3 {
            let mut disk = MemDisk::new();
            let clock = NodeClock::for_node(id(i));
            let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
            let cfg = CoreConfig {
                clock: clock.clone(),
                instance: i * 1_000,
                max_pending_proposals: pending,
                ..CoreConfig::new(id(i), 0)
            };
            let mut core = MultiRaftCore::new(cfg, store, i);
            let l = Rc::new(RefCell::new(Lsm::recover(&lsm, &mut disk).unwrap().0));
            let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), l.clone(), &mut disk).unwrap();
            core.create_group(GroupDesc { group: G, voters: voters.clone(), learners: vec![] }, sm)
                .unwrap();
            let mut host = RangeHost::new(l, clock.clone());
            host.add_range(G, START.to_vec(), END.to_vec());
            let node = Node { disk, core, host, replies: vec![], rolled: vec![], clock };
            c.nodes.insert(id(i), node);
        }
        c.settle();
        c
    }

    pub fn standard() -> Cluster {
        Cluster::new(LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() }, 1024)
    }

    pub fn wall(&self) -> u64 {
        WALL0 + self.wall_ahead + self.now.0 / 1_000_000
    }

    /// The wall clock of node `n`, with its skew.
    pub fn wall_of(&self, n: NodeId) -> u64 {
        self.wall().saturating_add_signed(self.skew.get(&n).copied().unwrap_or(0))
    }

    pub fn settle(&mut self) {
        for _ in 0..100_000 {
            let mut moved = false;
            let mut queue = VecDeque::new();
            let now = self.now;
            let walls: BTreeMap<NodeId, u64> =
                self.nodes.keys().map(|&n| (n, self.wall_of(n))).collect();
            for (&n, node) in &mut self.nodes {
                let wall = walls[&n];
                // The rollback files are written at once and are durable.
                node.rolled.extend(node.core.take_rolled_back());
                node.core.rollback_durable();
                if !self.slow.contains(&n) {
                    while let Some(op) = node.core.poll_io(now) {
                        let d = node.disk.execute(op);
                        node.core.on_io_done(d, now);
                        moved = true;
                    }
                    node.host.lsm().borrow_mut().run_io(&mut node.disk, now);
                }
                let a = node.core.drain_actions();
                for (op, r) in a.done {
                    moved |= node.host.on_done(&mut node.core, op, r);
                }
                moved |= node.host.poll(&mut node.core, now, wall);
                let replies = node.host.take_replies();
                moved |= !replies.is_empty();
                node.replies.extend(replies);
                for (to, f) in a.send {
                    queue.push_back((n, to, f));
                    moved = true;
                }
            }
            while let Some((from, to, f)) = queue.pop_front() {
                if !self.cut.contains(&(from, to)) {
                    self.nodes.get_mut(&to).unwrap().core.on_peer_message(from, f, now);
                }
            }
            if !moved {
                return;
            }
        }
        panic!("the cluster never settles");
    }

    pub fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.now = self.now + TICK;
            let now = self.now;
            let walls: BTreeMap<NodeId, u64> =
                self.nodes.keys().map(|&n| (n, self.wall_of(n))).collect();
            for (n, node) in &mut self.nodes {
                node.core.on_tick(now, walls[n]);
            }
            self.settle();
        }
    }

    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.core.status(G).is_some_and(|s| s.role == Role::Leader))
            .max_by_key(|(_, n)| n.core.status(G).unwrap().hard_state.term)
            .map(|(i, _)| *i)
    }

    pub fn elect(&mut self) -> NodeId {
        for _ in 0..1_000 {
            self.ticks(1);
            if let Some(l) = self.leader() {
                let s = self.nodes[&l].core.status(G).unwrap();
                if s.applied >= s.hard_state.commit && s.hard_state.commit > 0 {
                    return l;
                }
            }
        }
        panic!("no leader");
    }

    pub fn others(&self, n: NodeId) -> Vec<NodeId> {
        self.nodes.keys().copied().filter(|&o| o != n).collect()
    }

    pub fn isolate(&mut self, n: NodeId) {
        for o in self.others(n) {
            self.cut.insert((n, o));
            self.cut.insert((o, n));
        }
    }

    pub fn mutate(&mut self, n: NodeId, key: &[u8], value: u8, wc: WriteConcern) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        let w =
            WriteReq { g: G, keys: vec![key.to_vec()], m: Box::new(Set(value)), dedup: None, wc };
        node.host.mutate(&mut node.core, w, now, wall)
    }

    /// A write of `m` on `keys` with an idempotence key.
    pub fn mutate_with(
        &mut self,
        n: NodeId,
        keys: &[&[u8]],
        m: Box<dyn Mutation>,
        dedup: Option<DedupKey>,
    ) -> ReqId {
        self.mutate_with_wc(n, keys, m, dedup, MAJORITY)
    }

    /// [`Cluster::mutate_with`] with write concern `wc`.
    pub fn mutate_with_wc(
        &mut self,
        n: NodeId,
        keys: &[&[u8]],
        m: Box<dyn Mutation>,
        dedup: Option<DedupKey>,
        wc: WriteConcern,
    ) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        let keys = keys.iter().map(|k| k.to_vec()).collect();
        let w = WriteReq { g: G, keys, m, dedup, wc };
        node.host.mutate(&mut node.core, w, now, wall)
    }

    pub fn get(&mut self, n: NodeId, key: &[u8], concern: ReadConcern) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        node.host.get(&mut node.core, G, key, concern, now, wall)
    }

    pub fn scan(&mut self, n: NodeId, start: &[u8], end: &[u8], limit: usize) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        let r = ScanReq {
            g: G,
            start: start.to_vec(),
            end: end.to_vec(),
            concern: ReadConcern::Majority,
            limit,
            backward: false,
        };
        node.host.scan(&mut node.core, r, now, wall)
    }

    /// A scan of `[start, end)` from its last key down, at most `limit` documents (H-003).
    pub fn scan_back(&mut self, n: NodeId, start: &[u8], end: &[u8], limit: usize) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        let r = ScanReq {
            g: G,
            start: start.to_vec(),
            end: end.to_vec(),
            concern: ReadConcern::Majority,
            limit,
            backward: true,
        };
        node.host.scan(&mut node.core, r, now, wall)
    }

    pub fn reply(&self, n: NodeId, r: ReqId) -> Option<Result<Reply, RangeError>> {
        self.nodes[&n].replies.iter().find(|(i, _)| *i == r).map(|(_, x)| x.clone())
    }
}

/// Writes one byte at its document.
pub struct Set(pub u8);

impl Mutation for Set {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        assert_eq!(current.len(), 1);
        Ok(Outcome {
            puts: vec![Put { key: KEY.to_vec(), value_type: 1, value: vec![self.0] }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

pub const KEY: &[u8] = b"\x01doc";

pub fn value(r: Option<Result<Reply, RangeError>>) -> Option<u8> {
    match r {
        Some(Ok(Reply::Value { value, .. })) => value.map(|(_, v)| v[0]),
        other => panic!("not a read: {other:?}"),
    }
}

/// Writes one byte at its own document.
pub struct SetKey(pub Vec<u8>, pub u8);

impl Mutation for SetKey {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome {
            puts: vec![Put { key: self.0.clone(), value_type: 1, value: vec![self.1] }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

impl Cluster {
    /// Delivers the checksums the replicas compute over a few ticks (a sum waits for the blocks
    /// it reads) to the range's leader, as the embedder does (a cut link loses them); returns
    /// every one sent.
    pub fn route_checksums(&mut self) -> Vec<teseladb_range::checksum::Report> {
        let mut all = Vec::new();
        for _ in 0..20 {
            self.ticks_following(1);
            let mut sent = Vec::new();
            for (&n, node) in &mut self.nodes {
                sent.extend(node.host.take_checksum_reports().into_iter().map(|r| (n, r)));
            }
            let leader = self.leader();
            for (from, r) in sent {
                if let Some(l) = leader.filter(|&l| !self.cut.contains(&(from, l))) {
                    self.nodes.get_mut(&l).unwrap().host.on_checksum_report(&r);
                }
                all.push(r);
            }
        }
        all
    }
}

/// Ticks after cluster time jumps (`wall_ahead`): the clock rule keeps a node out of margin for a
/// while after its wall clock jumps, and a leader proposes nothing meanwhile.
pub const JUMP_TICKS: u32 = 200;

impl Cluster {
    /// Settles the cluster and lets every host follow its state machines
    /// (`RangeHost::follow_state`), as an embedder does after each poll, until nothing moves.
    pub fn follow(&mut self) {
        for _ in 0..100_000 {
            self.settle();
            let mut moved = false;
            for node in self.nodes.values_mut() {
                moved |= node.host.follow_state(&node.core);
            }
            if !moved {
                return;
            }
        }
        panic!("the hosts never stop following their state machines");
    }

    /// `n` ticks, following the state machines after each.
    pub fn ticks_following(&mut self, n: u32) {
        for _ in 0..n {
            self.ticks(1);
            self.follow();
        }
    }
}

impl Cluster {
    /// A write of `value` at its own document `key` with write concern `wc`.
    pub fn mutate_key(&mut self, n: NodeId, key: &[u8], value: u8, wc: WriteConcern) -> ReqId {
        let (now, wall) = (self.now, self.wall_of(n));
        let node = self.nodes.get_mut(&n).unwrap();
        let m = Box::new(SetKey(key.to_vec(), value));
        let w = WriteReq { g: G, keys: vec![key.to_vec()], m, dedup: None, wc };
        node.host.mutate(&mut node.core, w, now, wall)
    }
}
