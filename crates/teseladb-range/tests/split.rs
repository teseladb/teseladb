//! 007 T011: a split applied by the state machine (FR-005, FR-007, FR-022). Both halves stay on
//! the core's LSM, so no version moves: `Split` cuts the range at `at`, persists its new end and
//! writes the new range's local keys (GC threshold, last mark and the whole idempotence table the
//! entry carries). Every version of every key stays readable at its marks from the half that now
//! holds it, a restart keeps the cut, and the new range opens with its parent's state.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::MemDisk;
use teseladb_range::command::{
    Command, CopiedRecord, DedupKey, DedupRecord, IntentPut, Put, Resolution,
};
use teseladb_range::intents::{IntentOp, TxnId, TxnStatus, record_key};
use teseladb_range::local_keys;
use teseladb_range::sm::{RangeOutput, RangeSm, dedup_key, split_value};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(7);
const NEW: GroupId = GroupId(9);
const START: &[u8] = b"\x01a";
const AT: &[u8] = b"\x01m";
const END: &[u8] = b"\x01z";

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn cfg() -> LsmConfig {
    LsmConfig { dir: "core-00/lsm".into(), memtable_bytes: 32 << 10, ..LsmConfig::default() }
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

    fn open(&mut self, g: GroupId, start: &[u8], end: &[u8]) -> RangeSm {
        RangeSm::open(g, start.to_vec(), end.to_vec(), self.lsm.clone(), &mut self.disk).unwrap()
    }

    fn restart(self) -> Core {
        let Core { mut disk, lsm } = self;
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
        drop(lsm);
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        Core { disk, lsm }
    }

    fn get(&mut self, key: &[u8], at: Hlc) -> Option<Vec<u8>> {
        let view = self.lsm.borrow().view();
        for _ in 0..1000 {
            let found = view.get(&mut self.lsm.borrow_mut(), key, at);
            match found {
                Lookup::Hit(f) => return split_value(&f.value).map(|(_, v)| v.to_vec()),
                Lookup::NotFound => return None,
                Lookup::Failed(e) => panic!("{e}"),
                Lookup::Miss(_) => {
                    self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                }
            }
        }
        panic!("the read never finished")
    }
}

fn apply(
    sm: &mut RangeSm,
    g: GroupId,
    index: u64,
    at: Hlc,
    c: &Command,
) -> Result<RangeOutput, String> {
    let ctx = ApplyCtx { group: g, index, term: 1, ts: at };
    sm.apply(&c.encode(), &ctx).map_err(|e| e.0)
}

fn put(key: &[u8], v: u8) -> Put {
    Put { key: key.to_vec(), value_type: 1, value: vec![v] }
}

fn record(stmt: u32) -> DedupRecord {
    DedupRecord { lsid: 5, txn_number: 1, stmt_id: stmt, result: vec![stmt as u8] }
}

fn copied(stmt: u32) -> CopiedRecord {
    let key = DedupKey { lsid: 5, txn_number: 1, stmt_id: stmt };
    CopiedRecord { key, mark: ts(u64::from(stmt)), result: vec![stmt as u8] }
}

fn write(key: &[u8], v: u8, stmt: u32) -> Command {
    Command::Write {
        puts: vec![put(key, v)],
        deletes: vec![],
        dedup: Some(record(stmt)),
        live_delta: 0,
    }
}

/// A range with two versions on each side of `AT`, a raised GC threshold, then the split.
fn split_range(core: &mut Core) -> RangeSm {
    let mut g = core.open(G, START, END);
    for (i, (k, v)) in
        [(&b"\x01b"[..], 1), (b"\x01q", 2), (b"\x01b", 3), (b"\x01q", 4)].into_iter().enumerate()
    {
        let i = i as u64 + 1;
        apply(&mut g, G, i, ts(10 * i), &write(k, v, i as u32)).unwrap();
    }
    apply(&mut g, G, 5, ts(50), &Command::SetGcThreshold { ts: ts(15) }).unwrap();
    let records = (1..=4).map(copied).collect();
    let split = Command::Split { at: AT.to_vec(), new_range: NEW, records, right_live: 0 };
    assert_eq!(
        apply(&mut g, G, 6, ts(60), &split),
        Ok(RangeOutput::Split { new_range: NEW, at: ts(60) })
    );
    g
}

#[test]
fn a_split_keeps_every_version_and_gives_the_new_range_its_parent_state() {
    let mut core = Core::new();
    let mut g = split_range(&mut core);
    assert_eq!(g.bounds(), (START, AT));
    // The parent refuses keys of the new range from the cut on.
    let err = apply(&mut g, G, 7, ts(70), &write(b"\x01q", 9, 9)).unwrap_err();
    assert!(err.contains("outside the range"), "{err}");
    apply(&mut g, G, 7, ts(70), &write(b"\x01c", 9, 9)).unwrap();
    // The new range opens over the other half with its parent's GC threshold and last mark.
    let mut n = core.open(NEW, AT, END);
    assert_eq!((n.gc_threshold(), n.last_mark()), (ts(15), ts(60)));
    assert_eq!(n.applied_index(), 0);
    // Every version of both halves is still there at its marks.
    assert_eq!(core.get(b"\x01q", ts(25)), Some(vec![2]));
    assert_eq!(core.get(b"\x01q", ts(45)), Some(vec![4]));
    assert_eq!(core.get(b"\x01b", ts(15)), Some(vec![1]));
    assert_eq!(core.get(b"\x01b", ts(35)), Some(vec![3]));
    // The whole idempotence table is in both halves.
    for stmt in 1..=4 {
        let key = dedup_key(NEW, 5, 1, stmt);
        let view = core.lsm.borrow().view();
        let hit = matches!(
            view.get(
                &mut core.lsm.borrow_mut(),
                &key,
                Hlc { physical: u64::MAX, logical: u32::MAX }
            ),
            Lookup::Hit(_)
        );
        assert!(hit, "record {stmt} in the new range");
    }
    // The new range writes its own keys and refuses its parent's.
    let genesis = Command::SplitGenesis { parent: G, at_index: 6, end: END.to_vec() };
    assert_eq!(apply(&mut n, NEW, 1, ts(80), &genesis), Ok(RangeOutput::Applied { ts: ts(80) }));
    apply(&mut n, NEW, 2, ts(81), &write(b"\x01r", 5, 20)).unwrap();
    assert!(apply(&mut n, NEW, 3, ts(82), &write(b"\x01b", 5, 21)).is_err());
    assert_eq!(core.get(b"\x01r", ts(81)), Some(vec![5]));
}

#[test]
fn the_cut_survives_a_restart() {
    let mut core = Core::new();
    drop(split_range(&mut core));
    let mut core = core.restart();
    // Opened again with the bounds it had before the split (Group 0 may not have completed it
    // yet), the parent keeps the cut it applied.
    let mut g = core.open(G, START, END);
    assert_eq!(g.bounds(), (START, AT));
    assert!(apply(&mut g, G, 7, ts(70), &write(b"\x01q", 9, 9)).is_err());
    // Its snapshot exports only its half.
    assert_eq!(g.intervals()[1], (START.to_vec(), AT.to_vec()));
}

#[test]
fn a_restart_replays_the_entries_below_the_flushed_index_without_applying_them() {
    // The LSM flushed the split, but the log's snapshot after it was not saved before the crash:
    // the log replays from its first entry, with writes of keys the cut gave away.
    let mut core = Core::new();
    drop(split_range(&mut core));
    let mut core = core.restart();
    let mut g = core.open(G, START, END);
    assert_eq!((g.bounds(), g.applied_index()), ((START, AT), 6));
    // It does not know which split came last below what it flushed: no snapshot below it leaves.
    assert_eq!(g.snapshot_floor(), 6);
    let entries = [
        write(b"\x01b", 1, 1),
        write(b"\x01q", 2, 2),
        write(b"\x01b", 3, 3),
        write(b"\x01q", 4, 4),
        Command::SetGcThreshold { ts: ts(15) },
        Command::Split { at: AT.to_vec(), new_range: NEW, records: vec![], right_live: 0 },
    ];
    let mut mid = None;
    for (i, c) in entries.iter().enumerate() {
        let i = i as u64 + 1;
        assert_eq!(apply(&mut g, G, i, ts(10 * i), c), Ok(RangeOutput::Applied { ts: ts(10 * i) }));
        assert_eq!(g.applied_index(), i, "entry {i} is in the LSM already");
        // A snapshot taken now holds the state up to what the LSM flushed, past the floor.
        assert_eq!(g.snapshot_state_index(), 6);
        if i == 3 {
            mid = Some(g.snapshot());
        }
    }
    assert_eq!(g.bounds(), (START, AT));
    // A snapshot taken during the replay holds the state the LSM has: restored after another
    // restart, the log replays from its entry and the rest are not applied again either.
    let (mut other, _) = try_open_running_io(&mut core, G, END);
    other.restore(&mid.expect("taken")).unwrap();
    assert_eq!((other.bounds(), other.applied_index()), ((START, AT), 6));
    for (i, c) in entries.iter().enumerate().skip(3) {
        let i = i as u64 + 1;
        assert!(apply(&mut other, G, i, ts(10 * i), c).is_ok(), "entry {i}");
    }
    assert_eq!(core.get(b"\x01q", ts(100)), Some(vec![4]));
    // The entries past it apply.
    apply(&mut g, G, 7, ts(70), &write(b"\x01c", 9, 9)).unwrap();
    assert_eq!(g.applied_index(), 7);
    assert_eq!(core.get(b"\x01c", ts(100)), Some(vec![9]));
}

#[test]
fn a_split_point_must_fall_strictly_inside_the_range() {
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    for at in [START, b"\x01", END, b"\x02"] {
        let split =
            Command::Split { at: at.to_vec(), new_range: NEW, records: vec![], right_live: 0 };
        let err = apply(&mut g, G, 1, ts(1), &split).unwrap_err();
        assert!(err.contains("split"), "{err}");
    }
    // Into itself or into the local space it is refused as well.
    let split = Command::Split { at: AT.to_vec(), new_range: G, records: vec![], right_live: 0 };
    assert!(apply(&mut g, G, 1, ts(1), &split).is_err());
    assert_eq!(g.bounds(), (START, END));
}

#[test]
fn split_commands_round_trip_and_reject_garbage() {
    let cmds = [
        Command::Split {
            at: AT.to_vec(),
            new_range: NEW,
            records: vec![copied(1), copied(2)],
            right_live: 0,
        },
        Command::SplitGenesis { parent: G, at_index: 44, end: END.to_vec() },
    ];
    for c in cmds {
        assert_eq!(Command::decode(&c.encode()), Ok(c.clone()));
        let mut long = c.encode();
        long.push(0);
        assert!(Command::decode(&long).is_err());
        let short = c.encode();
        assert!(Command::decode(&short[..short.len() - 1]).is_err());
    }
}

#[test]
fn the_leader_reads_only_live_records() {
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    apply(&mut g, G, 1, ts(10), &write(b"\x01b", 1, 1)).unwrap();
    apply(&mut g, G, 2, ts(20), &write(b"\x01c", 2, 2)).unwrap();
    let purge = Command::PurgeDedup { keys: vec![dedup_key(G, 5, 1, 1)] };
    apply(&mut g, G, 3, ts(30), &purge).unwrap();
    let mut scan = local_keys::RecordScan::new(&core.lsm, G);
    let records = loop {
        match scan.step(&core.lsm) {
            Some(r) => break r.expect("readable"),
            None => {
                core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
            }
        }
    };
    // The purged record is gone; the live one comes with the mark of its write.
    assert_eq!(records, [CopiedRecord { mark: ts(20), ..copied(2) }]);
    // A key that is not a record, or a stored value without its mark, is not read as one.
    assert_eq!(local_keys::record_of(b"\x00short", &[0; 12]), None);
    assert_eq!(local_keys::record_of(&[0; 39], &[0; 12]), None);
    assert_eq!(local_keys::record_of(&dedup_key(G, 1, 1, 1), b"x"), None);
}

#[test]
fn local_keys_of_a_split_are_the_parent_state_under_the_new_range() {
    let writes = local_keys::split_writes(NEW, END, ts(3), ts(4), &[copied(7)]);
    let keys: Vec<&[u8]> = writes.iter().map(|(k, _)| k.as_slice()).collect();
    assert_eq!(keys.len(), 4);
    assert!(keys.contains(&dedup_key(NEW, 5, 1, 7).as_slice()));
    assert!(keys.iter().all(|k| k[0] == 0 && k[1..9] == NEW.0.to_be_bytes()));
}

/// Opens `g` with [`RangeSm::try_open`], running the LSM's I/O between tries; the range and the
/// number of tries.
fn try_open_running_io(core: &mut Core, g: GroupId, end: &[u8]) -> (RangeSm, u32) {
    let start = if g == G { START } else { AT };
    for tries in 1..100 {
        match RangeSm::try_open(g, start.to_vec(), end.to_vec(), core.lsm.clone()).unwrap() {
            Some(sm) => return (sm, tries),
            None => {
                core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
            }
        }
    }
    panic!("the range never opened")
}

#[test]
fn the_new_range_opens_without_the_disk_and_knows_its_parent_until_its_genesis() {
    let mut core = Core::new();
    drop(split_range(&mut core));
    // Opened with any end, it takes the one its parent's split persisted, and its genesis.
    let mut n = RangeSm::try_open(NEW, AT.to_vec(), b"\x04".to_vec(), core.lsm.clone())
        .unwrap()
        .expect("everything is in the memtable");
    assert_eq!(n.bounds(), (AT, END));
    let (parent, index) = n.born().expect("born from a split");
    assert_eq!(parent, G);
    assert!(index > 0);
    assert!(n.from_split());
    assert_eq!(n.last_write(), 0);
    let genesis = Command::SplitGenesis { parent, at_index: index, end: END.to_vec() };
    apply(&mut n, NEW, 1, ts(90), &genesis).unwrap();
    assert_eq!((n.born(), n.last_write()), (None, 1));
    // Once its genesis is applied it is no longer reported, also after a restart, where the keys
    // are in a file: the first try queues the read.
    let mut core = core.restart();
    let (n, tries) = try_open_running_io(&mut core, NEW, END);
    assert!(tries > 1);
    assert_eq!((n.born(), n.bounds()), (None, (AT, END)));
    assert_eq!(RangeSm::has_own_end(&core.lsm, NEW), Some(true));
    assert_eq!(RangeSm::has_own_end(&core.lsm, GroupId(99)), Some(false));
}

#[test]
fn a_replica_that_replays_a_born_range_from_its_first_entry_learns_its_end() {
    let core = Core::new();
    // A replica with no local key opens with the end of the data space.
    let mut n =
        RangeSm::try_open(NEW, AT.to_vec(), b"\x04".to_vec(), core.lsm.clone()).unwrap().unwrap();
    assert_eq!(n.born(), None);
    assert!(!n.from_split());
    let bad = Command::SplitGenesis { parent: G, at_index: 6, end: AT.to_vec() };
    assert!(apply(&mut n, NEW, 1, ts(1), &bad).is_err());
    let genesis = Command::SplitGenesis { parent: G, at_index: 6, end: END.to_vec() };
    apply(&mut n, NEW, 1, ts(1), &genesis).unwrap();
    assert_eq!(n.bounds(), (AT, END));
    assert!(apply(&mut n, NEW, 2, ts(2), &write(b"\x01z", 5, 20)).is_err());
}

#[test]
fn a_snapshot_carries_the_end() {
    let mut core = Core::new();
    let parent = split_range(&mut core);
    let snap = parent.snapshot();
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    // A replica that opened before the split takes the cut from the snapshot.
    let mut other = core.open(GroupId(11), START, END);
    assert!(other.restore(&snap).is_err());
    let (mut same, _) = try_open_running_io(&mut core, G, b"\x04");
    assert_eq!(same.snapshot_end(&snap), Some(AT.to_vec()));
    assert_eq!(other.snapshot_end(&snap), None, "not one of its snapshots");
    same.restore(&snap).unwrap();
    assert_eq!(same.bounds(), (START, AT));
}

#[test]
fn no_snapshot_from_before_the_last_split_leaves_a_replica() {
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    assert_eq!(g.snapshot_floor(), 0, "no split yet");
    apply(&mut g, G, 1, ts(10), &write(b"\x01b", 1, 1)).unwrap();
    assert_eq!(g.snapshot_floor(), 0);
    let split = Command::Split { at: AT.to_vec(), new_range: NEW, records: vec![], right_live: 0 };
    apply(&mut g, G, 2, ts(20), &split).unwrap();
    assert_eq!(g.snapshot_floor(), 2, "the split's entry");
    apply(&mut g, G, 3, ts(30), &write(b"\x01c", 1, 2)).unwrap();
    assert_eq!(g.snapshot_floor(), 2, "a write does not move it");
    let snap = g.snapshot();
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    // A replica opened over a flushed LSM, or restored from a snapshot, does not know which
    // split came last below it: none below what it holds leaves it.
    let (mut other, _) = try_open_running_io(&mut core, G, END);
    assert_eq!(other.snapshot_floor(), 3);
    other.restore(&snap).unwrap();
    assert_eq!(other.snapshot_floor(), 3);
    // The new range's first entry: its log lacks the data the split left in the LSM.
    let mut n = core.open(NEW, AT, END);
    assert_eq!(n.snapshot_floor(), 0);
    let genesis = Command::SplitGenesis { parent: G, at_index: 2, end: END.to_vec() };
    apply(&mut n, NEW, 1, ts(40), &genesis).unwrap();
    assert_eq!(n.snapshot_floor(), 1);
}

#[test]
fn the_live_bytes_follow_the_writes_and_split_between_the_halves() {
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    let sized = |key: &[u8], delta: i64| Command::Write {
        puts: vec![put(key, 1)],
        deletes: vec![],
        dedup: None,
        live_delta: delta,
    };
    apply(&mut g, G, 1, ts(1), &sized(b"\x01b", 40)).unwrap();
    apply(&mut g, G, 2, ts(2), &sized(b"\x01q", 60)).unwrap();
    // A delta that would go below zero stops at zero.
    apply(&mut g, G, 3, ts(3), &sized(b"\x01c", -200)).unwrap();
    assert_eq!(g.live_bytes(), 0);
    apply(&mut g, G, 4, ts(4), &sized(b"\x01d", 90)).unwrap();
    let split = Command::Split { at: AT.to_vec(), new_range: NEW, records: vec![], right_live: 60 };
    apply(&mut g, G, 5, ts(5), &split).unwrap();
    assert_eq!(g.live_bytes(), 30);
    assert_eq!(core.open(NEW, AT, END).live_bytes(), 60);
    // The counter survives a restart and travels in the snapshot.
    let snap = g.snapshot();
    let mut core = core.restart();
    let mut again = core.open(G, START, END);
    assert_eq!(again.live_bytes(), 30);
    again.restore(&snap).unwrap();
    assert_eq!(again.live_bytes(), 30);
    let short = &snap[..snap.len() - 1];
    assert!(again.restore(short).is_err());
}

#[test]
fn a_replica_opened_with_nothing_of_a_range_that_split_waits_for_a_snapshot() {
    let mut core = Core::new();
    // Opened with nothing: it waits until a snapshot is restored.
    let mut empty = core.open(GroupId(11), START, END);
    assert!(!empty.needs_snapshot());
    empty.expect_snapshot();
    assert!(empty.needs_snapshot());
    // One that applied entries, or holds its parent's cut, keeps using its log.
    let mut g = split_range(&mut core);
    g.expect_snapshot();
    assert!(!g.needs_snapshot());
    let snap = g.snapshot();
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    let (mut born, _) = try_open_running_io(&mut core, NEW, END);
    born.expect_snapshot();
    assert!(!born.needs_snapshot(), "its parent's cut wrote its keys here");
    let (mut reopened, _) = try_open_running_io(&mut core, G, END);
    reopened.expect_snapshot();
    assert!(!reopened.needs_snapshot(), "it applied up to its flushed entry");
    reopened.restore(&snap).unwrap();
    // So does a first entry applied as the range's founding leader.
    let mut founder = core.open(GroupId(12), START, END);
    founder.expect_snapshot();
    assert!(founder.needs_snapshot());
    apply(&mut founder, GroupId(12), 1, ts(90), &write(b"\x01b", 1, 30)).unwrap();
    assert!(!founder.needs_snapshot());
    // A restored snapshot ends the wait.
    let snap = core.open(GroupId(11), START, END).snapshot();
    empty.restore(&snap).unwrap();
    assert!(!empty.needs_snapshot());
}

#[test]
fn a_replica_waiting_for_a_born_range_has_none_of_its_data_until_it_arrives() {
    // The range born from the split, on the node that applied the cut, before any write of its
    // own: its snapshot holds what the split left in the parent's LSM.
    let mut core = Core::new();
    drop(split_range(&mut core));
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    let (mut born, _) = try_open_running_io(&mut core, NEW, END);
    apply(&mut born, NEW, 1, ts(70), &Command::SetGcThreshold { ts: ts(1) }).unwrap();
    let snap = born.snapshot();
    // On another node, a replica opened with nothing of the range: none of that data is there.
    let mut other = Core::new();
    let mut waiting = other.open(NEW, AT, END);
    waiting.expect_snapshot();
    assert!(waiting.needs_snapshot());
    assert!(!waiting.has_data_for(&snap), "the split's data has to arrive first");
}

#[test]
fn a_split_keeps_versions_and_intents_on_their_side_and_loses_nothing() {
    // 007 T011: versions and a transaction's intents on both sides of the cut (where the cut falls
    // by size is `ranges.rs`' concern in the server).
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    for (i, k) in [&b"\x01b"[..], b"\x01q", b"\x01b", b"\x01q"].into_iter().enumerate() {
        let i = i as u64 + 1;
        apply(&mut g, G, i, ts(10 * i), &write(k, i as u8, i as u32)).unwrap();
    }
    let txn = TxnId(7);
    let intent = |key: &[u8], v: u8| IntentPut {
        key: key.to_vec(),
        op: IntentOp::Put { value_type: 1, value: vec![v] },
        replaces: None,
    };
    let puts = Command::PutIntents {
        txn,
        record_key: record_key(txn),
        intents: vec![intent(b"\x01c", 70), intent(b"\x01r", 71)],
    };
    apply(&mut g, G, 5, ts(50), &puts).unwrap();
    // The cut falls between the two intents, with part of the versions on each side.
    let at = AT.to_vec();
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    let split = Command::Split { at: at.clone(), new_range: NEW, records: vec![], right_live: 0 };
    apply(&mut g, G, 6, ts(60), &split).unwrap();
    let mut n = core.open(NEW, &at, END);
    let genesis = Command::SplitGenesis { parent: G, at_index: 6, end: END.to_vec() };
    apply(&mut n, NEW, 1, ts(61), &genesis).unwrap();
    // Every version is still there at its marks.
    assert_eq!(core.get(b"\x01b", ts(15)), Some(vec![1]));
    assert_eq!(core.get(b"\x01b", ts(35)), Some(vec![3]));
    assert_eq!(core.get(b"\x01q", ts(25)), Some(vec![2]));
    assert_eq!(core.get(b"\x01q", ts(45)), Some(vec![4]));
    // Each half resolves the intent of its own side, and refuses the other side's.
    let resolve = |key: &[u8]| Command::ResolveIntents {
        txn,
        status: TxnStatus::Committed,
        commit_ts: ts(90),
        resolutions: vec![Resolution {
            key: key.to_vec(),
            intent_ts: ts(50),
            commit: Some(IntentOp::Put { value_type: 1, value: vec![key[1]] }),
        }],
        live_delta: 0,
    };
    let (left, right) =
        if b"\x01c".as_slice() < at.as_slice() { (&mut g, &mut n) } else { (&mut n, &mut g) };
    let (lg, rg) = if b"\x01c".as_slice() < at.as_slice() { (G, NEW) } else { (NEW, G) };
    assert!(apply(right, rg, 7, ts(91), &resolve(b"\x01c")).is_err(), "not its key");
    apply(left, lg, 7, ts(91), &resolve(b"\x01c")).unwrap();
    apply(right, rg, 7, ts(92), &resolve(b"\x01r")).unwrap();
    assert_eq!(core.get(b"\x01c", ts(95)), Some(vec![b'c']));
    assert_eq!(core.get(b"\x01r", ts(95)), Some(vec![b'r']));
}

/// Data a restore ingested (018) is not in the log: from the index the range applied then, no
/// replica rebuilds it from the entries, and the snapshot that leaves holds it. A replica opened
/// over its LSM, or restored from a snapshot, takes what it holds as that index.
#[test]
fn data_ingested_without_the_log_raises_the_log_floor() {
    let mut core = Core::new();
    let mut g = core.open(G, START, END);
    assert_eq!(g.log_floor(), 0, "the whole log rebuilds a new range");
    apply(&mut g, G, 1, ts(10), &write(b"\x01b", 1, 1)).unwrap();
    apply(&mut g, G, 2, ts(20), &write(b"\x01c", 1, 2)).unwrap();
    g.data_ingested();
    assert_eq!((g.log_floor(), g.snapshot_floor()), (2, 2));
    apply(&mut g, G, 3, ts(30), &write(b"\x01d", 1, 3)).unwrap();
    assert_eq!(g.log_floor(), 2, "a write does not move it");
    let snap = g.snapshot();
    core.lsm.borrow_mut().flush();
    core.lsm.borrow_mut().run_io(&mut core.disk, Nanos::ZERO);
    let (mut other, _) = try_open_running_io(&mut core, G, END);
    assert_eq!(other.log_floor(), 3, "what the LSM flushed");
    other.restore(&snap).unwrap();
    assert_eq!(other.log_floor(), 3, "the snapshot's index");
}
