//! A hard state that only moves the commit index needs no sync of its own: Raft keeps the commit
//! index as volatile state (only the term, the vote and the log are persistent), so it goes to
//! disk with the next batch. Before, each write of a single-voter group took two syncs, one for
//! its entry and one for its commit, and the next write waited for the second: 7,1 ms an insert
//! with `w: "majority"` against 5,3 ms in a `refnode` 9.0.4 (H-008 of the native tests).

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Done, GroupDesc, MultiRaftCore, OpError, OpId, RestoreError,
    StateMachine,
};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(4);
const ME: NodeId = NodeId(1);
const TICK: Nanos = Nanos::from_millis(10);
/// Less than the 1 ms after which a batch without a strict request closes by itself.
const STEP: Nanos = Nanos::from_micros(100);
const WALL0: u64 = 1_800_000_000_000;
const WRITES: u64 = 20;

/// Counts the entries applied.
#[derive(Default)]
struct Counter {
    applied: u64,
    entries: u64,
}

impl StateMachine for Counter {
    type Output = u64;

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

struct Single {
    core: MultiRaftCore<Counter>,
    disk: MemDisk,
    now: Nanos,
    done: Vec<(OpId, Result<Done<u64>, OpError>)>,
}

impl Single {
    /// Opens the core over `disk` and its group, new or recovered from the disk.
    fn open(mut disk: MemDisk, now: Nanos) -> Single {
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let core = MultiRaftCore::new(CoreConfig::new(ME, 0), store, 7);
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

    /// Proposes one entry and steps a tenth of a millisecond at a time until it is answered, as a
    /// client that sends its next write once the last one is answered.
    fn write(&mut self, i: u64) {
        self.write_then(i, Nanos::ZERO);
    }

    /// [`Single::write`], and then `pause` with no write, as a range that gets one write in a
    /// while because the load is spread over many ranges.
    fn write_then(&mut self, i: u64, pause: Nanos) {
        let op = self.core.propose(G, &i.to_le_bytes(), self.now).unwrap();
        for _ in 0..1_000 {
            self.settle();
            if self.done.iter().any(|(o, r)| *o == op && r.is_ok()) {
                self.now = self.now + pause;
                self.settle();
                return;
            }
            self.now = self.now + STEP;
        }
        panic!("write {i} was never answered");
    }
}

#[test]
fn a_write_of_a_single_voter_takes_one_sync_and_survives_a_restart() {
    let mut s = Single::open(MemDisk::new(), Nanos::ZERO);
    s.write(0);
    let before = s.core.store().metrics().syncs;
    for i in 1..=WRITES {
        s.write(i);
    }
    let syncs = s.core.store().metrics().syncs - before;
    assert!(syncs <= WRITES + 1, "{syncs} syncs for {WRITES} writes");
    // The commit index on disk may lag: after a restart the entries, all of them synced, commit
    // again and the state machine has every write.
    let Single { disk, now, .. } = s;
    let mut s = Single::open(disk, now);
    for _ in 0..20 {
        s.tick();
    }
    assert!(!s.core.has_stopped_group());
    assert_eq!(s.core.state_machine(G).unwrap().entries, WRITES + 1);
}

#[test]
fn a_range_with_one_write_in_a_while_still_takes_one_sync_a_write() {
    // With the load spread over 16 ranges each core gets one write in a while: the commit of one
    // write waited 1 ms for the next and then went to disk alone, with its own sync (2,56 syncs
    // an insert, 9,21 ms, H-008 of the native tests). It goes with the next write instead.
    let mut s = Single::open(MemDisk::new(), Nanos::ZERO);
    s.write(0);
    let before = s.core.store().metrics().syncs;
    for i in 1..=WRITES {
        s.write_then(i, Nanos::from_millis(2));
    }
    let syncs = s.core.store().metrics().syncs - before;
    assert!(syncs <= WRITES + 1, "{syncs} syncs for {WRITES} writes");
    let Single { disk, now, .. } = s;
    let mut s = Single::open(disk, now);
    for _ in 0..20 {
        s.tick();
    }
    assert!(!s.core.has_stopped_group());
    assert_eq!(s.core.state_machine(G).unwrap().entries, WRITES + 1);
}
