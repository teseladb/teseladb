//! 018 T005: write fences applied by the state machine (FR-002, `Reshard.tla`'s `blockedW`). From
//! the `Fence` entry on, a `Write` or `PutIntents` with a key of the span is refused at every
//! replica and writes nothing, while the resolution of intents laid before it applies. The fences
//! survive a restart, travel in the snapshots, follow a split to the half that holds their keys
//! and a merge into the left range; lifting one lets the writes through again.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::MemDisk;
use teseladb_range::command::{Absorbed, Command, IntentPut, Put, Resolution};
use teseladb_range::intents::{IntentOp, TxnId, TxnStatus, record_key};
use teseladb_range::sm::{RangeOutput, RangeSm, RangeState, split_value};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(3);
const R: GroupId = GroupId(4);
const START: &[u8] = b"\x01a";
const END: &[u8] = b"\x01z";
/// The fenced span: one collection's keys.
const FROM: &[u8] = b"\x01c";
const TO: &[u8] = b"\x01p";
const INSIDE: &[u8] = b"\x01d";
const OUTSIDE: &[u8] = b"\x01b";

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

    fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let view = self.lsm.borrow().view();
        for _ in 0..1000 {
            let found = view.get(&mut self.lsm.borrow_mut(), key, ts(u64::MAX >> 1));
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

fn apply(sm: &mut RangeSm, g: GroupId, index: u64, at: Hlc, c: &Command) -> RangeOutput {
    let ctx = ApplyCtx { group: g, index, term: 1, ts: at };
    sm.apply(&c.encode(), &ctx).unwrap()
}

fn write(key: &[u8], v: u8) -> Command {
    let puts = vec![Put { key: key.to_vec(), value_type: 1, value: vec![v] }];
    Command::Write { puts, deletes: vec![], dedup: None, live_delta: 0 }
}

fn fence(on: bool) -> Command {
    Command::Fence { start: FROM.to_vec(), end: TO.to_vec(), on }
}

fn intents(txn: TxnId, key: &[u8]) -> Command {
    let op = IntentOp::Put { value_type: 1, value: b"t".to_vec() };
    let intents = vec![IntentPut { key: key.to_vec(), op, replaces: None }];
    Command::PutIntents { txn, record_key: record_key(txn), intents }
}

fn applied(at: u64) -> RangeOutput {
    RangeOutput::Applied { ts: ts(at) }
}

#[test]
fn a_fence_refuses_the_writes_and_intents_of_its_span_from_its_entry_on() {
    let mut core = Core::new();
    let mut sm = core.open(G, START, END);
    let laid = TxnId(7);
    // Before the fence: a write and a transaction's intent of the span go through.
    assert_eq!(apply(&mut sm, G, 1, ts(10), &write(INSIDE, 1)), applied(10));
    assert_eq!(apply(&mut sm, G, 2, ts(11), &intents(laid, b"\x01e")), applied(11));
    assert_eq!(apply(&mut sm, G, 3, ts(12), &fence(true)), applied(12));
    assert!(sm.fenced(INSIDE) && sm.fenced(FROM) && !sm.fenced(TO) && !sm.fenced(OUTSIDE));
    // From the fence on: refused, nothing written; outside the span, written.
    assert_eq!(apply(&mut sm, G, 4, ts(13), &write(INSIDE, 2)), RangeOutput::Fenced);
    assert_eq!(apply(&mut sm, G, 5, ts(14), &intents(TxnId(8), b"\x01f")), RangeOutput::Fenced);
    assert_eq!(apply(&mut sm, G, 6, ts(15), &write(OUTSIDE, 3)), applied(15));
    assert_eq!(core.get(INSIDE), Some(vec![1]));
    assert_eq!(core.get(OUTSIDE), Some(vec![3]));
    assert_eq!(sm.applied_index(), 6);
    // The transaction decided before the fence still resolves its intent.
    let resolve = Command::ResolveIntents {
        txn: laid,
        status: TxnStatus::Committed,
        commit_ts: ts(11),
        resolutions: vec![Resolution {
            key: b"\x01e".to_vec(),
            intent_ts: ts(11),
            commit: Some(IntentOp::Put { value_type: 1, value: b"t".to_vec() }),
        }],
        live_delta: 0,
    };
    assert_eq!(apply(&mut sm, G, 7, ts(16), &resolve), applied(16));
    assert_eq!(core.get(b"\x01e"), Some(b"t".to_vec()));
    // The same fence twice is one; lifting it lets the writes through.
    assert_eq!(apply(&mut sm, G, 8, ts(17), &fence(true)), applied(17));
    assert_eq!(sm.fences().len(), 1);
    assert_eq!(apply(&mut sm, G, 9, ts(18), &fence(false)), applied(18));
    assert!(sm.fences().is_empty());
    assert_eq!(apply(&mut sm, G, 10, ts(19), &write(INSIDE, 4)), applied(19));
    assert_eq!(core.get(INSIDE), Some(vec![4]));
}

#[test]
fn fences_survive_a_restart_and_travel_in_snapshots() {
    let mut core = Core::new();
    let mut sm = core.open(G, START, END);
    apply(&mut sm, G, 1, ts(10), &fence(true));
    let snap = sm.snapshot();
    drop(sm);
    let mut core = core.restart();
    let mut sm = core.open(G, START, END);
    assert!(sm.fenced(INSIDE));
    assert_eq!(apply(&mut sm, G, 2, ts(11), &write(INSIDE, 1)), RangeOutput::Fenced);
    let mut copy = RangeSm::try_open(GroupId(5), START.to_vec(), END.to_vec(), core.lsm.clone())
        .unwrap()
        .unwrap();
    assert!(!copy.fenced(INSIDE));
    // A snapshot of another range does not restore; one of this range brings its fences.
    assert!(copy.restore(&snap).is_err());
    let mut same =
        RangeSm::try_open(G, START.to_vec(), END.to_vec(), core.lsm.clone()).unwrap().unwrap();
    same.restore(&snap).unwrap();
    assert!(same.fenced(INSIDE));
}

#[test]
fn a_split_and_a_merge_keep_the_fences_of_their_keys() {
    let mut core = Core::new();
    let mut sm = core.open(G, START, END);
    apply(&mut sm, G, 1, ts(10), &fence(true));
    // Cut inside the span: each half keeps its part of the fence.
    let at = b"\x01h";
    let split = Command::Split { at: at.to_vec(), new_range: R, records: vec![], right_live: 0 };
    apply(&mut sm, G, 2, ts(11), &split);
    assert_eq!(sm.fences(), [(FROM.to_vec(), at.to_vec())]);
    let mut right = core.open(R, at, END);
    assert_eq!(right.fences(), [(at.to_vec(), TO.to_vec())]);
    assert_eq!(apply(&mut right, R, 1, ts(12), &write(b"\x01i", 1)), RangeOutput::Fenced);
    assert_eq!(apply(&mut right, R, 2, ts(13), &write(b"\x01q", 2)), applied(13));
    // The left range lifts its part; merging the right one back brings that part again.
    let lift = Command::Fence { start: FROM.to_vec(), end: at.to_vec(), on: false };
    apply(&mut sm, G, 3, ts(14), &lift);
    assert!(!sm.fenced(INSIDE));
    let absorb = Absorbed {
        end: END.to_vec(),
        gc: ts(0),
        mark: ts(13),
        live: 0,
        records: vec![],
        fences: right.fences().to_vec(),
    };
    let merge = Command::Merge { right: R, merge: 9, absorb: Some(absorb) };
    apply(&mut sm, G, 4, ts(15), &merge);
    assert!(sm.fenced(b"\x01i") && !sm.fenced(INSIDE));
    drop(sm);
    let mut core = core.restart();
    let sm = core.open(G, START, END);
    assert_eq!(sm.fences(), [(at.to_vec(), TO.to_vec())]);
    assert_eq!(core.get(b"\x01q"), Some(vec![2]));
}
