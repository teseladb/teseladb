//! 007 T015: a merge applied by the state machines (FR-009, FR-022). Both ranges sit on the same
//! core's LSM, so no version moves: `MergeFreeze` stops the right range at one entry, and `Merge`
//! in the left range takes the right one's frozen state (its end, idempotence table, the greater
//! GC threshold and last mark, the sum of the live bytes) and from then on holds its keys. A
//! merge given up in the left range's log never applies afterwards, and the right range thaws
//! with `MergeFreeze` 0. Everything survives a restart and travels in the snapshots.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::MemDisk;
use teseladb_range::command::{Absorbed, Command, CopiedRecord, DedupRecord, Put};
use teseladb_range::local_keys::RecordScan;
use teseladb_range::sm::{RangeOutput, RangeSm, split_value};
use teseladb_types::{GroupId, Nanos};

const L: GroupId = GroupId(3);
const R: GroupId = GroupId(4);
const START: &[u8] = b"\x01a";
const AT: &[u8] = b"\x01m";
const END: &[u8] = b"\x01z";
/// The Group 0 index of the merge's intent.
const MERGE: u64 = 77;

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

    /// The idempotence table of `g` as the leader reads it.
    fn records(&mut self, g: GroupId) -> Vec<CopiedRecord> {
        let mut scan = RecordScan::new(&self.lsm, g);
        for _ in 0..1000 {
            if let Some(r) = scan.step(&self.lsm) {
                return r.unwrap();
            }
            self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
        }
        panic!("the scan never finished")
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

fn write(key: &[u8], v: u8, stmt: u32, live_delta: i64) -> Command {
    Command::Write {
        puts: vec![Put { key: key.to_vec(), value_type: 1, value: vec![v] }],
        deletes: vec![],
        dedup: Some(DedupRecord {
            lsid: 5,
            txn_number: 1,
            stmt_id: stmt,
            result: vec![stmt as u8],
        }),
        live_delta,
    }
}

/// Two neighbours with a version and a record each, the right one frozen at its entry 3.
fn neighbours(core: &mut Core) -> (RangeSm, RangeSm) {
    let mut l = core.open(L, START, AT);
    let mut r = core.open(R, AT, END);
    apply(&mut l, L, 1, ts(10), &write(b"\x01b", 1, 1, 30)).unwrap();
    apply(&mut r, R, 1, ts(20), &write(b"\x01q", 2, 2, 50)).unwrap();
    apply(&mut r, R, 2, ts(25), &Command::SetGcThreshold { ts: ts(12) }).unwrap();
    let freeze = Command::MergeFreeze { merge: MERGE };
    assert_eq!(apply(&mut r, R, 3, ts(30), &freeze), Ok(RangeOutput::Applied { ts: ts(30) }));
    (l, r)
}

/// The `Merge` that `l`'s leader proposes with `r`'s frozen state.
fn absorb(core: &mut Core, r: &RangeSm) -> Command {
    let records = core.records(R);
    let absorbed = Absorbed {
        end: r.bounds().1.to_vec(),
        gc: r.gc_threshold(),
        mark: r.last_mark(),
        live: r.live_bytes(),
        records,
    };
    Command::Merge { right: R, merge: MERGE, absorb: Some(absorbed) }
}

#[test]
fn a_frozen_range_writes_nothing_until_it_thaws() {
    let mut core = Core::new();
    let (_, mut r) = neighbours(&mut core);
    assert_eq!(r.frozen_for(), Some(MERGE));
    // A snapshot from before the freeze never leaves: every replica takes one past it.
    assert_eq!(r.snapshot_floor(), 3);
    let refused = apply(&mut r, R, 4, ts(40), &write(b"\x01r", 3, 3, 1));
    assert!(refused.unwrap_err().contains("frozen"));
    // The GC threshold still rises: it writes no data.
    apply(&mut r, R, 5, ts(41), &Command::SetGcThreshold { ts: ts(13) }).unwrap();
    // The freeze survives a restart, and thawing lets writes in again.
    let mut core = core.restart();
    let mut r = core.open(R, AT, END);
    assert_eq!(r.frozen_for(), Some(MERGE));
    apply(&mut r, R, 6, ts(50), &Command::MergeFreeze { merge: 0 }).unwrap();
    assert_eq!(r.frozen_for(), None);
    apply(&mut r, R, 7, ts(60), &write(b"\x01r", 3, 3, 1)).unwrap();
    let mut core = core.restart();
    assert_eq!(core.open(R, AT, END).frozen_for(), None);
}

#[test]
fn the_left_range_takes_the_right_ones_keys_and_frozen_state() {
    let mut core = Core::new();
    let (mut l, r) = neighbours(&mut core);
    let merge = absorb(&mut core, &r);
    assert_eq!(
        apply(&mut l, L, 2, ts(40), &merge),
        Ok(RangeOutput::Merged { right: R, merge: MERGE })
    );
    assert_eq!(l.bounds(), (START, END));
    assert_eq!(l.absorbed(), Some((R, MERGE)));
    // The greater GC threshold and last mark, the sum of the live bytes.
    assert_eq!((l.gc_threshold(), l.last_mark(), l.live_bytes()), (ts(12), ts(40), 80));
    // A snapshot from before the merge never leaves either.
    assert_eq!(l.snapshot_floor(), 2);
    // Both tables, under the left range: a retry of either statement finds its mark.
    let marks: Vec<(u32, Hlc)> = core.records(L).iter().map(|c| (c.key.stmt_id, c.mark)).collect();
    assert_eq!(marks, [(1, ts(10)), (2, ts(20))]);
    // The right range's data is read from the left one, which now writes over it.
    assert_eq!(core.get(b"\x01q", ts(40)), Some(vec![2]));
    apply(&mut l, L, 3, ts(50), &write(b"\x01q", 9, 3, 0)).unwrap();
    assert_eq!(
        (core.get(b"\x01q", ts(45)), core.get(b"\x01q", ts(55))),
        (Some(vec![2]), Some(vec![9]))
    );
    // Applying it again changes nothing; after a restart the range keeps its end.
    assert_eq!(
        apply(&mut l, L, 4, ts(60), &merge),
        Ok(RangeOutput::Merged { right: R, merge: MERGE })
    );
    assert_eq!(l.live_bytes(), 80);
    let mut core = core.restart();
    let l = core.open(L, START, AT);
    assert_eq!((l.bounds(), l.absorbed(), l.live_bytes()), ((START, END), Some((R, MERGE)), 80));
    // The snapshot carries the merged state: a replica restored from it on a core that has the
    // data, opened as Group 0 knew the range before the merge, ends up with it.
    let mut copy =
        RangeSm::try_open(L, START.to_vec(), AT.to_vec(), core.lsm.clone()).unwrap().unwrap();
    copy.restore(&l.snapshot()).unwrap();
    assert_eq!(
        (copy.bounds(), copy.absorbed(), copy.live_bytes()),
        ((START, END), Some((R, MERGE)), 80)
    );
}

#[test]
fn a_merge_given_up_never_applies_and_bad_merges_stop_the_range() {
    let mut core = Core::new();
    let (mut l, r) = neighbours(&mut core);
    let merge = absorb(&mut core, &r);
    let give_up = Command::Merge { right: R, merge: MERGE, absorb: None };
    let gave_up = Ok(RangeOutput::MergeGivenUp { right: R, merge: MERGE });
    assert_eq!(apply(&mut l, L, 2, ts(40), &give_up), gave_up);
    assert_eq!(l.abandoned(), MERGE);
    // A `Merge` of that intent proposed before is refused where it lands.
    assert_eq!(apply(&mut l, L, 3, ts(41), &merge), gave_up);
    assert_eq!((l.bounds(), l.absorbed()), ((START, AT), None));
    let mut core = core.restart();
    let mut l = core.open(L, START, AT);
    assert_eq!(l.abandoned(), MERGE);
    let mut copy = core.open(L, START, AT);
    copy.restore(&l.snapshot()).unwrap();
    assert_eq!(copy.abandoned(), MERGE);
    // Giving up a merge already applied changes nothing.
    let mut core = Core::new();
    let (mut l2, r2) = neighbours(&mut core);
    let merge2 = absorb(&mut core, &r2);
    apply(&mut l2, L, 2, ts(40), &merge2).unwrap();
    let merged = Ok(RangeOutput::Merged { right: R, merge: MERGE });
    assert_eq!(apply(&mut l2, L, 3, ts(41), &give_up), merged);
    assert_eq!(l2.abandoned(), 0);
    // An end that does not move the range on, or the range itself, is an error.
    let Command::Merge { absorb: Some(a), .. } = merge else { unreachable!() };
    let back = Command::Merge {
        right: R,
        merge: 90,
        absorb: Some(Absorbed { end: AT.to_vec(), ..a.clone() }),
    };
    assert!(apply(&mut l, L, 4, ts(50), &back).is_err());
    let itself = Command::Merge { right: L, merge: 91, absorb: Some(a) };
    assert!(apply(&mut l, L, 4, ts(50), &itself).is_err());
}

#[test]
fn a_cut_is_known_by_its_new_range_and_not_by_the_end_it_left() {
    // 007 A007-3-1: a split at `AT`, a merge that undoes it and a second split at the same key.
    // A replica that applied only the first cut ends at `AT` too, but it has not applied the
    // second one: Group 0 completes a split on the cut into its own new range, never on an end
    // that happens to match.
    let mut core = Core::new();
    let mut l = core.open(L, START, END);
    let first = Command::Split { at: AT.to_vec(), new_range: R, records: vec![], right_live: 0 };
    apply(&mut l, L, 1, ts(10), &first).unwrap();
    assert_eq!(l.last_cut(), Some(R));
    assert!(l.applied_split(AT, R));
    // The stale replica stays here, with the same end the second split will leave.
    let stale =
        RangeSm::try_open(L, START.to_vec(), END.to_vec(), core.lsm.clone()).unwrap().unwrap();
    let mut r = core.open(R, AT, END);
    apply(&mut r, R, 1, ts(20), &Command::MergeFreeze { merge: MERGE }).unwrap();
    let merge = absorb(&mut core, &r);
    apply(&mut l, L, 2, ts(30), &merge).unwrap();
    assert_eq!(l.bounds(), (START, END));
    const AGAIN: GroupId = GroupId(5);
    let second =
        Command::Split { at: AT.to_vec(), new_range: AGAIN, records: vec![], right_live: 0 };
    assert!(!l.applied_split(AT, AGAIN), "the merged range has not cut again");
    apply(&mut l, L, 3, ts(40), &second).unwrap();
    assert!(l.applied_split(AT, AGAIN));
    // The replica as it was after the first cut ends at `AT`, but not by this split.
    assert_eq!(stale.bounds(), (START, AT));
    assert!(!stale.applied_split(AT, AGAIN));
    assert!(stale.applied_split(AT, R));
    // The cut survives a restart and travels in the snapshots.
    let snap = l.snapshot();
    let mut core = core.restart();
    let reopened = core.open(L, START, END);
    assert_eq!(reopened.last_cut(), Some(AGAIN));
    let mut copy = core.open(L, START, END);
    copy.restore(&snap).unwrap();
    assert!(copy.applied_split(AT, AGAIN));
}
