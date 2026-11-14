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
    /// Data that entered without the log (a restore's ingest), and the index it entered at.
    bulk: Rc<Cell<(u64, u64)>>,
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
        let bulk = self.bulk.get().0;
        [self.applied.to_le_bytes(), self.entries.to_le_bytes(), bulk.to_le_bytes()].concat()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        let word = |i: usize| {
            snap.get(i * 8..i * 8 + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| RestoreError("short snapshot".into()))
        };
        (self.applied, self.entries) = (word(0)?, word(1)?);
        self.bulk.set((word(2)?, self.applied));
        self.cut = self.applied;
        self.needs.set(false);
        Ok(())
    }

    fn has_data_for(&self, _: &[u8]) -> bool {
        !self.missing.get()
    }

    fn snapshot_floor(&self) -> u64 {
        let floor = match self.ahead.get() {
            0 => self.cut,
            ahead => self.cut.max(self.applied + ahead),
        };
        floor.max(self.log_floor())
    }

    fn log_floor(&self) -> u64 {
        self.bulk.get().1
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
    bulk: Rc<Cell<(u64, u64)>>,
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
            let bulk = Rc::new(Cell::new((0, 0)));
            let sm = Cutter {
                release: release.clone(),
                missing: missing.clone(),
                needs,
                ahead: ahead.clone(),
                bulk: bulk.clone(),
                ..Cutter::default()
            };
            let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
            core.create_group(desc, sm).unwrap();
            let node = Node { core, disk, release, missing, ahead, bulk, data: Vec::new() };
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
fn a_replica_whose_parked_snapshot_nobody_serves_keeps_it_and_lets_the_leader_commit() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    let third = *c.nodes.keys().find(|&&n| n != l && n != f).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    // The follower parks the leader's snapshot, whose data is not here, and the third replica goes
    // for good: the leader commits nothing more without the follower.
    c.nodes[&f].missing.set(true);
    c.cut.clear();
    c.cut.insert(third);
    c.ticks(5);
    let parked = c.nodes[&f].core.awaited_snapshot(G).expect("parked").to_vec();
    let commit = |c: &Cluster, n: NodeId| c.nodes[&n].core.status(G).unwrap().hard_state.commit;
    let before = commit(&c, l);
    c.propose(l, b"w");
    c.ticks(20);
    assert_eq!(commit(&c, l), before, "a parked replica answers nothing");
    // No replica will serve its data (the range split past it, or its data would land over
    // another range of the node): the replica keeps the snapshot as its own without its data,
    // and votes, appends and commits again, so that the leader commits past it (multirange seed
    // 83, D-067).
    let applied = c.nodes[&f].core.state_machine(G).unwrap().entries;
    c.nodes.get_mut(&f).unwrap().core.reject_awaited(G);
    c.ticks(20);
    assert!(commit(&c, l) > before, "the leader commits with the follower's vote");
    assert_eq!(commit(&c, f), commit(&c, l));
    assert_eq!(c.nodes[&f].core.awaited_snapshot(G).map(<[u8]>::to_vec), Some(parked));
    assert_eq!(c.nodes[&f].core.state_machine(G).unwrap().entries, applied, "nothing applied");
    assert_ne!(c.nodes[&f].core.status(G).unwrap().role, Role::Leader);
    // It serves no read at a closed mark while its state machine is behind its log.
    assert_eq!(c.nodes[&f].core.closed(G), None);
    // Asked by the leader to take the leadership, it does not stand: a candidate without the
    // data would lead with a state behind what it committed.
    let leads = |c: &Cluster, n: NodeId| c.nodes[&n].core.status(G).unwrap().role == Role::Leader;
    c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, f);
    c.ticks(50);
    assert!(!leads(&c, f), "a replica without its snapshot's data takes no leadership");
    // Nor with the leader gone, though its log is the newest of the two left.
    c.cut.clear();
    c.cut.insert(l);
    c.ticks(300);
    assert!(!leads(&c, f), "a replica without its snapshot's data does not campaign");
    c.cut.clear();
    c.ticks(50);
    // Once its data is here it applies what it held back.
    c.nodes[&f].missing.set(false);
    let now = c.now;
    c.nodes.get_mut(&f).unwrap().core.snapshot_data_ready(G, now);
    c.ticks(20);
    assert_eq!(c.nodes[&f].core.awaited_snapshot(G), None);
    assert_eq!(c.applied(f), c.applied(l));
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

#[test]
fn an_own_snapshot_waiting_for_the_floor_goes_once_a_newer_one_is_installed() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    // The follower's memtable holds the cut: its own snapshot past the floor waits, taken.
    let held = c.applied(f);
    c.nodes[&f].release.set(Some(held));
    c.propose(l, CUT);
    c.ticks(3);
    let own = c.applied(f);
    assert!(c.nodes[&f].core.snapshot_index(G).unwrap() < own, "waiting for the floor");
    // Away, it misses the leader's next snapshots; back, it installs one past its own.
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.propose(l, CUT);
    c.ticks(5);
    c.cut.clear();
    c.ticks(50);
    assert_eq!(c.applied(f), c.applied(l), "caught up");
    let installed = c.nodes[&f].core.snapshot_index(G).unwrap();
    assert!(installed > own, "{installed} past {own}");
    // Its own one, older, is dropped rather than saved over it.
    c.nodes[&f].release.set(None);
    c.ticks(3);
    assert!(c.nodes[&f].core.snapshot_index(G).unwrap() >= installed);
}

/// Lane 4's measure (018): a replica away while a restore ingested data on the others, and whose
/// log the leader still holds from where it stopped, comes back with that data. Replaying the
/// entries would leave it without it: the leader sends its snapshot past the ingest.
#[test]
fn a_replica_behind_data_that_entered_without_the_log_receives_a_snapshot() {
    let mut c = Cluster::new(1_000);
    let l = c.elect();
    let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
    c.cut.insert(f);
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(3);
    // The leader's memtable holds its last entries: the snapshot past the ingest waits, and the
    // log keeps every entry the follower lacks.
    c.nodes[&l].release.set(Some(c.applied(l)));
    c.propose(l, b"held");
    let floor = c.applied(l);
    for (&id, n) in &mut c.nodes {
        if id != f {
            let applied = n.core.status(G).unwrap().applied;
            assert!(n.core.data_entered(G, |sm| sm.bulk.set((100, applied))), "{id:?}");
        }
    }
    assert!(!c.nodes.get_mut(&l).unwrap().core.data_entered(GroupId(99), |_| ()));
    c.ticks(3);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() < floor, "not saved yet");
    c.cut.clear();
    c.ticks(50);
    assert!(c.applied(f) < floor, "the follower takes no entry up to the ingest");
    c.nodes[&l].release.set(None);
    c.ticks(50);
    assert_eq!(c.applied(f), c.applied(l), "caught up");
    assert_eq!(c.nodes[&f].bulk.get().0, 100, "with the data the log does not have");
    assert!(c.chunks.iter().all(|(to, i)| *to == f && *i >= floor), "{:?}", c.chunks);
}

#[test]
fn a_snapshot_a_replica_waits_for_asks_for_the_flush_the_floor_waits_on() {
    // The leader's memtable holds every entry (its LSM never flushed): the snapshot it takes for
    // the replica that needs one waits for the floor, and the layer above learns that a flush is
    // owed, however busy the core is (seed 2361 of `search`: a range born from a split whose
    // replica never applied the cut, on a core that applied something every second).
    let mut c = Cluster::waiting(u64::MAX, &WAITING);
    let l = c.elect();
    c.nodes[&l].release.set(Some(0));
    c.propose(l, b"x");
    c.ticks(20);
    assert_eq!(c.nodes[&l].core.snapshot_index(G), Some(0), "the snapshot waits");
    assert!(c.chunks.is_empty(), "nothing goes meanwhile");
    assert!(c.nodes[&l].core.snapshot_awaits_flush(), "the leader owes a flush");
    assert!(c.nodes.iter().filter(|(n, _)| **n != l).all(|(_, n)| !n.core.snapshot_awaits_flush()));
    // The flush: the snapshot is saved and sent, and nothing more is owed.
    c.nodes[&l].release.set(None);
    c.ticks(20);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() > 0);
    assert!(c.chunks.iter().any(|(to, _)| *to == WAITING[0]), "the snapshot went");
    assert!(!c.nodes[&l].core.snapshot_awaits_flush());
}

#[test]
fn a_snapshot_asked_for_now_owes_the_flush_the_floor_waits_on() {
    // A range born from a split takes its snapshot at once, and its leader carries out neither a
    // split nor a replica change until it is saved: held by the floor, it owes a flush however
    // busy the core is. Before, only the snapshot a replica asked for owed one, and on a core
    // whose ranges applied something every few hundred milliseconds (a loaded machine) the
    // memtable never went down and the next split of that range never came (`admin_port`'s
    // `range_leaders_publish_themselves_and_spread_over_the_nodes`).
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.nodes[&l].release.set(Some(0));
    c.propose(l, b"x");
    c.ticks(5);
    assert!(!c.nodes[&l].core.snapshot_awaits_flush(), "nothing asked for yet");
    assert!(c.nodes.get_mut(&l).unwrap().core.snapshot_now(G));
    assert_eq!(c.nodes[&l].core.snapshot_index(G), Some(0), "the snapshot waits");
    assert!(c.nodes[&l].core.snapshot_awaits_flush(), "the leader owes a flush");
    // The flush: the snapshot is saved, and nothing more is owed.
    c.nodes[&l].release.set(None);
    c.ticks(5);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() > 0);
    assert!(!c.nodes[&l].core.snapshot_awaits_flush());
}

#[test]
fn a_snapshot_asked_for_while_one_waits_owes_the_flush_for_that_one() {
    // A split's cut raises the state machine's floor, so the core takes a snapshot on its own;
    // held by the release floor, nobody waits for it yet. The leader that then asks for one gets
    // that one (no other is taken), and the flush is owed for it: before, `snapshot_now` answered
    // «one is waiting» and nothing asked for the flush, so the range never split again on a busy
    // core.
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.nodes[&l].release.set(Some(0));
    c.propose(l, CUT);
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.snapshot_index(G), Some(0), "the cut's snapshot waits");
    assert!(!c.nodes[&l].core.snapshot_awaits_flush(), "nobody asked for it yet");
    assert!(!c.nodes.get_mut(&l).unwrap().core.snapshot_now(G), "the waiting one is the one");
    assert!(c.nodes[&l].core.snapshot_awaits_flush(), "the leader owes a flush");
    c.nodes[&l].release.set(None);
    c.ticks(5);
    assert!(c.nodes[&l].core.snapshot_index(G).unwrap() > 0);
    assert!(!c.nodes[&l].core.snapshot_awaits_flush());
}

#[test]
fn a_newer_snapshot_of_a_group_waited_for_owes_another_flush() {
    // The flush made for the snapshot waited for may not reach the newer one a cut takes in its
    // place: the mark changes, and the layer above flushes again.
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.nodes[&l].release.set(Some(0));
    c.propose(l, b"x");
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.snapshot_flush_mark(), None);
    assert!(c.nodes.get_mut(&l).unwrap().core.snapshot_now(G));
    let first = c.nodes[&l].core.snapshot_flush_mark();
    assert!(first.is_some());
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.snapshot_flush_mark(), first, "the same wait");
    c.propose(l, CUT);
    c.ticks(5);
    let second = c.nodes[&l].core.snapshot_flush_mark();
    assert!(second.is_some() && second != first, "{first:?} {second:?}");
    c.nodes[&l].release.set(None);
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.snapshot_flush_mark(), None);
}

#[test]
fn a_periodic_snapshot_held_by_the_floor_owes_no_flush() {
    // Under writes the floor stays below the snapshot just taken: nobody waits for it, so no
    // flush is asked for (the memtable goes down when it fills).
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.nodes[&l].release.set(Some(1));
    for i in 0..AWAY_ENTRIES {
        c.propose(l, &[i]);
    }
    c.ticks(5);
    assert_eq!(c.nodes[&l].core.store().group(G).unwrap().snapshot_index, 0, "held");
    assert!(c.nodes.values().all(|n| !n.core.snapshot_awaits_flush()));
}
