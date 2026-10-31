//! A snapshot from the leader installed on a replica whose state machine holds the log (005: a
//! range over its core's LSM) and already has the snapshot's data. After a restart a range applies
//! again the entries its LSM had flushed, so its memtables hold entries below what is durable and
//! the floor it declares can be far below the snapshot. The install does not depend on that floor:
//! the data of the whole snapshot is on the node.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Done, GroupDesc, MultiRaftCore, OpError, OpId, PeerFrame,
    RestoreError, StateMachine,
};
use teseladb_raft::Role;
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(9);
const TICK: Nanos = Nanos::from_millis(10);
const WALL0: u64 = 1_800_000_000_000;
/// Entries applied past the latest snapshot that trigger a new one.
const SNAPSHOT_ENTRIES: u64 = 3;
/// Entries the leader applies while the follower is away: enough for several snapshots.
const AWAY_ENTRIES: u8 = 8;

/// Counts the entries applied. It holds the log and declares the floor the test sets, as a range
/// does whose memtables hold entries again after a restart; its snapshots' data is on the node.
#[derive(Default)]
struct Held {
    applied: u64,
    entries: u64,
    floor: Rc<Cell<Option<u64>>>,
}

impl StateMachine for Held {
    type Output = u64;
    const HOLDS_LOG: bool = true;

    fn release_floor(&self) -> Option<u64> {
        self.floor.get()
    }

    fn apply(&mut self, _: &[u8], ctx: &ApplyCtx) -> Result<u64, ApplyError> {
        self.applied = ctx.index;
        self.entries += 1;
        Ok(self.entries)
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        [self.applied.to_le_bytes(), self.entries.to_le_bytes()].concat()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        let word = |i: usize| {
            snap.get(i * 8..i * 8 + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| RestoreError("short snapshot".into()))
        };
        (self.applied, self.entries) = (word(0)?, word(1)?);
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}

struct Node {
    core: MultiRaftCore<Held>,
    disk: MemDisk,
    done: Vec<(OpId, Result<Done<u64>, OpError>)>,
    floor: Rc<Cell<Option<u64>>>,
}

/// Three voters over `MemDisk`s; the test drives time, the disks and the network.
struct Cluster {
    nodes: BTreeMap<NodeId, Node>,
    now: Nanos,
    cut: BTreeSet<NodeId>,
}

impl Cluster {
    fn new() -> Cluster {
        let mut c = Cluster { nodes: BTreeMap::new(), now: Nanos::ZERO, cut: BTreeSet::new() };
        let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
        for i in 1..=3 {
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let cfg = CoreConfig {
                snapshot_entries: SNAPSHOT_ENTRIES,
                instance: i * 1_000,
                ..CoreConfig::new(NodeId(i), 0)
            };
            let mut core = MultiRaftCore::new(cfg, store, i);
            let floor = Rc::new(Cell::new(None));
            let sm = Held { floor: floor.clone(), ..Held::default() };
            let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
            core.create_group(desc, sm).unwrap();
            c.nodes.insert(NodeId(i), Node { core, disk, done: vec![], floor });
        }
        c.settle();
        c
    }

    fn settle(&mut self) {
        let mut queue: VecDeque<(NodeId, NodeId, PeerFrame)> = VecDeque::new();
        for _ in 0..100_000 {
            let mut moved = false;
            for (&n, node) in &mut self.nodes {
                while let Some(op) = node.core.poll_io(self.now) {
                    let d = node.disk.execute(op);
                    node.core.on_io_done(d, self.now);
                    moved = true;
                }
                let a = node.core.drain_actions();
                node.done.extend(a.done);
                for (to, f) in a.send {
                    queue.push_back((n, to, f));
                    moved = true;
                }
            }
            while let Some((from, to, f)) = queue.pop_front() {
                if !self.cut.contains(&from) && !self.cut.contains(&to) {
                    self.nodes.get_mut(&to).unwrap().core.on_peer_message(from, f, self.now);
                }
            }
            if !moved {
                return;
            }
        }
        panic!("the cluster never settles");
    }

    fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.now = self.now + TICK;
            for node in self.nodes.values_mut() {
                node.core.on_tick(self.now, WALL0 + self.now.as_millis());
            }
            self.settle();
        }
    }

    /// The leader among the nodes that are not cut off.
    fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(i, _)| !self.cut.contains(i))
            .filter(|(_, n)| n.core.status(G).is_some_and(|s| s.role == Role::Leader))
            .max_by_key(|(_, n)| n.core.status(G).unwrap().hard_state.term)
            .map(|(i, _)| *i)
    }

    fn elect(&mut self) -> NodeId {
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

    fn propose(&mut self, n: NodeId, payload: &[u8]) -> OpId {
        let now = self.now;
        self.nodes.get_mut(&n).unwrap().core.propose(G, payload, now).unwrap()
    }

    fn applied(&self, n: NodeId, op: OpId) -> bool {
        self.nodes[&n].done.iter().any(|(o, r)| *o == op && matches!(r, Ok(Done::Applied(_))))
    }
}

#[test]
fn a_snapshot_whose_data_is_here_installs_below_the_declared_floor() {
    let mut c = Cluster::new();
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    let held = c.nodes[&f].core.status(G).unwrap().applied;
    // The follower is away while the leader compacts its log past what the follower has.
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
        c.ticks(1);
    }
    c.ticks(5);
    let compacted = c.nodes[&l].core.store().group(G).unwrap().snapshot_index;
    assert!(compacted > held, "the leader compacted past the follower");
    // Its state machine still holds in memory entries it already had, as a range does after a
    // restart: the floor it declares is below the snapshot that comes.
    c.nodes[&f].floor.set(Some(held.saturating_sub(1)));
    c.cut.clear();
    c.ticks(100);
    let node = &c.nodes[&f];
    assert!(!node.core.has_stopped_group(), "the install does not stop the group");
    let snap = node.core.store().group(G).unwrap().snapshot_index;
    assert!(snap >= compacted, "the snapshot is saved: {snap} < {compacted}");
    let leader = c.nodes[&l].core.status(G).unwrap().applied;
    assert_eq!(node.core.status(G).unwrap().applied, leader, "the follower caught up");
    // With the leader away the follower's vote and log still make a majority.
    let other = *c.nodes.keys().find(|&&n| n != l && n != f).unwrap();
    c.cut.insert(l);
    let leader = c.elect();
    assert!(leader == f || leader == other);
    let op = c.propose(leader, b"after");
    c.ticks(5);
    assert!(c.applied(leader, op), "a write goes through without the old leader");
}
