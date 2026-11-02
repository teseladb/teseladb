//! 007 T020: a core change as the range's state machine and each core see it (FR-011,
//! `spec/tla/CoreRelocate.tla`). `Relocate` in the range's log names the target core and the
//! Group 0 intent; a replica on another core stops applying there, one on the target core runs.
//! The target survives a restart (local key `0B`) and travels in the snapshots, so a replica that
//! missed the entry learns it from a snapshot. Each core decides from its copy and the target
//! what to do: run, hand the range over, drop its copy or start an imported one.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::MemDisk;
use teseladb_range::command::{Command, KIND_RELOCATE, Put};
use teseladb_range::relocate::{Copy, Step, step};
use teseladb_range::sm::{RangeOutput, RangeSm};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(5);
const START: &[u8] = b"\x01a";
const END: &[u8] = b"\x01z";
/// The core the range starts on and the one it moves to.
const OLD: u16 = 1;
const NEW: u16 = 3;
/// Group 0 indexes of two intents.
const INTENT: u64 = 40;
const BACK: u64 = 52;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn cfg() -> LsmConfig {
    LsmConfig { dir: "core-01/lsm".into(), memtable_bytes: 32 << 10, ..LsmConfig::default() }
}

struct Core {
    disk: MemDisk,
    lsm: Rc<RefCell<Lsm>>,
}

impl Core {
    fn new() -> Core {
        let mut disk = MemDisk::new();
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        Core { disk, lsm }
    }

    fn open(&mut self, core: u16) -> RangeSm {
        let mut sm =
            RangeSm::open(G, START.to_vec(), END.to_vec(), self.lsm.clone(), &mut self.disk)
                .unwrap();
        sm.set_core(core);
        sm
    }

    /// Puts what the memtable holds on disk: a snapshot restores over flushed data.
    fn flush(&mut self) {
        self.lsm.borrow_mut().flush();
        self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
    }

    fn restart(self) -> Core {
        let Core { mut disk, lsm } = self;
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
        drop(lsm);
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        Core { disk, lsm }
    }
}

fn apply(sm: &mut RangeSm, index: u64, at: Hlc, c: &Command) -> Result<RangeOutput, String> {
    let ctx = ApplyCtx { group: G, index, term: 1, ts: at };
    sm.apply(&c.encode(), &ctx).map_err(|e| e.0)
}

fn write(key: &[u8]) -> Command {
    Command::Write {
        puts: vec![Put { key: key.to_vec(), value_type: 1, value: vec![7] }],
        deletes: vec![],
        dedup: None,
        live_delta: 4,
    }
}

#[test]
fn relocate_round_trips_with_its_kind() {
    let c = Command::Relocate { core: NEW, intent: INTENT };
    assert_eq!(c.kind(), KIND_RELOCATE);
    assert_eq!(Command::decode(&c.encode()), Ok(c));
}

#[test]
fn a_replica_on_another_core_than_the_target_halts_and_one_on_the_target_runs() {
    let mut core = Core::new();
    let mut old = core.open(OLD);
    apply(&mut old, 1, ts(10), &write(b"\x01b")).unwrap();
    assert!(!old.halted() && old.relocated().is_none());
    let relocate = Command::Relocate { core: NEW, intent: INTENT };
    assert_eq!(apply(&mut old, 2, ts(20), &relocate), Ok(RangeOutput::Applied { ts: ts(20) }));
    assert_eq!(old.relocated(), Some((NEW, INTENT)));
    assert!(old.halted());
    // The multi-Raft core asks through the trait: it hands the replica nothing more.
    assert!(StateMachine::halted(&old));
    // The same state on the target core runs.
    core.flush();
    let mut on_new = core.open(NEW);
    on_new.restore(&old.snapshot()).unwrap();
    assert_eq!(on_new.relocated(), Some((NEW, INTENT)));
    assert!(!on_new.halted());
}

#[test]
fn the_target_survives_a_restart_and_a_repeat_of_the_intent_changes_nothing() {
    let mut core = Core::new();
    let mut sm = core.open(OLD);
    apply(&mut sm, 1, ts(10), &Command::Relocate { core: NEW, intent: INTENT }).unwrap();
    apply(&mut sm, 2, ts(11), &Command::Relocate { core: OLD, intent: INTENT }).unwrap();
    assert_eq!(sm.relocated(), Some((NEW, INTENT)), "a repeat of an intent is a no-op");
    drop(sm);
    let mut core = core.restart();
    let sm = core.open(OLD);
    assert_eq!(sm.relocated(), Some((NEW, INTENT)));
    assert!(sm.halted());
    let sm = core.open(NEW);
    assert!(!sm.halted());
}

#[test]
fn a_later_intent_moves_the_range_back() {
    let mut core = Core::new();
    let mut sm = core.open(NEW);
    apply(&mut sm, 1, ts(10), &Command::Relocate { core: NEW, intent: INTENT }).unwrap();
    assert!(!sm.halted());
    apply(&mut sm, 2, ts(20), &Command::Relocate { core: OLD, intent: BACK }).unwrap();
    assert_eq!(sm.relocated(), Some((OLD, BACK)));
    assert!(sm.halted());
}

#[test]
fn a_replica_that_missed_the_entry_learns_the_target_from_a_snapshot() {
    let mut core = Core::new();
    let mut lagging = core.open(OLD);
    let mut leader = core.open(NEW);
    apply(&mut leader, 1, ts(10), &Command::Relocate { core: NEW, intent: INTENT }).unwrap();
    let snap = leader.snapshot();
    core.flush();
    assert!(!lagging.halted());
    lagging.restore(&snap).unwrap();
    assert_eq!(lagging.relocated(), Some((NEW, INTENT)));
    assert!(lagging.halted());
}

#[test]
fn a_snapshot_written_before_core_changes_existed_restores_with_none() {
    let mut core = Core::new();
    let mut sm = core.open(OLD);
    apply(&mut sm, 1, ts(10), &write(b"\x01b")).unwrap();
    let mut snap = sm.snapshot();
    // The write fences (018: an empty list, one byte), the field the last cut added (u64,
    // A007-3-1), then the two a core change added: the target (u16) and the intent (u64). A
    // snapshot from before each of them restores with none.
    snap.truncate(snap.len() - 1);
    core.flush();
    let mut other = core.open(OLD);
    other.restore(&snap).unwrap();
    assert!(other.fences().is_empty());
    snap.truncate(snap.len() - 8);
    let mut other = core.open(OLD);
    other.restore(&snap).unwrap();
    assert_eq!((other.relocated(), other.last_cut()), (None, None));
    snap.truncate(snap.len() - 10);
    let mut other = core.open(OLD);
    other.restore(&snap).unwrap();
    assert_eq!(other.relocated(), None);
    assert_eq!(other.applied_index(), 1);
}

#[test]
fn each_core_runs_hands_over_drops_or_starts_its_copy() {
    // A copy that never moved, or moved here, runs.
    assert_eq!(step(OLD, None, Copy::Running, None), Step::Run);
    assert_eq!(step(NEW, Some(NEW), Copy::Running, Some(true)), Step::Run);
    // A copy whose range moved elsewhere hands it over, whatever the other cores hold: the target
    // drops an imported copy it had for a hand-over that comes again.
    assert_eq!(step(OLD, Some(NEW), Copy::Running, None), Step::Hand);
    assert_eq!(step(OLD, Some(NEW), Copy::Running, Some(true)), Step::Hand);
    // An imported copy starts once no other core holds the range.
    let fresh = Copy::Imported { this_run: true };
    assert_eq!(step(NEW, Some(NEW), fresh, Some(false)), Step::Start);
    assert_eq!(step(NEW, Some(NEW), fresh, Some(true)), Step::Wait);
    assert_eq!(step(NEW, Some(NEW), fresh, None), Step::Wait);
    // After a restart, an imported copy is dropped while the old core still holds the range: that
    // core hands it over again.
    let before = Copy::Imported { this_run: false };
    assert_eq!(step(NEW, Some(NEW), before, Some(true)), Step::Drop);
    assert_eq!(step(NEW, Some(NEW), before, Some(false)), Step::Start);
    assert_eq!(step(NEW, Some(NEW), before, None), Step::Wait);
}
