//! Intents and transaction records in a range (008 T005, FR-001 to FR-005, contract
//! `txn-protocol.md`): the rules the leader applies before `PutIntents`, `EndTxn` and `PushTxn`,
//! the commands' encoding, and their application by the state machine, which every replica
//! repeats and a restart keeps.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Kind, Lookup, Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::MemDisk;
use teseladb_range::command::{Command, IntentPut, Resolution};
use teseladb_range::intents::{
    ABANDONED_AFTER_MS, Conflict, Intent, IntentOp, IntentOwner, Latest, Push, RecordError,
    RecordOp, TXN_SPACE, TxnId, TxnRecord, TxnStatus, check_put, end_txn, pred, push_txn,
    record_key, succ,
};
use teseladb_range::sm::{RangeSm, last_mark_key, live_key};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(9);
const START: &[u8] = b"\x01";
const END: &[u8] = b"\x03";
const T1: TxnId = TxnId(0x1111);
const T2: TxnId = TxnId(0x2222);
const MAX_ROUNDS: usize = 1000;
const DOC: u8 = 1;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn put(value: &[u8]) -> IntentOp {
    IntentOp::Put { value_type: DOC, value: value.to_vec() }
}

fn record(txn: TxnId, status: TxnStatus) -> TxnRecord {
    TxnRecord {
        txn,
        lsid: 7,
        txn_number: 3,
        status,
        read_ts: ts(100),
        min_commit_ts: ts(0),
        commit_ts: ts(0),
        heartbeat_ts: ts(100),
        spans: vec![(b"\x01a".to_vec(), b"\x01b".to_vec())],
    }
}

fn latest_value(ts_: u64) -> Latest {
    Latest { ts: ts(ts_), intent: false, value: vec![DOC, b'v'] }
}

fn latest_intent(txn: TxnId, ts_: u64) -> Latest {
    let value = Intent { txn, record_key: record_key(txn), op: put(b"x") }.encode();
    Latest { ts: ts(ts_), intent: true, value }
}

#[test]
fn the_record_lives_in_the_transaction_key_space() {
    let k = record_key(TxnId(0x0102));
    assert_eq!(k[0], TXN_SPACE);
    assert_eq!(&k[1..], &0x0102u128.to_be_bytes());
    assert_eq!(succ(Hlc { physical: 5, logical: u32::MAX }), ts(6));
    assert_eq!(succ(ts(5)), Hlc { physical: 5, logical: 1 });
    assert_eq!(pred(ts(6)), Hlc { physical: 5, logical: u32::MAX });
    assert_eq!(pred(Hlc { physical: 6, logical: 2 }), Hlc { physical: 6, logical: 1 });
}

#[test]
fn records_and_intents_round_trip_and_reject_garbage() {
    let r = record(T1, TxnStatus::Committed);
    assert_eq!(TxnRecord::decode(&r.encode()).unwrap(), r);
    // A scan's row carries the record without its first byte, which it takes for the type.
    assert_eq!(TxnRecord::decode_row(&r.encode()[1..]).unwrap(), r);
    for status in [TxnStatus::Pending, TxnStatus::Committed, TxnStatus::Aborted] {
        assert_eq!(TxnStatus::from_code(status.code()), Some(status));
    }
    assert_eq!(TxnStatus::from_code(9), None);
    let mut bad = r.encode();
    bad[0] = 99;
    assert!(TxnRecord::decode(&bad).is_err(), "unknown record format");
    let mut bad = r.encode();
    bad[1 + 16 + 16 + 8] = 9;
    assert!(TxnRecord::decode(&bad).is_err(), "unknown status");
    for op in [put(b"v"), IntentOp::Delete] {
        let i = Intent { txn: T2, record_key: record_key(T2), op };
        assert_eq!(Intent::decode(&i.encode()).unwrap(), i);
    }
    let mut bad = Intent { txn: T2, record_key: vec![], op: IntentOp::Delete }.encode();
    *bad.last_mut().unwrap() = 7;
    assert!(Intent::decode(&bad).is_err(), "unknown real kind");
}

/// The key `check_put` checks.
const A_KEY: &[u8] = b"\x01a";

#[test]
fn put_intents_conflicts_follow_the_contract() {
    let read = ts(100);
    assert_eq!(check_put(A_KEY, None, T1, read), Ok(None));
    assert_eq!(check_put(A_KEY, Some(&latest_value(100)), T1, read), Ok(None));
    assert_eq!(check_put(A_KEY, Some(&latest_value(101)), T1, read), Err(Conflict::Newer(ts(101))));
    // Its own intent is replaced.
    assert_eq!(check_put(A_KEY, Some(&latest_intent(T1, 120)), T1, read), Ok(Some(ts(120))));
    // Another transaction's intent, older or newer than the read: the owner, to push.
    for at in [50, 150] {
        let owner =
            IntentOwner { txn: T2, record_key: record_key(T2), key: A_KEY.to_vec(), ts: ts(at) };
        assert_eq!(
            check_put(A_KEY, Some(&latest_intent(T2, at)), T1, read),
            Err(Conflict::Intent(owner))
        );
    }
    let corrupt = Latest { ts: ts(1), intent: true, value: vec![1] };
    assert_eq!(check_put(A_KEY, Some(&corrupt), T1, read), Err(Conflict::Corrupt));
}

fn begin() -> RecordOp {
    RecordOp::Begin { lsid: 7, txn_number: 3, read_ts: ts(100) }
}

/// The abort of session 7's transaction 3, the one [`begin`] begins.
fn abort(spans: Vec<(Vec<u8>, Vec<u8>)>) -> RecordOp {
    RecordOp::Abort { lsid: 7, txn_number: 3, spans }
}

#[test]
fn end_txn_follows_the_record_transitions() {
    let spans = vec![(b"\x01a".to_vec(), b"\x01c".to_vec())];
    let created = end_txn(None, T1, begin(), ts(110)).unwrap();
    assert_eq!(
        (created.status, created.heartbeat_ts, created.read_ts, created.lsid),
        (TxnStatus::Pending, ts(110), ts(100), 7)
    );
    let stored = created.encode();
    let beat = end_txn(Some(&stored), T1, RecordOp::Heartbeat { spans: spans.clone() }, ts(120));
    assert_eq!(
        beat.as_ref().map(|r| (r.heartbeat_ts, r.spans.clone())),
        Ok((ts(120), spans.clone()))
    );
    // A begin over its own pending record changes nothing but the heartbeat.
    assert_eq!(end_txn(Some(&stored), T1, begin(), ts(130)).unwrap().heartbeat_ts, ts(130));
    // The commit goes to the pushed minimum when it is higher.
    let pushed = TxnRecord { min_commit_ts: ts(300), ..created.clone() }.encode();
    let commit = |at| RecordOp::Commit { ts: ts(at), spans: spans.clone() };
    let c = end_txn(Some(&pushed), T1, commit(200), ts(210)).unwrap();
    assert_eq!((c.status, c.commit_ts), (TxnStatus::Committed, ts(300)));
    let c = end_txn(Some(&pushed), T1, commit(400), ts(410)).unwrap();
    assert_eq!(c.commit_ts, ts(400));
    // Committed is final: a retried commit and a late abort see it as it is.
    let committed = c.encode();
    assert_eq!(end_txn(Some(&committed), T1, commit(900), ts(910)), Ok(c.clone()));
    assert_eq!(end_txn(Some(&committed), T1, abort(vec![]), ts(910)), Ok(c));
    // Abort, also with no record; the abort is the session's last use.
    let a = end_txn(Some(&stored), T1, abort(spans.clone()), ts(140)).unwrap();
    assert_eq!((a.status, a.heartbeat_ts), (TxnStatus::Aborted, ts(140)));
    let a0 = end_txn(None, T1, abort(spans.clone()), ts(140)).unwrap();
    assert_eq!(
        (a0.status, a0.spans, a0.lsid, a0.txn_number),
        (TxnStatus::Aborted, spans.clone(), 7, 3)
    );
    // Aborted is final: a late coordinator gets NoSuchTransaction for anything but its abort.
    let aborted = a.encode();
    for op in [begin(), commit(150), RecordOp::Heartbeat { spans: vec![] }] {
        assert_eq!(end_txn(Some(&aborted), T1, op, ts(150)), Err(RecordError::NoSuchTransaction));
    }
    // A commit or heartbeat with no record never began.
    assert_eq!(end_txn(None, T1, commit(1), ts(1)), Err(RecordError::NoSuchTransaction));
    assert_eq!(
        end_txn(None, T1, RecordOp::Heartbeat { spans: vec![] }, ts(1)),
        Err(RecordError::NoSuchTransaction)
    );
    assert_eq!(end_txn(Some(b"x"), T1, begin(), ts(1)), Err(RecordError::Corrupt));
}

#[test]
fn a_coordinator_adopts_the_record_a_push_created() {
    let pushed = push_txn(None, T1, Push::To(ts(500)), ts(120)).unwrap();
    assert_eq!((pushed.status, pushed.min_commit_ts), (TxnStatus::Pending, ts(500)));
    let adopted = end_txn(Some(&pushed.encode()), T1, begin(), ts(130)).unwrap();
    assert_eq!(
        (adopted.status, adopted.min_commit_ts, adopted.lsid, adopted.read_ts),
        (TxnStatus::Pending, ts(500), 7, ts(100))
    );
    // A missing record pushed as abandoned is created aborted, and the late coordinator fails.
    // A fresh intent's missing record is created pending, counting its heartbeat from the intent:
    // the coordinator adopts it.
    let young = Push::AbortIfAbandoned { intent_ts: ts(100) };
    let waiting = push_txn(None, T1, young, ts(120)).unwrap();
    assert_eq!((waiting.status, waiting.heartbeat_ts), (TxnStatus::Pending, ts(100)));
    let adopted = end_txn(Some(&waiting.encode()), T1, begin(), ts(130)).unwrap();
    assert_eq!(adopted.status, TxnStatus::Pending);
    // Past the threshold since the intent, it is created aborted.
    let old = Push::AbortIfAbandoned { intent_ts: ts(100) };
    let aborted = push_txn(None, T1, old, ts(101 + ABANDONED_AFTER_MS)).unwrap();
    assert_eq!(aborted.status, TxnStatus::Aborted);
    assert_eq!(
        end_txn(Some(&aborted.encode()), T1, begin(), ts(130)),
        Err(RecordError::NoSuchTransaction)
    );
}

/// A record a push created before the coordinator's `Begin` landed has no session. When the
/// `Begin` never lands, the coordinator's abort gives the record its session and its mark, so that
/// record GC keeps it as long as the session (`txn-bank` 996, `heavy`).
#[test]
fn the_abort_gives_a_pushed_record_its_session_and_its_last_use() {
    let spans = vec![(b"\x01a".to_vec(), b"\x01a\x00".to_vec())];
    let waiting = push_txn(None, T1, Push::To(ts(500)), ts(120)).unwrap();
    assert_eq!((waiting.lsid, waiting.txn_number, waiting.read_ts), (0, 0, ts(0)));
    let a = end_txn(Some(&waiting.encode()), T1, abort(spans.clone()), ts(900)).unwrap();
    assert_eq!(
        (a.status, a.lsid, a.txn_number, a.heartbeat_ts, a.spans.clone()),
        (TxnStatus::Aborted, 7, 3, ts(900), spans.clone())
    );
    // Aborted by a push first: the late abort still gives it the session and its mark, and keeps
    // what the record says.
    let old = Push::AbortIfAbandoned { intent_ts: ts(100) };
    let pushed = push_txn(None, T1, old, ts(101 + ABANDONED_AFTER_MS)).unwrap();
    assert_eq!((pushed.status, pushed.lsid), (TxnStatus::Aborted, 0));
    let late = end_txn(Some(&pushed.encode()), T1, abort(spans), ts(9_000)).unwrap();
    assert_eq!(
        late,
        TxnRecord { lsid: 7, txn_number: 3, heartbeat_ts: ts(9_000), ..pushed.clone() }
    );
    // A mark below the record's last use does not take it back; a session it has stays.
    let earlier = end_txn(Some(&late.encode()), T1, abort(vec![]), ts(50)).unwrap();
    assert_eq!(earlier, late);
    let other = RecordOp::Abort { lsid: 8, txn_number: 1, spans: vec![] };
    assert_eq!(end_txn(Some(&late.encode()), T1, other, ts(9_500)).unwrap().lsid, 7);
    // An internal transaction's abort (no session) leaves the session as it is.
    let internal = RecordOp::Abort { lsid: 0, txn_number: 0, spans: vec![] };
    let a = end_txn(Some(&pushed.encode()), T1, internal, ts(9_000)).unwrap();
    assert_eq!((a.lsid, a.heartbeat_ts), (0, ts(9_000)));
}

#[test]
fn pushes_raise_the_minimum_or_abort_the_abandoned() {
    let live = record(T1, TxnStatus::Pending);
    let stored = live.encode();
    let up = push_txn(Some(&stored), T1, Push::To(ts(200)), ts(150)).unwrap();
    assert_eq!(up.min_commit_ts, ts(200));
    let higher = TxnRecord { min_commit_ts: ts(900), ..live.clone() }.encode();
    assert_eq!(
        push_txn(Some(&higher), T1, Push::To(ts(200)), ts(150)).unwrap().min_commit_ts,
        ts(900)
    );
    // Heartbeat at 100: alive until 100 + 5 s.
    let edge = ts(100 + ABANDONED_AFTER_MS);
    assert_eq!(
        push_txn(Some(&stored), T1, Push::AbortIfAbandoned { intent_ts: ts(0) }, edge),
        Ok(live.clone())
    );
    let late = ts(101 + ABANDONED_AFTER_MS);
    let gone =
        push_txn(Some(&stored), T1, Push::AbortIfAbandoned { intent_ts: ts(0) }, late).unwrap();
    assert_eq!(gone.status, TxnStatus::Aborted);
    assert!(live.abandoned_at(late) && !gone.abandoned_at(late));
    // Final records do not move.
    for status in [TxnStatus::Committed, TxnStatus::Aborted] {
        let r = record(T1, status);
        for p in [Push::To(ts(999)), Push::AbortIfAbandoned { intent_ts: ts(0) }] {
            assert_eq!(push_txn(Some(&r.encode()), T1, p, late), Ok(r.clone()));
        }
    }
    assert_eq!(push_txn(Some(b""), T1, Push::To(ts(1)), ts(1)), Err(RecordError::Corrupt));
}

/// A recovery of a client transaction's commit on another node (008) aborts its record: a running
/// one at once, whatever its heartbeat; a final one stays as it is; and one that does not exist is
/// created aborted, so that the transaction's coordinator, slow, cannot begin or commit it after.
#[test]
fn a_recovery_push_aborts_a_running_record_and_fences_a_missing_one() {
    let live = record(T1, TxnStatus::Pending);
    let at = ts(150);
    let recover = Push::Abort { lsid: 7, txn_number: 3 };
    let aborted = push_txn(Some(&live.encode()), T1, recover, at).unwrap();
    assert_eq!(aborted, TxnRecord { status: TxnStatus::Aborted, ..live.clone() });
    for status in [TxnStatus::Committed, TxnStatus::Aborted] {
        let r = record(T1, status);
        assert_eq!(push_txn(Some(&r.encode()), T1, recover, at), Ok(r.clone()));
    }
    // The fence carries its transaction's session, so that it lives as long as one (008).
    let fence = push_txn(None, T2, recover, at).unwrap();
    assert_eq!(
        (fence.txn, fence.status, fence.lsid, fence.txn_number),
        (T2, TxnStatus::Aborted, 7, 3)
    );
    assert_eq!(fence.read_ts, ts(0), "never begun");
    let begin = RecordOp::Begin { lsid: 7, txn_number: 1, read_ts: ts(100) };
    assert_eq!(end_txn(Some(&fence.encode()), T2, begin, at), Err(RecordError::NoSuchTransaction));
    let commit = RecordOp::Commit { ts: ts(160), spans: Vec::new() };
    assert_eq!(end_txn(Some(&fence.encode()), T2, commit, at), Err(RecordError::NoSuchTransaction));
}

/// A final record whose intents are all resolved forgets its intervals (008, record GC): only
/// then may it go; a running one, or one that does not exist, refuses.
#[test]
fn a_final_record_forgets_its_intervals_once_resolved() {
    let at = ts(150);
    for status in [TxnStatus::Committed, TxnStatus::Aborted] {
        let r = record(T1, status);
        assert!(!r.spans.is_empty(), "the test's record keeps intervals");
        let done = end_txn(Some(&r.encode()), T1, RecordOp::Resolved, at).unwrap();
        assert_eq!(done, TxnRecord { spans: Vec::new(), ..r });
    }
    let live = record(T1, TxnStatus::Pending);
    let refused = end_txn(Some(&live.encode()), T1, RecordOp::Resolved, at);
    assert_eq!(refused, Err(RecordError::NoSuchTransaction));
    assert_eq!(end_txn(None, T1, RecordOp::Resolved, at), Err(RecordError::NoSuchTransaction));
}

#[test]
fn transaction_commands_round_trip() {
    let commands = [
        Command::PutIntents {
            txn: T1,
            record_key: record_key(T1),
            intents: vec![
                IntentPut { key: b"\x01a".to_vec(), op: put(b"v"), replaces: None },
                IntentPut { key: b"\x01b".to_vec(), op: IntentOp::Delete, replaces: Some(ts(4)) },
            ],
        },
        Command::EndTxn { record: record(T1, TxnStatus::Committed) },
        Command::PushTxn { record: record(T2, TxnStatus::Pending) },
        Command::ResolveIntents {
            txn: T1,
            status: TxnStatus::Committed,
            commit_ts: ts(9),
            resolutions: vec![
                Resolution { key: b"\x01a".to_vec(), intent_ts: ts(5), commit: Some(put(b"v")) },
                Resolution { key: b"\x01b".to_vec(), intent_ts: ts(5), commit: None },
            ],
            live_delta: -12,
        },
    ];
    for (c, kind) in commands.iter().zip([8, 9, 12, 10]) {
        assert_eq!(c.kind(), kind);
        let bytes = c.encode();
        assert_eq!(Command::decode(&bytes).unwrap(), *c);
        assert!(Command::decode(&bytes[..bytes.len() - 1]).is_err());
    }
    let mut bad = commands[3].encode();
    bad[2 + 16] = 7;
    assert!(Command::decode(&bad).is_err(), "unknown status");
}

fn cfg() -> LsmConfig {
    LsmConfig { dir: "core-00/lsm".into(), memtable_bytes: 32 << 10, ..LsmConfig::default() }
}

struct Replica {
    disk: MemDisk,
    lsm: Rc<RefCell<Lsm>>,
    sm: RangeSm,
}

impl Replica {
    fn new() -> Replica {
        let mut disk = MemDisk::new();
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        Replica { disk, lsm, sm }
    }

    fn restart(self) -> Replica {
        let Replica { mut disk, lsm, .. } = self;
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
        drop(lsm);
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        Replica { disk, lsm, sm }
    }

    fn apply(&mut self, index: u64, at: u64, c: &Command) -> Result<(), String> {
        let ctx = ApplyCtx { group: G, index, term: 1, ts: ts(at) };
        self.sm.apply(&c.encode(), &ctx).map(|_| ()).map_err(|e| e.0)
    }

    /// The visible version of `key` at `at`: its kind and value.
    fn get(&mut self, key: &[u8], at: u64) -> Option<(Kind, Vec<u8>)> {
        let view = self.lsm.borrow().view();
        for _ in 0..MAX_ROUNDS {
            let found = view.get(&mut self.lsm.borrow_mut(), key, ts(at));
            match found {
                Lookup::Hit(f) => return Some((f.kind, f.value.to_vec())),
                Lookup::NotFound => return None,
                Lookup::Failed(e) => panic!("{e}"),
                Lookup::Miss(_) => {
                    self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                }
            }
        }
        panic!("get never finished")
    }
}

fn intents_of(txn: TxnId, puts: &[(&[u8], IntentOp, Option<u64>)]) -> Command {
    Command::PutIntents {
        txn,
        record_key: record_key(txn),
        intents: puts
            .iter()
            .map(|(k, op, r)| IntentPut { key: k.to_vec(), op: op.clone(), replaces: r.map(ts) })
            .collect(),
    }
}

#[test]
fn a_replica_applies_intents_records_and_resolutions_and_keeps_them_across_a_restart() {
    let mut r = Replica::new();
    let write = Command::Write {
        puts: vec![teseladb_range::command::Put {
            key: b"\x01a".to_vec(),
            value_type: DOC,
            value: b"old".to_vec(),
        }],
        deletes: vec![],
        dedup: None,
        live_delta: 6,
    };
    r.apply(1, 10, &write).unwrap();
    let c = intents_of(T1, &[(b"\x01a", put(b"new"), None), (b"\x01b", IntentOp::Delete, None)]);
    r.apply(2, 20, &c).unwrap();
    // The transaction writes `a` again: the first intent goes.
    r.apply(3, 25, &intents_of(T1, &[(b"\x01a", put(b"newer"), Some(20))])).unwrap();
    let begun = end_txn(None, T1, begin(), ts(21)).unwrap();
    r.apply(4, 30, &Command::EndTxn { record: begun }).unwrap();
    let mut r = r.restart();
    let (kind, payload) = r.get(b"\x01a", 40).unwrap();
    assert_eq!(kind, Kind::Intent);
    assert_eq!(Intent::decode(&payload).unwrap().op, put(b"newer"));
    assert_eq!(r.get(b"\x01a", 22).unwrap(), (Kind::Put, b"\x01old".to_vec()), "intent at 20 gone");
    let stored = r.get(&record_key(T1), 40).unwrap().1;
    assert_eq!(TxnRecord::decode(&stored).unwrap().status, TxnStatus::Pending);
    // Committed at 50: `a` at its commit, `b` (never written before) stays absent.
    let committed =
        end_txn(Some(&stored), T1, RecordOp::Commit { ts: ts(50), spans: vec![] }, ts(50));
    r.apply(5, 50, &Command::EndTxn { record: committed.unwrap() }).unwrap();
    let resolve = Command::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: ts(50),
        resolutions: vec![
            Resolution { key: b"\x01a".to_vec(), intent_ts: ts(25), commit: Some(put(b"newer")) },
            Resolution {
                key: b"\x01b".to_vec(),
                intent_ts: ts(20),
                commit: Some(IntentOp::Delete),
            },
        ],
        live_delta: 2,
    };
    r.apply(6, 55, &resolve).unwrap();
    let mut r = r.restart();
    assert_eq!(r.get(b"\x01a", 49).unwrap(), (Kind::Put, b"\x01old".to_vec()));
    assert_eq!(r.get(b"\x01a", 50).unwrap(), (Kind::Put, b"\x01newer".to_vec()));
    assert_eq!(r.get(b"\x01b", 60), None);
    assert_eq!(r.sm.live_bytes(), 8);
    assert_eq!(r.sm.last_mark(), ts(55));
    assert_eq!(r.get(&last_mark_key(G), u64::MAX).map(|v| v.1.len()), Some(12));
    assert_eq!(r.get(&live_key(G), u64::MAX).unwrap().1, 8u64.to_be_bytes().to_vec());
    // An aborted transaction's intent is dropped; a commit at the intent's own mark replaces it.
    r.apply(7, 60, &intents_of(T2, &[(b"\x01c", put(b"t2"), None)])).unwrap();
    r.apply(8, 61, &intents_of(T1, &[(b"\x01d", put(b"d"), None)])).unwrap();
    let abort = Command::ResolveIntents {
        txn: T2,
        status: TxnStatus::Aborted,
        commit_ts: ts(0),
        resolutions: vec![Resolution { key: b"\x01c".to_vec(), intent_ts: ts(60), commit: None }],
        live_delta: 0,
    };
    r.apply(9, 62, &abort).unwrap();
    let same = Command::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: ts(61),
        resolutions: vec![Resolution {
            key: b"\x01d".to_vec(),
            intent_ts: ts(61),
            commit: Some(put(b"d")),
        }],
        live_delta: 4,
    };
    r.apply(10, 63, &same).unwrap();
    let mut r = r.restart();
    assert_eq!(r.get(b"\x01c", 70), None);
    assert_eq!(r.get(b"\x01d", 61).unwrap(), (Kind::Put, b"\x01d".to_vec()));
    // A push writes its record too.
    let pushed = push_txn(None, T2, Push::AbortIfAbandoned { intent_ts: ts(60) }, ts(64)).unwrap();
    r.apply(11, 64, &Command::PushTxn { record: pushed.clone() }).unwrap();
    assert_eq!(TxnRecord::decode(&r.get(&record_key(T2), 70).unwrap().1).unwrap(), pushed);
}

/// After a restart the core restores the replica's own snapshot, which may be older than what
/// its LSM flushed, and the log goes on from that snapshot. The entries the LSM holds are not
/// written again: an intent written again over the commit that replaced it at the same mark
/// hides that commit until its resolution comes back, and for good if the node stops in between
/// with the intent flushed. Seed 883 of the simulator's `indexes` (plan `heavy`): a replica
/// restarted from a snapshot 38 entries behind its LSM, flushed the intent of a deletion of an
/// index entry again and stopped before the resolution; after the next restart it led the range
/// and a check read the entry the transaction had deleted.
#[test]
fn a_restart_from_an_older_snapshot_does_not_write_again_what_the_lsm_holds() {
    let a = b"\x01a";
    let mut r = Replica::new();
    let write = Command::Write {
        puts: vec![teseladb_range::command::Put {
            key: a.to_vec(),
            value_type: DOC,
            value: b"old".to_vec(),
        }],
        deletes: vec![],
        dedup: None,
        live_delta: 6,
    };
    r.apply(1, 10, &write).unwrap();
    let snap = r.sm.snapshot();
    let intent = intents_of(T1, &[(a, IntentOp::Delete, None)]);
    let resolve = Command::ResolveIntents {
        txn: T1,
        status: TxnStatus::Committed,
        commit_ts: ts(20),
        resolutions: vec![Resolution {
            key: a.to_vec(),
            intent_ts: ts(20),
            commit: Some(IntentOp::Delete),
        }],
        live_delta: -6,
    };
    r.apply(2, 20, &intent).unwrap();
    r.apply(3, 21, &resolve).unwrap();
    assert_eq!(r.get(a, 30), None, "deleted at 20");
    let mut r = r.restart();
    r.sm.restore(&snap).unwrap();
    // The log goes on from the snapshot: entry 2 comes first.
    r.apply(2, 20, &intent).unwrap();
    assert_eq!(r.get(a, 30), None, "the commit's deletion, not the intent written again");
    // The node stops before entry 3 comes again.
    let mut r = r.restart();
    assert_eq!(r.get(a, 30), None, "no intent flushed over the deletion");
    r.sm.restore(&snap).unwrap();
    r.apply(2, 20, &intent).unwrap();
    r.apply(3, 21, &resolve).unwrap();
    assert_eq!(r.get(a, 30), None);
    assert_eq!(r.get(a, 19).unwrap(), (Kind::Put, b"\x01old".to_vec()));
    assert_eq!(r.sm.live_bytes(), 0);
    assert_eq!(r.sm.last_mark(), ts(21));
}

#[test]
fn entries_that_break_the_rules_stop_the_group() {
    let mut r = Replica::new();
    // A key outside the range.
    let out = intents_of(T1, &[(b"\x05x", put(b"v"), None)]);
    assert!(r.apply(1, 10, &out).unwrap_err().contains("outside the range"));
    // Resolutions that do not match the transaction's state.
    let pending = Command::ResolveIntents {
        txn: T1,
        status: TxnStatus::Pending,
        commit_ts: ts(1),
        resolutions: vec![],
        live_delta: 0,
    };
    assert!(r.apply(1, 10, &pending).unwrap_err().contains("do not match"));
    let aborted_with_commit = Command::ResolveIntents {
        txn: T1,
        status: TxnStatus::Aborted,
        commit_ts: ts(1),
        resolutions: vec![Resolution {
            key: b"\x01a".to_vec(),
            intent_ts: ts(1),
            commit: Some(put(b"v")),
        }],
        live_delta: 0,
    };
    assert!(r.apply(1, 10, &aborted_with_commit).unwrap_err().contains("do not match"));
    // A frozen range writes nothing.
    r.apply(1, 10, &Command::MergeFreeze { merge: 4 }).unwrap();
    let c = intents_of(T1, &[(b"\x01a", put(b"v"), None)]);
    assert!(r.apply(2, 11, &c).unwrap_err().contains("frozen"));
    let rec = Command::EndTxn { record: record(T1, TxnStatus::Pending) };
    assert!(r.apply(2, 11, &rec).unwrap_err().contains("frozen"));
}

#[test]
fn a_record_outside_the_range_stops_the_group() {
    let mut disk = MemDisk::new();
    let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
    let mut sm = RangeSm::open(G, b"\x01".to_vec(), b"\x02".to_vec(), lsm, &mut disk).unwrap();
    let ctx = ApplyCtx { group: G, index: 1, term: 1, ts: ts(5) };
    let c = Command::EndTxn { record: record(T1, TxnStatus::Pending) };
    assert!(sm.apply(&c.encode(), &ctx).unwrap_err().0.contains("outside the range"));
}
