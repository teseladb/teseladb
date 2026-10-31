//! A small cluster of cores for the contract tests: every node has one core over a `MemDisk`, the
//! test drives time, the disk and the network by hand, and cuts links to make partitions.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Done, Forwarded, GroupDesc, MultiRaftCore, OpError, OpId,
    PeerFrame, RestoreError, StateMachine,
};
use teseladb_raft::Role;
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{Decoder, Encoder, GroupId, Nanos, NodeId};

pub const G: GroupId = GroupId(7);
pub const TICK: Nanos = Nanos::from_millis(10);
/// Wall clock of the first tick.
pub const WALL0: u64 = 1_800_000_000_000;

/// Appends every payload to a byte string; the output is the length after the entry. The
/// snapshot is the whole string, so a few large entries make a snapshot of several chunks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Appender {
    pub data: Vec<u8>,
    pub applied: u64,
    pub entries: u64,
    pub last_time: u64,
    /// Mark of the last entry applied: each one is above it (005 FR-004).
    pub last_ts: Option<teseladb_hlc::Hlc>,
    /// Refuse every snapshot (a version that cannot read them).
    pub fail_restore: bool,
    /// The data of the snapshots is not on this node yet (005: a range's LSM data).
    pub missing_data: std::rc::Rc<std::cell::Cell<bool>>,
    /// Its leader keeps a read cache this far above its election (005 FR-005).
    pub cache_margin: Option<u64>,
    /// Once it applied this index it applies nothing more (0: never), as a range that moved to
    /// another core (007 FR-011).
    pub halt_at: std::rc::Rc<std::cell::Cell<u64>>,
}

impl StateMachine for Appender {
    type Output = u64;

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<u64, ApplyError> {
        if payload == b"poison" {
            return Err(ApplyError("poison".into()));
        }
        assert!(ctx.index > self.applied, "applied in order");
        assert!(ctx.ts.physical >= self.last_time, "the entries' marks never go down");
        assert!(
            self.last_ts.is_none_or(|last| ctx.ts > last),
            "each entry's mark is above the last"
        );
        self.last_ts = Some(ctx.ts);
        self.data.extend_from_slice(payload);
        (self.applied, self.entries, self.last_time) =
            (ctx.index, self.entries + 1, ctx.ts.physical);
        Ok(self.data.len() as u64)
    }

    fn skip(&mut self, index: u64) {
        assert!(index > self.applied, "skipped in order");
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.applied).u64(self.entries).u64(self.last_time).bytes(&self.data);
        e.finish()
    }

    fn has_data_for(&self, _: &[u8]) -> bool {
        !self.missing_data.get()
    }

    fn read_cache_margin_ms(&self) -> Option<u64> {
        self.cache_margin
    }

    fn halted(&self) -> bool {
        self.halt_at.get() != 0 && self.applied >= self.halt_at.get()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        if self.fail_restore {
            return Err(RestoreError("refused by the test".into()));
        }
        let mut d = Decoder::new(snap);
        let read = |d: &mut Decoder<'_>| -> Result<Appender, teseladb_types::DecodeError> {
            let (applied, entries, last_time) = (d.u64()?, d.u64()?, d.u64()?);
            Ok(Appender {
                applied,
                entries,
                last_time,
                // The snapshot does not carry the mark: the entries after it are above it anyway.
                last_ts: None,
                data: d.bytes()?.to_vec(),
                fail_restore: false,
                missing_data: self.missing_data.clone(),
                cache_margin: self.cache_margin,
                halt_at: self.halt_at.clone(),
            })
        };
        *self = read(&mut d).map_err(|e| RestoreError(format!("{e:?}")))?;
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}

pub struct Node {
    pub core: MultiRaftCore<Appender>,
    pub disk: MemDisk,
    pub done: Vec<(OpId, Result<Done<u64>, OpError>)>,
    pub forwarded: Vec<Forwarded>,
    pub snapshot_data: Vec<(GroupId, u64)>,
    /// What the core handed out to roll back, as its rollback files would hold it (they survive
    /// a restart).
    pub rolled: Vec<(GroupId, teseladb_raftlog::LogEntry)>,
}

pub struct Cluster {
    pub nodes: BTreeMap<NodeId, Node>,
    pub now: Nanos,
    pub queue: VecDeque<(NodeId, NodeId, PeerFrame)>,
    pub cut: BTreeSet<(NodeId, NodeId)>,
    /// Every frame sent, `(from, to, frame)`.
    pub sent: Vec<(NodeId, NodeId, PeerFrame)>,
    /// Nodes whose disk does not answer (their operations wait).
    pub slow: BTreeSet<NodeId>,
    /// Frames for which it returns `true` are lost.
    pub drop: Option<fn(NodeId, NodeId, &PeerFrame) -> bool>,
    /// Milliseconds a node's wall clock is ahead of the others'.
    pub skew: BTreeMap<NodeId, u64>,
    /// Nodes whose rollback files are not written yet: their cores are not told they are durable.
    pub rollback_unwritten: BTreeSet<NodeId>,
}

pub fn id(i: u64) -> NodeId {
    NodeId(i)
}

pub fn cfg(i: u64) -> CoreConfig {
    CoreConfig { instance: i * 1_000, ..CoreConfig::new(id(i), 0) }
}

impl Cluster {
    /// `n` voters 1..=n with group `G` on each.
    pub fn new(n: u64) -> Cluster {
        Cluster::with(n, cfg)
    }

    pub fn with(n: u64, mk: impl Fn(u64) -> CoreConfig) -> Cluster {
        let mut c = Cluster {
            nodes: BTreeMap::new(),
            now: Nanos::ZERO,
            queue: VecDeque::new(),
            cut: BTreeSet::new(),
            sent: Vec::new(),
            slow: BTreeSet::new(),
            rollback_unwritten: BTreeSet::new(),
            drop: None,
            skew: BTreeMap::new(),
        };
        let voters: Vec<NodeId> = (1..=n).map(id).collect();
        for i in 1..=n {
            c.add_node(i, &mk);
            let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
            c.nodes.get_mut(&id(i)).unwrap().core.create_group(desc, Appender::default()).unwrap();
        }
        c.settle();
        c
    }

    /// A node without groups.
    pub fn add_node(&mut self, i: u64, mk: impl Fn(u64) -> CoreConfig) {
        let mut disk = MemDisk::new();
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let core = MultiRaftCore::new(mk(i), store, i);
        self.nodes.insert(
            id(i),
            Node {
                core,
                disk,
                done: vec![],
                forwarded: vec![],
                snapshot_data: vec![],
                rolled: vec![],
            },
        );
    }

    /// Restarts a node over what its disk kept: the core starts empty and the test attaches the
    /// state machines of `recovered_groups`.
    pub fn restart(&mut self, n: NodeId) {
        let mut node = self.nodes.remove(&n).unwrap();
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut node.disk).unwrap();
        node.core = MultiRaftCore::new(cfg(n.0), store, n.0 + 100);
        node.done.clear();
        self.nodes.insert(n, node);
    }

    /// Runs disk operations and collects actions until nothing moves, delivering the frames.
    pub fn settle(&mut self) {
        for _ in 0..100_000 {
            let mut moved = false;
            let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
            for n in ids {
                let now = self.now;
                let slow = self.slow.contains(&n);
                let node = self.nodes.get_mut(&n).unwrap();
                // The rollback files are written at once and are durable (005 FR-010).
                if !self.rollback_unwritten.contains(&n) {
                    node.rolled.extend(node.core.take_rolled_back());
                    node.core.rollback_durable();
                }
                while !slow && let Some(op) = node.core.poll_io(now) {
                    let d = node.disk.execute(op);
                    node.core.on_io_done(d, now);
                    moved = true;
                }
                let a = node.core.drain_actions();
                node.done.extend(a.done);
                node.forwarded.extend(a.forwarded);
                node.snapshot_data.extend(a.snapshot_data);
                for (to, f) in a.send {
                    self.sent.push((n, to, f.clone()));
                    self.queue.push_back((n, to, f));
                    moved = true;
                }
            }
            while let Some((from, to, f)) = self.queue.pop_front() {
                if self.cut.contains(&(from, to))
                    || !self.nodes.contains_key(&to)
                    || self.drop.is_some_and(|d| d(from, to, &f))
                {
                    continue;
                }
                let now = self.now;
                self.nodes.get_mut(&to).unwrap().core.on_peer_message(from, f, now);
            }
            if !moved {
                return;
            }
        }
        panic!("the cluster never settles");
    }

    pub fn tick(&mut self) {
        self.now = self.now + TICK;
        let wall = WALL0 + self.now.as_millis();
        for (n, node) in &mut self.nodes {
            node.core.on_tick(self.now, wall + self.skew.get(n).copied().unwrap_or(0));
        }
        self.settle();
    }

    pub fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.tick();
        }
    }

    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.core.status(G).is_some_and(|s| s.role == Role::Leader))
            .max_by_key(|(_, n)| n.core.status(G).unwrap().hard_state.term)
            .map(|(i, _)| *i)
    }

    /// Ticks until there is a leader whose term's first entry is applied.
    pub fn elect(&mut self) -> NodeId {
        for _ in 0..1_000 {
            self.tick();
            if let Some(l) = self.leader() {
                let s = self.nodes[&l].core.status(G).unwrap();
                if s.applied >= s.hard_state.commit && s.hard_state.commit > 0 {
                    return l;
                }
            }
        }
        panic!("no leader");
    }

    pub fn isolate(&mut self, n: NodeId) {
        for &o in self.nodes.keys() {
            if o != n {
                self.cut.insert((n, o));
                self.cut.insert((o, n));
            }
        }
    }

    pub fn heal(&mut self) {
        self.cut.clear();
    }

    pub fn propose(&mut self, n: NodeId, payload: &[u8]) -> Result<OpId, OpError> {
        let now = self.now;
        self.nodes.get_mut(&n).unwrap().core.propose(G, payload, now)
    }

    /// The results of `op` on node `n` (a test checks there is exactly one).
    pub fn results(&self, n: NodeId, op: OpId) -> Vec<Result<Done<u64>, OpError>> {
        self.nodes[&n].done.iter().filter(|(o, _)| *o == op).map(|(_, r)| r.clone()).collect()
    }

    pub fn sm(&self, n: NodeId) -> &Appender {
        self.nodes[&n].core.state_machine(G).unwrap()
    }

    pub fn others(&self, n: NodeId) -> Vec<NodeId> {
        self.nodes.keys().copied().filter(|&o| o != n).collect()
    }
}
