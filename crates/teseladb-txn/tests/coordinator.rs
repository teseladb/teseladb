//! The coordinator of a transaction (008 T006, FR-001, FR-004, FR-005): the record created with
//! the first write, the heartbeat that keeps it alive, the commit across two ranges with the
//! intents resolved after it, the abort, and a coordinator that dies and whose transaction a
//! writer aborts. One node with two ranges, driven as an embedder would.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::{
    RangeError, RangeHost, ReadSpec, Reply, ReqId, SplitDone, TxnOp, TxnReq,
};
use teseladb_range::intents::{
    IntentOp, RecordOp, TXN_SPACE, TxnId, TxnRecord, TxnStatus, record_key,
};
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::{RangeSm, RangeState};
use teseladb_txn::coordinator::{Coordinator, Event, Outcome, ReqNo, Route, StmtId, TxnError};
use teseladb_txn::reaper::Reaper;
use teseladb_txn::record::{HEARTBEAT_EVERY_MS, Spans};
use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G1: GroupId = GroupId(7);
const G2: GroupId = GroupId(8);
const CUT: &[u8] = b"\x01m";
const WALL: u64 = 1_800_000_000_000;
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
const DOC: u8 = 1;
const A: &[u8] = b"\x01a";
const Z: &[u8] = b"\x01z";

/// The node's routing table: each range by its first key. It starts with keys below `\x01m` in
/// G1 and the rest (transaction records too) in G2, and learns the ranges splits make.
struct Ranges(Vec<(Vec<u8>, GroupId)>);

impl Ranges {
    fn two() -> Ranges {
        Ranges(vec![(b"\x01".to_vec(), G1), (CUT.to_vec(), G2)])
    }

    fn learn(&mut self, start: &[u8], g: GroupId) {
        self.0.push((start.to_vec(), g));
        self.0.sort();
    }
}

impl Route for Ranges {
    fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        self.0.iter().rev().find(|(start, _)| start.as_slice() <= key).map(|(_, g)| *g)
    }
}

struct Node {
    disk: MemDisk,
    core: MultiRaftCore<RangeSm>,
    host: RangeHost,
    route: Ranges,
    now: Nanos,
    replies: Vec<(ReqId, Result<Reply, RangeError>)>,
    /// Coordinators by name, and which of their requests each host request carries.
    coords: BTreeMap<u32, Coordinator>,
    sent: BTreeMap<ReqId, (u32, ReqNo)>,
    events: BTreeMap<u32, Vec<Event>>,
}

impl Node {
    fn new() -> Node {
        let mut disk = MemDisk::new();
        let clock = NodeClock::for_node(NodeId(1));
        let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
        let cfg = CoreConfig { clock: clock.clone(), ..CoreConfig::new(NodeId(1), 0) };
        let mut core = MultiRaftCore::new(cfg, store, 1);
        let lcfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
        let mut host = RangeHost::new(lsm.clone(), clock);
        for (g, start, end) in [(G1, b"\x01".as_slice(), CUT), (G2, CUT, b"\x03".as_slice())] {
            let sm =
                RangeSm::open(g, start.to_vec(), end.to_vec(), lsm.clone(), &mut disk).unwrap();
            let desc = GroupDesc { group: g, voters: vec![NodeId(1)], learners: vec![] };
            core.create_group(desc, sm).unwrap();
            host.add_range(g, start.to_vec(), end.to_vec());
        }
        let mut n = Node {
            disk,
            core,
            host,
            route: Ranges::two(),
            now: Nanos::ZERO,
            replies: vec![],
            coords: BTreeMap::new(),
            sent: BTreeMap::new(),
            events: BTreeMap::new(),
        };
        n.settle();
        n
    }

    fn wall(&self) -> u64 {
        WALL + self.now.0 / 1_000_000
    }

    fn clock(&self) -> Hlc {
        Hlc { physical: self.wall(), logical: 0 }
    }

    /// Runs the node and the coordinators until nothing moves.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            let mut moved = false;
            self.now = self.now + Nanos::from_millis(1);
            let (now, wall) = (self.now, self.wall());
            self.core.on_tick(now, wall);
            while let Some(op) = self.core.poll_io(now) {
                let done = teseladb_raftlog::Disk::execute(&mut self.disk, op);
                self.core.on_io_done(done, now);
                moved = true;
            }
            let c = self.host.lsm().borrow_mut().run_io(&mut self.disk, now);
            moved |= !c.flushed.is_empty();
            for (op, r) in self.core.drain_actions().done {
                moved |= self.host.on_done(&mut self.core, op, r);
            }
            moved |= self.host.poll(&mut self.core, now, wall);
            moved |= self.pump();
            if !moved {
                return;
            }
        }
        panic!("the node never settled");
    }

    /// Moves requests from the coordinators to the host and answers back.
    fn pump(&mut self) -> bool {
        let mut moved = false;
        for (id, r) in self.host.take_replies() {
            moved = true;
            match self.sent.remove(&id) {
                Some((name, no)) => {
                    let c = self.coords.get_mut(&name).expect("a coordinator");
                    c.on_reply(&self.route, no, r);
                }
                None => self.replies.push((id, r)),
            }
        }
        let names: Vec<u32> = self.coords.keys().copied().collect();
        for name in names {
            let c = self.coords.get_mut(&name).expect("a coordinator");
            let reqs = c.take_requests();
            let events = c.take_events();
            moved |= !reqs.is_empty() || !events.is_empty();
            self.events.entry(name).or_default().extend(events);
            for r in reqs {
                let (now, wall) = (self.now, self.wall());
                let req = TxnReq { g: r.g, op: r.op, wc: MAJORITY };
                let id = self.host.txn(&mut self.core, req, now, wall);
                self.sent.insert(id, (name, r.id));
            }
        }
        moved
    }

    /// Lets `ms` milliseconds go by, ticking only the coordinators in `alive`.
    fn advance(&mut self, ms: u64, alive: &[u32]) {
        for _ in 0..ms / 100 {
            self.now = self.now + Nanos::from_millis(100);
            for name in alive {
                let (now, wall) = (self.now, self.wall());
                self.coords.get_mut(name).expect("a coordinator").tick(&self.route, now, wall);
            }
            self.settle();
        }
    }

    fn begin(&mut self, name: u32, txn: TxnId) {
        let c = Coordinator::new(txn, (u128::from(name), 1), self.clock(), self.now);
        self.coords.insert(name, c);
    }

    fn write(&mut self, name: u32, writes: &[(&[u8], IntentOp)]) -> Result<(), TxnError> {
        let writes = writes.iter().map(|(k, op)| (k.to_vec(), op.clone())).collect();
        let c = self.coords.get_mut(&name).expect("a coordinator");
        let stmt = c.write(&self.route, writes)?;
        self.settle();
        self.written(name, stmt)
    }

    fn written(&mut self, name: u32, stmt: StmtId) -> Result<(), TxnError> {
        let events = self.events.entry(name).or_default();
        let at = events
            .iter()
            .position(|e| matches!(e, Event::Written { stmt: s, .. } if *s == stmt))
            .expect("the statement was answered");
        match events.remove(at) {
            Event::Written { result, .. } => result,
            other => panic!("not a statement's answer: {other:?}"),
        }
    }

    fn commit(&mut self, name: u32) -> Outcome {
        let clock = self.clock();
        self.coords.get_mut(&name).expect("a coordinator").commit(clock);
        self.settle();
        self.clock_wait(name);
        self.ended(name)
    }

    /// Lets the clock reach the mark of a commit that waits for it (FR-008), then settles.
    fn clock_wait(&mut self, name: u32) {
        while let Some(ms) = self.coords.get(&name).and_then(Coordinator::clock_wait) {
            while self.wall() < ms {
                self.now = self.now + Nanos::from_millis(1);
            }
            let (now, wall) = (self.now, self.wall());
            self.coords.get_mut(&name).expect("a coordinator").tick(&self.route, now, wall);
            self.settle();
        }
    }

    fn abort(&mut self, name: u32) -> Outcome {
        self.coords.get_mut(&name).expect("a coordinator").abort();
        self.settle();
        self.ended(name)
    }

    fn ended(&mut self, name: u32) -> Outcome {
        let events = self.events.entry(name).or_default();
        let at = events.iter().position(|e| matches!(e, Event::Ended(_))).expect("it ended");
        match events.remove(at) {
            Event::Ended(o) => o,
            other => panic!("not an end: {other:?}"),
        }
    }

    fn resolved(&self, name: u32) -> bool {
        self.events.get(&name).is_some_and(|e| e.contains(&Event::Resolved))
    }

    fn get(&mut self, key: &[u8]) -> Option<(u8, Vec<u8>)> {
        let (now, wall) = (self.now, self.wall());
        let g = self.route.range_of(key).unwrap();
        let id = self.host.get(&mut self.core, g, key, ReadConcern::Majority, now, wall);
        self.settle();
        let at = self.replies.iter().position(|(r, _)| *r == id).expect("answered");
        match self.replies.remove(at).1 {
            Ok(Reply::Value { value, .. }) => value,
            other => panic!("not a value: {other:?}"),
        }
    }

    /// Sends `op` to its range and waits for the answer.
    fn ask(&mut self, g: GroupId, op: TxnOp) -> Result<Reply, RangeError> {
        let (now, wall) = (self.now, self.wall());
        let id = self.host.txn(&mut self.core, TxnReq { g, op, wc: MAJORITY }, now, wall);
        self.settle();
        let at = self.replies.iter().position(|(r, _)| *r == id).expect("answered");
        self.replies.remove(at).1
    }

    /// One pass of `reaper` over the records, its requests run until it is idle.
    fn reap(&mut self, reaper: &mut Reaper) {
        let (now, wall) = (self.now, self.wall());
        let spec = ReadSpec {
            g: G2,
            key: vec![TXN_SPACE],
            scan: Some((vec![TXN_SPACE + 1], usize::MAX)),
            backward: false,
            at: None,
            concern: ReadConcern::Local,
            view: Default::default(),
            uncertain_to: None,
        };
        let id = self.host.read_with(&mut self.core, spec, now, wall);
        self.settle();
        let at = self.replies.iter().position(|(r, _)| *r == id).expect("answered");
        let Ok(Reply::Batch(rows)) = self.replies.remove(at).1 else { panic!("no records") };
        let records = rows.iter().filter_map(|(_, (_, v))| TxnRecord::decode_row(v).ok());
        reaper.scan(&self.route, self.clock(), records);
        loop {
            let reqs = reaper.take_requests();
            if reqs.is_empty() {
                return;
            }
            for q in reqs {
                let answer = self.ask(q.g, q.op);
                reaper.on_reply(&self.route, q.id, answer);
            }
        }
    }

    fn record(&mut self, txn: TxnId) -> TxnRecord {
        let stored = self.get(&record_key(txn)).expect("a record");
        TxnRecord::decode(&[&[stored.0][..], &stored.1].concat()).expect("decodes")
    }
}

fn put(v: &[u8]) -> IntentOp {
    IntentOp::Put { value_type: DOC, value: v.to_vec() }
}

fn doc(v: &[u8]) -> Option<(u8, Vec<u8>)> {
    Some((DOC, v.to_vec()))
}

#[test]
fn a_transaction_commits_across_two_ranges_and_resolves_its_intents() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    assert_eq!(n.coords[&1].session(), (1, 1));
    assert!(!n.coords[&1].written(), "nothing written: another read mark may replace it");
    assert_eq!(n.write(1, &[(A, put(b"a1")), (Z, put(b"z1"))]), Ok(()));
    assert_eq!(n.write(1, &[(A, put(b"a2"))]), Ok(()), "its own key again");
    assert!(n.coords[&1].written());
    assert_eq!(n.coords[&1].written_ranges().into_iter().collect::<Vec<_>>(), [G1, G2]);
    let intents = n.coords[&1].intent_max();
    let Outcome::Committed(ts) = n.commit(1) else { panic!("not committed") };
    assert!(ts >= intents, "the commit is at or above its intents");
    assert!(n.resolved(1));
    assert_eq!(n.coords[&1].unresolved(), 0);
    assert_eq!(n.get(A), doc(b"a2"));
    assert_eq!(n.get(Z), doc(b"z1"));
    let rec = n.record(TxnId(1));
    assert_eq!((rec.status, rec.commit_ts, rec.lsid), (TxnStatus::Committed, ts, 1));
    assert_eq!(rec.spans, vec![(A.to_vec(), [A, &[0]].concat()), (Z.to_vec(), [Z, &[0]].concat())]);
    // A finished transaction takes no more statements; committing again says the same.
    assert_eq!(n.write(1, &[(A, put(b"x"))]), Err(TxnError::NoSuchTransaction));
    assert_eq!(n.commit(1), Outcome::Committed(ts));
    assert_eq!(n.abort(1), Outcome::Committed(ts));
}

#[test]
fn an_aborted_transaction_leaves_nothing() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    n.write(1, &[(A, put(b"a1")), (Z, IntentOp::Delete)]).unwrap();
    assert_eq!(n.abort(1), Outcome::Aborted);
    assert!(n.resolved(1));
    assert_eq!(n.get(A), None);
    assert_eq!(n.record(TxnId(1)).status, TxnStatus::Aborted);
    assert_eq!(n.abort(1), Outcome::Aborted);
    assert_eq!(n.commit(1), Outcome::Failed(TxnError::NoSuchTransaction));
    // Without writes there is no record.
    n.begin(2, TxnId(2));
    assert_eq!(n.abort(2), Outcome::Aborted);
    n.begin(3, TxnId(3));
    let read = n.coords[&3].read_ts();
    assert_eq!(n.commit(3), Outcome::Committed(read));
    assert!(n.resolved(3));
}

#[test]
fn the_heartbeat_keeps_a_slow_transaction_alive() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    n.write(1, &[(A, put(b"a1"))]).unwrap();
    let first = n.record(TxnId(1)).heartbeat_ts;
    n.advance(3 * HEARTBEAT_EVERY_MS + 7_000, &[1]);
    assert!(n.record(TxnId(1)).heartbeat_ts.physical >= first.physical + 7_000);
    // Another transaction runs into its intent: it is alive, so that one gets WriteConflict.
    n.begin(2, TxnId(2));
    assert_eq!(n.write(2, &[(A, put(b"other"))]), Err(TxnError::WriteConflict));
    assert!(matches!(n.commit(1), Outcome::Committed(_)));
    assert_eq!(n.get(A), doc(b"a1"));
}

#[test]
fn a_dead_coordinators_transaction_is_aborted_by_a_writer() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    n.write(1, &[(A, put(b"dead")), (Z, put(b"dead"))]).unwrap();
    // Its coordinator stops: no heartbeat for more than five seconds.
    n.advance(6_000, &[]);
    n.begin(2, TxnId(2));
    assert_eq!(n.write(2, &[(A, put(b"alive"))]), Ok(()));
    assert_eq!(n.record(TxnId(1)).status, TxnStatus::Aborted);
    assert!(matches!(n.commit(2), Outcome::Committed(_)));
    assert_eq!(n.get(A), doc(b"alive"));
    // The late coordinator comes back: its commit fails and its other intent goes.
    assert_eq!(n.commit(1), Outcome::Failed(TxnError::NoSuchTransaction));
    assert!(n.resolved(1));
    assert_eq!(n.get(Z), None);
}

#[test]
fn a_write_finds_a_newer_version_and_fails_with_write_conflict() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    n.begin(2, TxnId(2));
    n.write(2, &[(A, put(b"first"))]).unwrap();
    assert!(matches!(n.commit(2), Outcome::Committed(_)));
    // Transaction 1 read before that commit.
    assert_eq!(n.write(1, &[(A, put(b"late"))]), Err(TxnError::WriteConflict));
    assert_eq!(n.abort(1), Outcome::Aborted);
    assert_eq!(n.get(A), doc(b"first"));
}

impl Node {
    fn write_and_commit(&mut self, name: u32, writes: &[(&[u8], IntentOp)]) -> Outcome {
        let writes = writes.iter().map(|(k, op)| (k.to_vec(), op.clone())).collect();
        let clock = self.clock();
        let c = self.coords.get_mut(&name).expect("a coordinator");
        c.write_and_commit(&self.route, writes, clock);
        self.settle();
        self.clock_wait(name);
        self.ended(name)
    }
}

#[test]
fn a_single_batch_in_one_range_commits_in_one_phase() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    let Outcome::Committed(ts) = n.write_and_commit(1, &[(A, put(b"a")), (b"\x01b", put(b"b"))])
    else {
        panic!("not committed")
    };
    assert!(ts > n.coords[&1].read_ts());
    assert!(n.resolved(1));
    assert_eq!(n.get(A), doc(b"a"));
    // No record: the transaction never needed one.
    assert_eq!(n.get(&record_key(TxnId(1))), None);
    // Across two ranges it writes intents and commits with its record.
    n.begin(2, TxnId(2));
    assert!(matches!(
        n.write_and_commit(2, &[(A, put(b"a2")), (Z, put(b"z2"))]),
        Outcome::Committed(_)
    ));
    assert_eq!((n.get(A), n.get(Z)), (doc(b"a2"), doc(b"z2")));
    assert_eq!(n.record(TxnId(2)).status, TxnStatus::Committed);
    // A conflict in one phase fails the commit; nothing is written.
    n.begin(3, TxnId(3));
    n.begin(4, TxnId(4));
    n.write(4, &[(A, put(b"held"))]).unwrap();
    assert_eq!(n.write_and_commit(3, &[(A, put(b"a3"))]), Outcome::Failed(TxnError::WriteConflict));
    assert_eq!(n.abort(4), Outcome::Aborted);
    assert_eq!(n.get(A), doc(b"a2"));
}

const G3: GroupId = GroupId(9);
const G4: GroupId = GroupId(10);
/// Where G1 splits: `\x01h` and `\x01i` go to G3.
const AT: &[u8] = b"\x01f";
const H: &[u8] = b"\x01h";
const I: &[u8] = b"\x01i";
/// Where G2 splits: the transaction records go to G4.
const RECORDS: &[u8] = b"\x02";

impl Node {
    /// Completes the split of range `g` at `at` into `new_range` asked of its leader, as Group 0
    /// would, without telling the routing table: requests sent there meanwhile get `OutOfRange`.
    fn split(&mut self, g: GroupId, at: &[u8], new_range: GroupId) {
        let end = self.core.state_machine(g).and_then(RangeState::end).expect("an end").to_vec();
        self.settle();
        let done = self.host.take_splits();
        assert_eq!(done, [SplitDone { g, at: at.to_vec(), new_range }]);
        let at_index = self.core.status(g).expect("a range").applied;
        let lsm = self.host.lsm().clone();
        let sm = RangeSm::open(new_range, at.to_vec(), end.clone(), lsm, &mut self.disk).unwrap();
        let desc = GroupDesc { group: new_range, voters: vec![NodeId(1)], learners: vec![] };
        self.core.create_group(desc, sm).unwrap();
        self.host.add_born_range(new_range, at.to_vec(), end, g, at_index);
        self.settle();
    }

    /// Sends a statement without waiting for its answer.
    fn send_write(&mut self, name: u32, writes: &[(&[u8], IntentOp)]) -> StmtId {
        let writes = writes.iter().map(|(k, op)| (k.to_vec(), op.clone())).collect();
        let c = self.coords.get_mut(&name).expect("a coordinator");
        c.write(&self.route, writes).expect("sent")
    }
}

#[test]
fn a_split_in_the_middle_of_a_transaction_sends_its_requests_to_the_new_ranges() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    assert_eq!(n.write(1, &[(H, put(b"h1")), (A, put(b"a1"))]), Ok(()));
    // G1 splits while a statement goes to it: the range refuses it with `OutOfRange` and the
    // coordinator waits for its node to learn the new range, then sends it there.
    assert!(n.host.split(G1, AT, G3));
    let stmt = n.send_write(1, &[(I, put(b"i1"))]);
    n.split(G1, AT, G3);
    n.route.learn(AT, G3);
    n.advance(200, &[1]);
    assert_eq!(n.written(1, stmt), Ok(()));
    // G2 splits too: the record moves to G4, so the commit goes there once the node learns it,
    // and the intent written in G1 before the split is resolved in G3.
    assert!(n.host.split(G2, RECORDS, G4));
    n.split(G2, RECORDS, G4);
    let clock = n.clock();
    n.coords.get_mut(&1).expect("a coordinator").commit(clock);
    n.settle();
    n.route.learn(RECORDS, G4);
    n.advance(200, &[1]);
    // G4's first leader starts its read cache 250 ms above its lease (005 FR-005), and the commit
    // goes above the reads of its record there (008 T043): it waits for the clock (FR-008).
    n.clock_wait(1);
    let Outcome::Committed(ts) = n.ended(1) else { panic!("not committed") };
    n.advance(1_000, &[1]);
    assert!(n.resolved(1));
    assert_eq!((n.get(H), n.get(I), n.get(A)), (doc(b"h1"), doc(b"i1"), doc(b"a1")));
    let rec = n.record(TxnId(1));
    assert_eq!((rec.status, rec.commit_ts), (TxnStatus::Committed, ts));
}

/// The reaper removes a committed record whose coordinator died before resolving its intents
/// (008, record GC) once its session has expired, and only after resolving them: also the intent
/// of a key longer than the record keeps, which its interval gives only as a prefix and the range
/// finds with `FindIntents`. Removed before that, a reader would take the intent's transaction
/// for abandoned and the committed value would be lost.
#[test]
fn the_reaper_removes_a_finished_record_only_after_resolving_every_intent() {
    let mut n = Node::new();
    let long: Vec<u8> = [b"\x01p".as_slice(), &[b'k'; 2_000]].concat();
    let txn = TxnId(70);
    n.begin(1, txn);
    n.write(1, &[(A, put(b"a")), (long.as_slice(), put(b"long"))]).unwrap();
    // The coordinator dies; its commit reached the record, its resolutions did not.
    n.coords.remove(&1);
    let mut spans = Spans::default();
    spans.add(A);
    spans.add(&long);
    let intervals = spans.intervals();
    assert!(intervals.iter().any(|(s, _)| s.len() < long.len()), "the long key is cut");
    let commit = RecordOp::Commit { ts: n.clock(), spans: intervals };
    let committed = n.ask(G2, TxnOp::EndTxn { txn, op: commit });
    assert!(matches!(committed, Ok(Reply::Record(r)) if r.status == TxnStatus::Committed));
    let mut reaper = Reaper::with_session_ttl(1_000);
    n.reap(&mut reaper);
    assert_eq!(reaper.forgotten(), 0, "its session has not expired");
    n.advance(2_000, &[]);
    n.reap(&mut reaper);
    assert_eq!(reaper.forgotten(), 1);
    assert_eq!(n.get(&record_key(txn)), None, "removed");
    assert_eq!(n.get(A), doc(b"a"));
    assert_eq!(n.get(&long), doc(b"long"), "resolved before the removal");
}

/// A statement sent to a range frozen for a merge gets `OutOfRange` while the route still names
/// that range. If the merge is given up and the range thaws, the keys stay where they were: the
/// coordinator sends the statement to the same range again at its next tick and it is written
/// (seeds 160 and 683 of `changestream` and 381 of `search`, plan `heavy` at 200 %, where the
/// statements waited for good for a route that never changed).
#[test]
fn a_statement_refused_by_a_range_that_thaws_goes_to_it_again() {
    let mut n = Node::new();
    n.begin(1, TxnId(1));
    assert_eq!(n.write(1, &[(A, put(b"a1"))]), Ok(()));
    assert!(n.host.freeze(G1, 5));
    n.settle();
    let frozen = n.core.state_machine(G1).and_then(|sm| sm.frozen_for());
    assert_eq!(frozen, Some(5), "the merge froze the range");
    let stmt = n.send_write(1, &[(A, put(b"a2"))]);
    n.settle();
    n.advance(300, &[1]);
    assert!(
        !n.events.get(&1).is_some_and(|e| e
            .iter()
            .any(|e| matches!(e, Event::Written { stmt: s, .. } if *s == stmt))),
        "a frozen range writes nothing"
    );
    let now = n.now;
    assert!(n.host.thaw(&mut n.core, G1, now), "the merge is given up");
    n.settle();
    n.advance(300, &[1]);
    assert_eq!(n.written(1, stmt), Ok(()));
    assert!(matches!(n.commit(1), Outcome::Committed(_)));
    assert_eq!(n.get(A), doc(b"a2"));
}
