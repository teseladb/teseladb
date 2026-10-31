//! A state machine that relies on the defaults of `StateMachine`: it holds the log
//! (`HOLDS_LOG`) but needs nothing back from it, and its snapshots' data is always on the node.

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Done, GroupDesc, MultiRaftCore, RestoreError, StateMachine,
};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, ReleaseProvider, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(4);
const ME: NodeId = NodeId(1);
const TICK: Nanos = Nanos::from_millis(10);
const WALL0: u64 = 1_800_000_000_000;
/// Entries applied past the latest snapshot that trigger a new one.
const SNAPSHOT_ENTRIES: u64 = 3;

/// Counts the entries applied; its snapshot is the count.
#[derive(Default)]
struct Counter {
    applied: u64,
    entries: u64,
}

impl StateMachine for Counter {
    type Output = u64;
    const HOLDS_LOG: bool = true;

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

/// One core with a single-voter group over a `MemDisk`, driven by hand.
struct Single {
    core: MultiRaftCore<Counter>,
    disk: MemDisk,
    now: Nanos,
    done: Vec<(teseladb_multiraft::OpId, Result<Done<u64>, teseladb_multiraft::OpError>)>,
}

impl Single {
    /// Opens the core over `disk` at monotonic time `now` (a restart goes on from the time of the
    /// previous life: the wall clock never runs backwards).
    fn open(mut disk: MemDisk, seed: u64, now: Nanos) -> Single {
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let cfg = CoreConfig { snapshot_entries: SNAPSHOT_ENTRIES, ..CoreConfig::new(ME, 0) };
        let core = MultiRaftCore::new(cfg, store, seed);
        let mut s = Single { core, disk, now, done: vec![] };
        let desc = GroupDesc { group: G, voters: vec![ME], learners: vec![] };
        s.core.create_group(desc, Counter::default()).unwrap();
        s.tick();
        s
    }

    fn settle(&mut self) {
        while let Some(op) = self.core.poll_io(self.now) {
            let d = self.disk.execute(op);
            self.core.on_io_done(d, self.now);
        }
        self.done.extend(self.core.drain_actions().done);
    }

    fn tick(&mut self) {
        self.now = self.now + TICK;
        self.core.on_tick(self.now, WALL0 + self.now.as_millis());
        self.settle();
    }
}

#[test]
fn a_state_machine_that_needs_nothing_back_clears_the_floor_and_restores_its_snapshot() {
    let mut s = Single::open(MemDisk::new(), 1, Nanos::ZERO);
    // A floor left by hand for the LSM is replaced by what the state machine declares: nothing.
    s.core.set_release_floor(G, ReleaseProvider::Lsm, Some(1));
    assert_eq!(s.core.store().release_floor_of(G), Some(1));
    s.tick();
    assert_eq!(s.core.store().release_floor_of(G), None);
    for i in 0..8u8 {
        let op = s.core.propose(G, &[i], s.now).unwrap();
        s.tick();
        assert!(s.done.iter().any(|(o, r)| *o == op && r == &Ok(Done::Applied(u64::from(i) + 1))));
    }
    let snap = s.core.store().group(G).unwrap().snapshot_index;
    assert!(snap > 0, "the periodic snapshots are not held back");
    // After a restart the snapshot's data is taken as present and it is restored.
    let Single { disk, now, .. } = s;
    let mut s = Single::open(disk, 2, now);
    for _ in 0..5 {
        s.tick();
    }
    assert!(!s.core.has_stopped_group());
    let sm = s.core.state_machine(G).unwrap();
    assert_eq!(sm.entries, 8);
    let op = s.core.propose(G, b"z", s.now).unwrap();
    s.tick();
    assert!(s.done.iter().any(|(o, r)| *o == op && r == &Ok(Done::Applied(9))));
}
