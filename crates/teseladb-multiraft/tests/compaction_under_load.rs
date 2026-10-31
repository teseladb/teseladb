//! A state machine that holds the log (005: a range over its core's LSM) under continuous writes:
//! the entry it just applied is always in its memtable, so its release floor is always below it.
//! The snapshot taken at that entry waits until the floor reaches it (a flush), and the log is
//! compacted while the writes go on.

use teseladb_multiraft::{
    ApplyCtx, ApplyError, CoreConfig, Done, GroupDesc, MultiRaftCore, OpError, OpId, RestoreError,
    StateMachine,
};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(3);
const ME: NodeId = NodeId(1);
const TICK: Nanos = Nanos::from_millis(10);
const WALL0: u64 = 1_800_000_000_000;
/// Entries applied past the latest snapshot that trigger a new one.
const SNAPSHOT_ENTRIES: u64 = 4;
/// Entries a memtable holds before it is flushed.
const MEMTABLE_ENTRIES: u64 = 3;
/// Writes of the test, one per tick.
const WRITES: u64 = 40;

/// Counts entries. Its "memtable" holds the entries since the last flush, which happens when a
/// write finds it full: the entry just applied is never flushed, as under continuous writes.
#[derive(Default)]
struct Memtable {
    applied: u64,
    entries: u64,
    flushed: u64,
    held: Vec<u64>,
}

impl StateMachine for Memtable {
    type Output = u64;
    const HOLDS_LOG: bool = true;

    fn release_floor(&self) -> Option<u64> {
        self.held.first().map(|first| first - 1)
    }

    fn apply(&mut self, _: &[u8], ctx: &ApplyCtx) -> Result<u64, ApplyError> {
        if self.held.len() as u64 == MEMTABLE_ENTRIES {
            self.flushed = *self.held.last().expect("full");
            self.held.clear();
        }
        self.held.push(ctx.index);
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
        self.flushed = self.applied;
        self.held.clear();
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}

#[test]
fn a_log_under_continuous_writes_is_compacted_once_the_floor_passes_the_snapshot() {
    let mut disk = MemDisk::new();
    let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
    let cfg = CoreConfig { snapshot_entries: SNAPSHOT_ENTRIES, ..CoreConfig::new(ME, 0) };
    let mut core = MultiRaftCore::new(cfg, store, 1);
    let desc = GroupDesc { group: G, voters: vec![ME], learners: vec![] };
    core.create_group(desc, Memtable::default()).unwrap();
    let mut now = Nanos::ZERO;
    let mut done: Vec<(OpId, Result<Done<u64>, OpError>)> = Vec::new();
    let mut ops = Vec::new();
    for i in 0..WRITES + 2 {
        now = now + TICK;
        core.on_tick(now, WALL0 + now.as_millis());
        if i >= 2 {
            ops.push(core.propose(G, b"w", now).unwrap());
        }
        while let Some(op) = core.poll_io(now) {
            let d = disk.execute(op);
            core.on_io_done(d, now);
        }
        done.extend(core.drain_actions().done);
        let sm = core.state_machine(G).unwrap();
        assert!(
            sm.held.is_empty() || sm.release_floor() < Some(sm.applied),
            "the entry just applied is never flushed"
        );
    }
    assert!(!core.has_stopped_group());
    for op in &ops {
        assert!(done.iter().any(|(o, r)| o == op && matches!(r, Ok(Done::Applied(_)))));
    }
    let sm = core.state_machine(G).unwrap();
    let log = core.store().group(G).unwrap();
    assert!(log.snapshot_index > 0, "the log was compacted under the writes");
    assert!(
        log.snapshot_index <= sm.flushed,
        "no snapshot covers what was not flushed: {} > {}",
        log.snapshot_index,
        sm.flushed
    );
    assert!(
        sm.applied - log.snapshot_index <= SNAPSHOT_ENTRIES + 2 * MEMTABLE_ENTRIES,
        "the snapshots keep up with the writes: applied {}, snapshot {}",
        sm.applied,
        log.snapshot_index
    );
}
