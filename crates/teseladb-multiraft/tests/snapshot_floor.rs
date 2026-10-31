//! A state machine's snapshot floor (007: a range's data travels as it is when it is sent, so a
//! snapshot from before the range's last split would arrive without the part that split gave
//! away). The core takes a snapshot as soon as it applied past one below the floor, whatever
//! `snapshot_entries` says, and sends no snapshot below it: a replica that needs one waits for the
//! newer snapshot.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, GroupDesc, MultiRaftCore, PeerFrame, RestoreError,
    StateMachine,
};
use teseladb_raft::Role;
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(9);
const TICK: Nanos = Nanos::from_millis(10);
const WALL0: u64 = 1_800_000_000_000;
/// The payload that moves the floor to its entry, as a range's `Split` does.
const CUT: &[u8] = b"cut";
/// Ticks past a replica's boot, when it votes (the default `election_ticks_max`).
const BOOT_TICKS: u32 = 30;
const WAITING: [NodeId; 1] = [NodeId(1)];
/// Entries the leader applies while the follower is away: enough for several snapshots.
const AWAY_ENTRIES: u8 = 8;

/// Counts the entries applied; its floor is the last `CUT` it applied (the index of its snapshot
/// after a restore). It holds the log and declares the release floor the test sets, so that a
/// snapshot waits as a range's does until its memtable is flushed.
#[derive(Default)]
struct Cutter {
    applied: u64,
    entries: u64,
    cut: u64,
    release: Rc<Cell<Option<u64>>>,
    missing: Rc<Cell<bool>>,
    /// Waits for a snapshot before taking entries (a replica added to a range born from a split).
    needs: Rc<Cell<bool>>,
    /// Its state runs this many entries ahead of what it applied, and so does its floor (a range
    /// restarted over an LSM that flushed entries its commit index does not cover yet).
    ahead: Rc<Cell<u64>>,
}

impl StateMachine for Cutter {
    type Output = u64;
    const HOLDS_LOG: bool = true;

    fn release_floor(&self) -> Option<u64> {
        self.release.get()
    }

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<u64, ApplyError> {
        self.applied = ctx.index;
        self.entries += 1;
        if payload == CUT {
            self.cut = ctx.index;
        }
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
        self.cut = self.applied;
        self.needs.set(false);
        Ok(())
    }

    fn has_data_for(&self, _: &[u8]) -> bool {
        !self.missing.get()
    }

    fn snapshot_floor(&self) -> u64 {
        match self.ahead.get() {
            0 => self.cut,
            ahead => self.cut.max(self.applied + ahead),
        }
    }

    fn snapshot_state_index(&self) -> u64 {
        self.applied + self.ahead.get()
    }

    fn needs_snapshot(&self) -> bool {
        self.needs.get()
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}

struct Node {
    core: MultiRaftCore<Cutter>,
    disk: MemDisk,
    release: Rc<Cell<Option<u64>>>,
    missing: Rc<Cell<bool>>,
    ahead: Rc<Cell<u64>>,
    data: Vec<(GroupId, u64)>,
}

/// Three voters over `MemDisk`s; the test drives time, the disks and the network.
struct Cluster {
    nodes: BTreeMap<NodeId, Node>,
    now: Nanos,
    cut: BTreeSet<NodeId>,
    /// Snapshot pieces sent: `(to, index)`.
    chunks: Vec<(NodeId, u64)>,
}

impl Cluster {
    fn new(snapshot_entries: u64) -> Cluster {
        Cluster::waiting(snapshot_entries, &[])
    }

    /// The cluster with the replicas of `waiting` opened for a snapshot they have not received.
    fn waiting(snapshot_entries: u64, waiting: &[NodeId]) -> Cluster {
        let mut c = Cluster {
            nodes: BTreeMap::new(),
            now: Nanos::ZERO,
            cut: BTreeSet::new(),
            chunks: Vec::new(),
        };
        let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
        for i in 1..=3 {
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let cfg = CoreConfig {
                snapshot_entries,
                instance: i * 1_000,
                ..CoreConfig::new(NodeId(i), 0)
            };
            let mut core = MultiRaftCore::new(cfg, store, i);
            let (release, missing) = (Rc::new(Cell::new(None)), Rc::new(Cell::new(false)));
            let needs = Rc::new(Cell::new(waiting.contains(&NodeId(i))));
            let ahead = Rc::new(Cell::new(0));
            let sm = Cutter {
                release: release.clone(),
                missing: missing.clone(),
                needs,
                ahead: ahead.clone(),
                ..Cutter::default()
            };
            let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
            core.create_group(desc, sm).unwrap();
            let node = Node { core, disk, release, missing, ahead, data: Vec::new() };
            c.nodes.insert(NodeId(i), node);
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
                node.data.extend(a.snapshot_data);
                for (to, f) in a.send {
                    queue.push_back((n, to, f));
                    moved = true;
                }
            }
            while let Some((from, to, f)) = queue.pop_front() {
                if self.cut.contains(&from) || self.cut.contains(&to) {
                    continue;
                }
                if let PeerFrame::SnapshotChunk(c) = &f {
                    self.chunks.push((to, c.index));
                }
                self.nodes.get_mut(&to).unwrap().core.on_peer_message(from, f, self.now);
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

    fn elect(&mut self) -> NodeId {
        for _ in 0..1_000 {
            self.ticks(1);
            let leader = self
                .nodes
                .iter()
                .filter(|(_, n)| n.core.status(G).is_some_and(|s| s.role == Role::Leader))
                .map(|(i, _)| *i)
                .next();
            if let Some(l) = leader {
                let s = self.nodes[&l].core.status(G).unwrap();
                if s.applied >= s.hard_state.commit && s.hard_state.commit > 0 {
                    return l;
                }
            }
        }
        panic!("no leader");
    }

    fn propose(&mut self, n: NodeId, payload: &[u8]) {
        let now = self.now;
        self.nodes.get_mut(&n).unwrap().core.propose(G, payload, now).unwrap();
        self.ticks(1);
    }

    fn applied(&self, n: NodeId) -> u64 {
        self.nodes[&n].core.status(G).unwrap().applied
    }
}

#[test]
fn every_replica_takes_a_snapshot_as_soon_as_it_applies_past_its_floor() {
    let mut c = Cluster::new(1_000);
    let l = c.elect();
    c.propose(l, b"a");
    c.ticks(3);
    assert!(c.nodes.values().all(|n| n.core.snapshot_index(G) == Some(0)), "none due yet");
    c.propose(l, CUT);
    c.ticks(3);
    let at = c.applied(l);
    for (id, n) in &c.nodes {
        assert_eq!(n.core.snapshot_index(G), Some(at), "{id:?}");
        assert_eq!(n.core.store().group(G).unwrap().snapshot_index, at, "{id:?}: saved");
    }
    // Past the floor, the next one waits for `snapshot_entries` again.
    c.propose(l, b"b");
    c.ticks(3);
    assert!(c.nodes.values().all(|n| n.core.snapshot_index(G) == Some(at)));
}

#[test]
fn a_replica_that_needs_a_snapshot_waits_for_one_past_the_floor() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    let held = c.applied(f);
    // The follower is away while the leader compacts its log past what the follower has.
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    let compacted = c.nodes[&l].core.store().group(G).unwrap().snapshot_index;
    assert!(compacted > held, "the leader compacted past the follower");
    // The leader's memtable holds the cut: its snapshot past the floor waits, taken.
    let before = c.applied(l);
    c.nodes[&l].release.set(Some(before));
    c.propose(l, CUT);
    c.propose(l, b"after");
    let floor = before + 1;
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() < floor, "not saved yet");
    c.cut.clear();
    c.ticks(100);
    assert!(c.chunks.is_empty(), "nothing below the floor was sent: {:?}", c.chunks);
    assert!(c.applied(f) < floor, "the follower waits");
    // The memtable is flushed: the snapshot past the floor is saved and goes. Its data is not on
    // the follower yet, which parks it and asks for it.
    c.nodes[&l].release.set(None);
    c.nodes[&f].missing.set(true);
    c.ticks(100);
    assert!(
        !c.chunks.is_empty() && c.chunks.iter().all(|(to, i)| *to == f && *i >= floor),
        "{:?} to {f:?}, floor {floor}",
        c.chunks
    );
    let (g, index) = *c.nodes[&f].data.last().expect("the follower asks for the data");
    assert!(g == G && index >= floor);
    let state = c.nodes[&f].core.awaited_snapshot(G).expect("parked").to_vec();
    assert_eq!(state[..8], index.to_le_bytes());
    assert_eq!(c.nodes[&f].core.awaited_snapshot(GroupId(99)), None);
    c.nodes.get_mut(&f).unwrap().missing.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(20);
    assert_eq!(c.nodes[&f].core.awaited_snapshot(G), None);
    assert_eq!(c.applied(f), c.applied(l), "the follower caught up");
}

#[test]
fn a_newer_snapshot_replaces_one_whose_data_never_comes() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    // The follower parks the leader's snapshot: its data is not here, and no replica will serve
    // it once the range is cut past it.
    c.nodes[&f].missing.set(true);
    c.cut.clear();
    c.ticks(5);
    let first = c.nodes[&f].data.last().expect("the follower asks for the data").1;
    c.propose(l, CUT);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    let floor = c.nodes[&l].core.state_machine(G).unwrap().snapshot_floor();
    assert!(floor > first);
    // The leader gives the first transfer up and sends its newer snapshot, which replaces the
    // parked one.
    c.ticks(400);
    let (g, newer) = *c.nodes[&f].data.last().unwrap();
    assert!(g == G && newer >= floor, "{newer} < {floor}");
    let state = c.nodes[&f].core.awaited_snapshot(G).expect("parked").to_vec();
    assert_eq!(state[..8], newer.to_le_bytes());
    c.nodes.get_mut(&f).unwrap().missing.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(20);
    assert_eq!(c.applied(f), c.applied(l), "the follower caught up");
    // Its vote and term were kept: it still takes part in elections.
    let term = c.nodes[&f].core.status(G).unwrap().hard_state.term;
    assert_eq!(term, c.nodes[&l].core.status(G).unwrap().hard_state.term);
}

#[test]
fn a_follower_told_its_snapshot_is_stale_gets_a_newer_one_at_once() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    c.nodes[&f].missing.set(true);
    c.cut.clear();
    c.ticks(5);
    let first = c.nodes[&f].data.last().expect("the follower parks it").1;
    c.propose(l, CUT);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    let floor = c.nodes[&l].core.state_machine(G).unwrap().snapshot_floor();
    // Nothing to reject on a group it does not have, or one that waits for nothing.
    let other = *c.nodes.keys().find(|&&n| n != l && n != f).unwrap();
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(GroupId(99));
    c.nodes.get_mut(&other).unwrap().core.reject_awaited(G);
    c.ticks(1);
    assert_eq!(c.nodes[&f].data.last().unwrap().1, first);
    // Well before the leader would give the transfer up, the follower's rejection brings the
    // newer snapshot.
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.ticks(5);
    let newer = c.nodes[&f].data.last().unwrap().1;
    assert!(newer >= floor && first < floor, "{first} → {newer}, floor {floor}");
}

#[test]
fn a_replica_that_needs_a_snapshot_takes_no_entries_until_it_has_one() {
    let mut c = Cluster::new(1_000);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    c.propose(l, b"a");
    // The follower lost what it had: it waits for a snapshot, and refuses the entries meanwhile.
    let needs = Rc::new(Cell::new(true));
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.remove_group(G).unwrap();
    let sm = Cutter { needs: needs.clone(), ..Cutter::default() };
    let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
    node.core.create_group(GroupDesc { group: G, voters, learners: vec![] }, sm).unwrap();
    c.cut.clear();
    c.ticks(20);
    // Its refusal makes the leader, whose log was never compacted, take a snapshot and send it:
    // the follower is caught up by the snapshot, not by the entries.
    assert!(!needs.get(), "a snapshot was installed");
    let entries = |n: NodeId| c.nodes[&n].core.state_machine(G).unwrap().entries;
    assert_eq!(entries(f), entries(l));
    assert_eq!(c.applied(f), c.applied(l));
    c.propose(l, CUT);
    c.ticks(20);
    assert_eq!(c.applied(f), c.applied(l));
}

#[test]
fn a_group_removed_forgets_the_snapshots_it_refused() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    let before = c.applied(l);
    c.nodes[&l].release.set(Some(before));
    c.propose(l, CUT);
    c.cut.clear();
    c.ticks(20);
    assert!(c.chunks.is_empty(), "refused below the floor");
    // The group goes: nothing is sent for it afterwards.
    c.nodes.get_mut(&l).unwrap().core.remove_group(G).unwrap();
    c.nodes[&l].release.set(None);
    c.ticks(20);
    let floor = before + 1;
    assert!(c.chunks.iter().all(|(_, i)| *i >= floor), "{:?}", c.chunks);
}

#[test]
fn a_replica_that_needs_a_snapshot_never_leads() {
    // Every replica is new and node 1 waits for a snapshot: its LSM lacks the range's data (it
    // never applied the parent's cut), so as a leader it would serve what an older life of the
    // range left on its disk. The three pass their boot apart, then only node 1's clock moves:
    // its election timeout comes first, and the others would vote for it.
    let mut c = Cluster::waiting(1_000, &WAITING);
    c.cut.extend(c.nodes.keys().copied().collect::<Vec<_>>());
    c.ticks(BOOT_TICKS);
    c.cut.clear();
    let waiter = WAITING[0];
    for _ in 0..BOOT_TICKS {
        c.now = c.now + TICK;
        let now = c.now;
        c.nodes.get_mut(&waiter).unwrap().core.on_tick(now, WALL0 + now.as_millis());
        c.settle();
        let st = c.nodes[&waiter].core.status(G).unwrap();
        assert_ne!(st.role, Role::Leader, "a replica without its data leads");
    }
    // One of the others leads.
    assert_ne!(c.elect(), waiter);
}

#[test]
fn a_replica_parked_on_a_snapshot_whose_data_never_comes_can_be_closed_and_vote_again() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    // The follower parks the leader's snapshot: its data is not here. A parked replica answers
    // nothing but a newer snapshot, so it does not vote.
    c.nodes[&f].missing.set(true);
    c.cut.clear();
    c.ticks(5);
    assert!(c.nodes[&f].core.awaited_snapshot(G).is_some());
    // The leader goes for good: no replica will send a newer snapshot.
    c.cut.insert(l);
    // Nothing of the parked snapshot is on disk: the group closes, and opens again from its log.
    let node = c.nodes.get_mut(&f).unwrap();
    node.core.close_group(G).unwrap();
    let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
    let sm = Cutter { release: node.release.clone(), ..Cutter::default() };
    node.core.create_group(GroupDesc { group: G, voters, learners: vec![] }, sm).unwrap();
    assert_eq!(node.core.awaited_snapshot(G), None);
    // With the third replica it elects a leader again.
    let led = |c: &Cluster| {
        c.nodes
            .iter()
            .any(|(id, n)| *id != l && n.core.status(G).is_some_and(|s| s.role == Role::Leader))
    };
    for _ in 0..1_000 {
        if led(&c) {
            break;
        }
        c.ticks(1);
    }
    assert!(led(&c), "no leader without the one cut off");
}

#[test]
fn a_snapshot_taken_while_the_state_runs_ahead_of_the_log_is_sent_past_the_floor() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    // The leader's state holds entries its log has not applied: its floor stays ahead of every
    // entry it applies, and each snapshot it takes holds the state up to that floor.
    c.nodes[&l].ahead.set(3);
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    let floor = c.nodes[&l].core.state_machine(G).unwrap().snapshot_floor();
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() < floor);
    c.cut.clear();
    c.ticks(10);
    assert!(
        c.chunks.iter().any(|(to, _)| *to == f),
        "the snapshot holds the state past the floor: it is sent"
    );
}

#[test]
fn a_leader_without_a_snapshot_takes_one_for_a_replica_that_needs_it() {
    // The log is never compacted by count: only the replica's need makes the leader take one.
    let mut c = Cluster::waiting(u64::MAX, &WAITING);
    let l = c.elect();
    assert_ne!(l, WAITING[0], "a replica that needs a snapshot does not lead");
    c.propose(l, b"x");
    c.ticks(20);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() > 0, "the leader took a snapshot");
    assert!(c.chunks.iter().any(|(to, _)| *to == WAITING[0]), "and sent it");
}
