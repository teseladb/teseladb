//! The range host on a core (005 FR-007 to FR-010, T015): writes take their latches, read the
//! current versions, are evaluated at their mark and answered at the point their write concern
//! asks for; reads go through the core and the latches; the LSM's release floor reaches the log.
//! One replica that leads on its own, its log and its LSM on one disk, driven as an embedder
//! would.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::ViewCache;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::{DedupKey, Put};
use teseladb_range::host::{
    DeleteReq, Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, ScanReq, VIEW_SPAN,
    WriteReq,
};
use teseladb_range::mutate::Outcome;
use teseladb_range::push::Row;
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_range::view::{Decline, FastRead, Newest, Part};
use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

const G: GroupId = GroupId(7);
const START: &[u8] = b"\x01a";
const END: &[u8] = b"\x01z";
const WALL: u64 = 1_800_000_000_000;
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };

struct Node {
    disk: MemDisk,
    core: MultiRaftCore<RangeSm>,
    host: RangeHost,
    now: Nanos,
    replies: Vec<(ReqId, Result<Reply, RangeError>)>,
}

impl Node {
    fn new() -> Node {
        Node::with_lsm(LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() })
    }

    /// A node whose LSM has `lcfg`.
    fn with_lsm(lcfg: LsmConfig) -> Node {
        let mut disk = MemDisk::new();
        let clock = NodeClock::for_node(NodeId(1));
        let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
        let cfg = CoreConfig { clock: clock.clone(), ..CoreConfig::new(NodeId(1), 0) };
        let mut core = MultiRaftCore::new(cfg, store, 1);
        let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        let desc = GroupDesc { group: G, voters: vec![NodeId(1)], learners: vec![] };
        core.create_group(desc, sm).unwrap();
        let mut host = RangeHost::new(lsm, clock);
        host.add_range(G, START.to_vec(), END.to_vec());
        let mut n = Node { disk, core, host, now: Nanos::ZERO, replies: vec![] };
        n.settle();
        n
    }

    fn wall(&self) -> u64 {
        WALL + self.now.0 / 1_000_000
    }

    /// Runs the core, the LSM and the host until nothing moves.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            let mut moved = false;
            self.now = self.now + Nanos::from_millis(1);
            let (now, wall) = (self.now, self.wall());
            self.core.on_tick(now, wall);
            moved |= self.turn(now, wall);
            if !moved {
                return;
            }
        }
        panic!("the node never settled");
    }

    /// One turn of the core's loop at `now` without a tick: the core's wall clock stays at
    /// `wall`, the one of its last tick. `true` if something moved.
    fn turn(&mut self, now: Nanos, wall: u64) -> bool {
        let mut moved = false;
        // A read that asked for another block moved too: a document in a value file needs the
        // block of its key first and then the one of its value (004 FR-034).
        let asked = self.host.lsm().borrow().block_reads_asked();
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
        let replies = self.host.take_replies();
        moved |= !replies.is_empty();
        self.replies.extend(replies);
        moved || self.host.lsm().borrow().block_reads_asked() != asked
    }

    fn reply(&self, id: ReqId) -> Option<&Result<Reply, RangeError>> {
        self.replies.iter().find(|(r, _)| *r == id).map(|(_, x)| x)
    }

    fn mutate(&mut self, key: &[u8], m: Box<dyn Mutation>, wc: WriteConcern) -> ReqId {
        let (now, wall) = (self.now, self.wall());
        let w = WriteReq { g: G, keys: vec![key.to_vec()], m, dedup: None, wc };
        self.host.mutate(&mut self.core, w, now, wall)
    }

    fn get(&mut self, key: &[u8], concern: ReadConcern) -> ReqId {
        let (now, wall) = (self.now, self.wall());
        self.host.get(&mut self.core, G, key, concern, now, wall)
    }
}

/// Adds one to a counter document (the first byte of its value).
struct Inc;

impl Mutation for Inc {
    fn eval(&self, current: &[Option<(u8, &[u8])>], ts: Hlc) -> Result<Outcome, MutationError> {
        let n = current[0].map_or(0, |(_, v)| v[0]);
        Ok(Outcome {
            puts: vec![Put { key: b"\x01count".to_vec(), value_type: 1, value: vec![n + 1] }],
            deletes: vec![],
            change: ts.physical.to_be_bytes().to_vec(),
            result: vec![],
        })
    }
}

struct Refuse;

impl Mutation for Refuse {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Err(MutationError("duplicate key".into()))
    }
}

fn count(n: &Node, id: ReqId) -> Option<u8> {
    match n.reply(id) {
        Some(Ok(Reply::Value { value: Some((1, v)), .. })) => Some(v[0]),
        Some(Ok(Reply::Value { value: None, .. })) => Some(0),
        other => panic!("not a value: {other:?}"),
    }
}

#[test]
fn concurrent_increments_all_count_and_reads_see_them() {
    let mut n = Node::new();
    let ids: Vec<ReqId> =
        (0..20).map(|_| n.mutate(b"\x01count", Box::new(Inc), MAJORITY)).collect();
    n.settle();
    let mut marks = Vec::new();
    for id in &ids {
        match n.reply(*id) {
            Some(Ok(Reply::Mutated { ts, change, .. })) => {
                assert_eq!(
                    change,
                    &ts.physical.to_be_bytes().to_vec(),
                    "evaluated at its own mark"
                );
                marks.push(*ts);
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(marks.windows(2).all(|w| w[0] < w[1]), "each write above the last");
    for concern in [ReadConcern::Majority, ReadConcern::Linearizable, ReadConcern::Local] {
        let r = n.get(b"\x01count", concern);
        n.settle();
        assert_eq!(count(&n, r), Some(20), "{concern:?}");
    }
    let r = n.get(b"\x01none", ReadConcern::Available);
    n.settle();
    assert!(matches!(n.reply(r), Some(Ok(Reply::Value { value: None, .. }))));
}

#[test]
fn each_write_concern_answers_and_w0_never_does() {
    let mut n = Node::new();
    let w1 = n.mutate(b"\x01count", Box::new(Inc), WriteConcern { w: W::Nodes(1), j: false });
    let w1j = n.mutate(b"\x01count", Box::new(Inc), WriteConcern { w: W::Nodes(1), j: true });
    let w0 = n.mutate(b"\x01count", Box::new(Inc), WriteConcern { w: W::Zero, j: false });
    let too_many = n.mutate(b"\x01count", Box::new(Inc), WriteConcern { w: W::Nodes(2), j: false });
    n.settle();
    assert!(matches!(n.reply(w1), Some(Ok(Reply::Mutated { .. }))));
    assert!(matches!(n.reply(w1j), Some(Ok(Reply::Mutated { .. }))));
    assert_eq!(n.reply(w0), None, "w: 0 has no answer");
    assert_eq!(n.reply(too_many), Some(&Err(RangeError::Unsatisfiable)));
    // The w: 0 write was applied all the same.
    let r = n.get(b"\x01count", ReadConcern::Majority);
    n.settle();
    assert_eq!(count(&n, r), Some(3));
}

#[test]
fn a_refused_mutation_writes_nothing_and_frees_its_document() {
    let mut n = Node::new();
    let bad = n.mutate(b"\x01count", Box::new(Refuse), MAJORITY);
    let good = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert_eq!(
        n.reply(bad),
        Some(&Err(RangeError::Mutation(MutationError("duplicate key".into()))))
    );
    assert!(matches!(n.reply(good), Some(Ok(Reply::Mutated { .. }))));
}

/// Writes its document and claims a shared key, as a unique index entry does (009 FR-004): it
/// asks to read the key and refuses when another document holds it.
struct Claim(u8);

const CLAIMED: &[u8] = b"\x01unique";

impl Mutation for Claim {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        panic!("the host evaluates with the keys")
    }

    fn eval_with(
        &self,
        keys: &[Vec<u8>],
        current: &[Option<(u8, &[u8])>],
        _: Hlc,
    ) -> Result<Outcome, MutationError> {
        let at = keys.iter().position(|k| k == CLAIMED);
        if let Some(Some((_, holder))) = at.map(|i| current[i])
            && holder != [self.0]
        {
            return Err(MutationError("duplicate key".into()));
        }
        let put = |k: &[u8]| Put { key: k.to_vec(), value_type: 1, value: vec![self.0] };
        let puts = vec![put(&[0x01, b'd', self.0]), put(CLAIMED)];
        Ok(Outcome { puts, deletes: vec![], change: vec![], result: vec![] })
    }

    fn reads(&self, outcome: &Outcome) -> Vec<Vec<u8>> {
        outcome.puts.iter().filter(|p| p.key == CLAIMED).map(|p| p.key.clone()).collect()
    }
}

#[test]
fn a_mutation_reads_the_keys_it_asks_for_before_it_writes() {
    let mut n = Node::new();
    let first = n.mutate(&[0x01, b'd', 1], Box::new(Claim(1)), MAJORITY);
    n.settle();
    assert!(matches!(n.reply(first), Some(Ok(Reply::Mutated { .. }))));
    let again = n.mutate(&[0x01, b'd', 1], Box::new(Claim(1)), MAJORITY);
    let other = n.mutate(&[0x01, b'd', 2], Box::new(Claim(2)), MAJORITY);
    n.settle();
    assert!(matches!(n.reply(again), Some(Ok(Reply::Mutated { .. }))), "its own key");
    assert_eq!(
        n.reply(other),
        Some(&Err(RangeError::Mutation(MutationError("duplicate key".into())))),
        "another document holds the key"
    );
    let read = n.get(&[0x01, b'd', 2], ReadConcern::Majority);
    n.settle();
    assert_eq!(count(&n, read), Some(0), "the refused write wrote nothing");
}

/// Writes its document and a key of another range (an index entry past a split, 009).
struct Spill;

impl Mutation for Spill {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = |k: &[u8]| Put { key: k.to_vec(), value_type: 1, value: vec![1] };
        let puts = vec![put(b"\x01doc"), put(b"\x02other")];
        Ok(Outcome { puts, deletes: vec![], change: vec![], result: vec![] })
    }
}

#[test]
fn a_mutation_that_reaches_another_range_writes_nothing() {
    let mut n = Node::new();
    let spill = n.mutate(b"\x01doc", Box::new(Spill), MAJORITY);
    n.settle();
    let Some(Err(RangeError::Mutation(MutationError(msg)))) = n.reply(spill) else {
        panic!("{:?}", n.reply(spill))
    };
    assert!(msg.contains("another range"), "{msg}");
    let read = n.get(b"\x01doc", ReadConcern::Majority);
    n.settle();
    assert_eq!(count(&n, read), Some(0), "nothing was written");
}

#[test]
fn requests_for_an_unknown_range_or_outside_it_fail() {
    let mut n = Node::new();
    let (now, wall) = (n.now, n.wall());
    let unknown = n.host.get(&mut n.core, GroupId(99), b"\x01a", ReadConcern::Majority, now, wall);
    let outside = n.get(b"\x02zz", ReadConcern::Majority);
    let outside_write = n.mutate(b"\x00local", Box::new(Inc), MAJORITY);
    n.settle();
    assert_eq!(n.reply(unknown), Some(&Err(RangeError::OutOfRange)));
    assert_eq!(n.reply(outside), Some(&Err(RangeError::OutOfRange)));
    assert_eq!(n.reply(outside_write), Some(&Err(RangeError::OutOfRange)));
}

#[test]
fn the_lsm_floor_reaches_the_log() {
    let mut n = Node::new();
    let id = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(n.reply(id).is_some());
    let floor = n.core.store().release_floor_of(G);
    assert_eq!(
        floor,
        n.host.lsm().borrow().release_floor(G),
        "the log holds what the LSM did not flush"
    );
    assert!(floor.is_some(), "the write is only in a memtable");
    n.host.lsm().borrow_mut().flush();
    n.settle();
    assert_eq!(n.core.store().release_floor_of(G), None);
}

#[test]
fn dedup_records_travel_with_the_write() {
    let mut n = Node::new();
    let (now, wall) = (n.now, n.wall());
    let rec = DedupKey { lsid: 5, txn_number: 1, stmt_id: 0 };
    let w = WriteReq {
        g: G,
        keys: vec![b"\x01count".to_vec()],
        m: Box::new(Inc),
        dedup: Some(rec),
        wc: MAJORITY,
    };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    n.settle();
    assert!(matches!(n.reply(id), Some(Ok(Reply::Mutated { .. }))));
    let key = teseladb_range::sm::dedup_key(G, 5, 1, 0);
    let view = n.host.lsm().borrow().view();
    let newest = Hlc { physical: u64::MAX, logical: 0 };
    let found = view.get(&mut n.host.lsm().borrow_mut(), &key, newest);
    assert!(matches!(found, teseladb_lsm::Lookup::Hit(_)), "{found:?}");
}

#[test]
fn a_write_without_documents_or_on_a_range_the_core_lacks_fails() {
    let mut n = Node::new();
    let (now, wall) = (n.now, n.wall());
    let none = WriteReq { g: G, keys: vec![], m: Box::new(Inc), dedup: None, wc: MAJORITY };
    let none = n.host.mutate(&mut n.core, none, now, wall);
    // The host knows a range the core does not hold (yet): nobody leads it here.
    n.host.add_range(GroupId(8), b"\x02a".to_vec(), b"\x02z".to_vec());
    let lost = WriteReq {
        g: GroupId(8),
        keys: vec![b"\x02k".to_vec()],
        m: Box::new(Inc),
        dedup: None,
        wc: MAJORITY,
    };
    let lost = n.host.mutate(&mut n.core, lost, now, wall);
    n.settle();
    assert_eq!(n.reply(none), Some(&Err(RangeError::OutOfRange)));
    assert_eq!(n.reply(lost), Some(&Err(RangeError::NotLeader { hint: NodeId(0) })));
}

/// Writes a value of `.1` bytes at `key`, each of them `.2`.
struct SetLarge(Vec<u8>, usize, u8);

impl Mutation for SetLarge {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = Put { key: self.0.clone(), value_type: 1, value: vec![self.2; self.1] };
        Ok(Outcome { puts: vec![put], deletes: vec![], change: vec![], result: vec![] })
    }
}

/// Writes `value` at `key` (`None`: deletes it).
struct SetKey(Vec<u8>, Option<u8>);

impl Mutation for SetKey {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let (puts, deletes) = match self.1 {
            Some(v) => (vec![Put { key: self.0.clone(), value_type: 1, value: vec![v] }], vec![]),
            None => (vec![], vec![self.0.clone()]),
        };
        Ok(Outcome { puts, deletes, change: vec![], result: vec![] })
    }
}

#[test]
fn a_scan_returns_the_visible_documents_up_to_its_limit() {
    let mut n = Node::new();
    for (k, v) in
        [(&b"\x01b"[..], Some(2)), (b"\x01a", Some(1)), (b"\x01c", Some(3)), (b"\x01b", None)]
    {
        let m = Box::new(SetKey(k.to_vec(), v));
        n.mutate(k, m, MAJORITY);
        n.settle();
    }
    let (now, wall) = (n.now, n.wall());
    let scan = |start: &[u8], end: &[u8], concern, limit| ScanReq {
        g: G,
        start: start.to_vec(),
        end: end.to_vec(),
        concern,
        limit,
        backward: false,
    };
    let back = |start: &[u8], end: &[u8], limit| ScanReq {
        backward: true,
        ..scan(start, end, ReadConcern::Majority, limit)
    };
    let all =
        n.host.scan(&mut n.core, scan(b"\x01a", b"\x01z", ReadConcern::Majority, 10), now, wall);
    let one = n.host.scan(&mut n.core, scan(b"\x01a", b"\x01z", ReadConcern::Local, 1), now, wall);
    let outside =
        n.host.scan(&mut n.core, scan(b"\x01a", b"\x02", ReadConcern::Majority, 10), now, wall);
    let empty =
        n.host.scan(&mut n.core, scan(b"\x01m", b"\x01m", ReadConcern::Majority, 10), now, wall);
    let none =
        n.host.scan(&mut n.core, scan(b"\x01d", b"\x01e", ReadConcern::Majority, 10), now, wall);
    let last = n.host.scan(&mut n.core, back(b"\x01a", b"\x01z", 1), now, wall);
    let all_back = n.host.scan(&mut n.core, back(b"\x01a", b"\x01z", 10), now, wall);
    let below_c = n.host.scan(&mut n.core, back(b"\x01a", b"\x01c", 10), now, wall);
    n.settle();
    let rows = |r: Option<&Result<Reply, RangeError>>| match r {
        Some(Ok(Reply::Batch(rows))) => {
            rows.iter().map(|(k, (_, v))| (k.clone(), v[0])).collect::<Vec<_>>()
        }
        other => panic!("not a batch: {other:?}"),
    };
    assert_eq!(
        rows(n.reply(all)),
        vec![(b"\x01a".to_vec(), 1), (b"\x01c".to_vec(), 3)],
        "the deleted one is gone"
    );
    assert_eq!(rows(n.reply(one)), vec![(b"\x01a".to_vec(), 1)]);
    assert_eq!(n.reply(outside), Some(&Err(RangeError::OutOfRange)));
    assert_eq!(n.reply(empty), Some(&Err(RangeError::OutOfRange)), "an empty interval");
    assert_eq!(n.reply(none), Some(&Ok(Reply::Batch(vec![]))));
    // Backward (H-003): from the last key down, the deleted one skipped, the end excluded.
    assert_eq!(rows(n.reply(last)), vec![(b"\x01c".to_vec(), 3)]);
    assert_eq!(rows(n.reply(all_back)), vec![(b"\x01c".to_vec(), 3), (b"\x01a".to_vec(), 1)]);
    assert_eq!(rows(n.reply(below_c)), vec![(b"\x01a".to_vec(), 1)]);
}

#[test]
fn a_read_at_a_mark_sees_that_moment_and_later_writes_stay_above_it() {
    let mut n = Node::new();
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    let r = n.get(b"\x01count", ReadConcern::Majority);
    n.settle();
    let Some(Ok(Reply::Value { ts: first, .. })) = n.reply(r).cloned() else { panic!() };
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    // At the first read's mark the count is still 1; now it is 2.
    let (now, wall) = (n.now, n.wall());
    let past = n.host.get_at(&mut n.core, G, b"\x01count", first, now, wall);
    n.settle();
    assert_eq!(count(&n, past), Some(1));
    // A read at a mark ahead of the clock (within the cluster's clock offset) waits for the clock
    // to reach it (008), then the next write goes above it.
    let ahead = Hlc { physical: n.wall() + 200, logical: 0 };
    let (now, wall) = (n.now, n.wall());
    let r = n.host.get_at(&mut n.core, G, b"\x01count", ahead, now, wall);
    while n.reply(r).is_none() {
        n.settle();
    }
    assert_eq!(count(&n, r), Some(2));
    let w = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    match n.reply(w) {
        Some(Ok(Reply::Mutated { ts, .. })) => assert!(*ts > ahead, "{ts:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&n, r), Some(2), "the read at that mark still sees 2");
}

#[test]
fn a_read_at_a_mark_is_served_in_the_turn_whose_wall_clock_reaches_it() {
    // 005 FR-032: a transaction's mark comes from the node's HLC, which a connection's thread
    // moved with the wall clock it read a moment ago. The core reads the wall clock at the start
    // of each turn with work and the host compares the mark with that reading, not with the one
    // of the core's last tick, up to a tick old.
    let mut n = Node::new();
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    let (now, at_tick) = (n.now, n.wall());
    let mark = Hlc { physical: at_tick + 5, logical: 0 };
    let r = n.host.get_at(&mut n.core, G, b"\x01count", mark, now, at_tick);
    while n.turn(now, at_tick) {}
    assert!(n.reply(r).is_none(), "the wall clock of the last tick is below the mark");
    // The next turn, before the core's next tick: its wall clock reads the mark's millisecond.
    n.core.on_clock(now, at_tick + 5);
    while n.turn(now, at_tick + 5) {}
    assert_eq!(count(&n, r), Some(1));
    // Half a second ahead of the wall clock is still refused (D-081).
    let far = Hlc { physical: at_tick + 5 + 501, logical: 0 };
    let r = n.host.get_at(&mut n.core, G, b"\x01count", far, now, at_tick + 5);
    while n.turn(now, at_tick + 5) {}
    assert!(matches!(n.reply(r), Some(Err(RangeError::Timeout))), "{:?}", n.reply(r));
}

#[test]
fn delete_removes_a_document_and_a_missing_one_writes_no_tombstone() {
    // FR-017: `delete` of the client API.
    let mut n = Node::new();
    let w = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(matches!(n.reply(w), Some(Ok(Reply::Mutated { .. }))));
    let (now, wall) = (n.now, n.wall());
    let req = DeleteReq { g: G, key: b"\x01count".to_vec(), dedup: None, wc: MAJORITY };
    let d = n.host.delete(&mut n.core, req, now, wall);
    let req = DeleteReq { g: G, key: b"\x01none".to_vec(), dedup: None, wc: MAJORITY };
    let missing = n.host.delete(&mut n.core, req, now, wall);
    n.settle();
    assert!(matches!(n.reply(d), Some(Ok(Reply::Mutated { result, .. })) if result == &[1]));
    assert!(matches!(n.reply(missing), Some(Ok(Reply::Mutated { result, .. })) if result == &[0]));
    let r = n.get(b"\x01count", ReadConcern::Majority);
    n.settle();
    assert!(matches!(n.reply(r), Some(Ok(Reply::Value { value: None, .. }))));
}

/// A write the leader reaches only after its deadline writes nothing (009 FR-006): the
/// operation that sent it may have ended and released its read of the catalog, so an index
/// change may already count on no write with the old list of indexes.
#[test]
fn a_write_past_its_deadline_is_not_evaluated() {
    let mut n = Node::new();
    let (now, wall) = (n.now, n.wall());
    let w = |m: Box<dyn Mutation>| WriteReq {
        g: G,
        keys: vec![b"\x01count".to_vec()],
        m,
        dedup: None,
        wc: MAJORITY,
    };
    let late = n.host.mutate_until(&mut n.core, w(Box::new(Inc)), wall - 1, (now, wall));
    n.settle();
    assert_eq!(n.reply(late), Some(&Err(RangeError::Timeout)));
    let read = n.get(b"\x01count", ReadConcern::Linearizable);
    n.settle();
    assert_eq!(count(&n, read), Some(0), "the late write wrote nothing");
    let (now, wall) = (n.now, n.wall());
    let on_time = n.host.mutate_until(&mut n.core, w(Box::new(Inc)), wall + 60_000, (now, wall));
    n.settle();
    assert!(matches!(n.reply(on_time), Some(Ok(Reply::Mutated { .. }))));
}

/// A read from a connection's thread (005 FR-028), at the node's wall clock and monotonic time.
fn read_outside(n: &Node, key: &[u8], concern: ReadConcern, later: Nanos) -> FastRead {
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let mut cache = ViewCache::default();
    view.read(&mut cache, key, n.wall(), || n.now + later, concern)
}

#[test]
fn a_read_outside_the_core_sees_the_range_and_a_later_write_goes_above_it() {
    let mut n = Node::new();
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    let FastRead::Found { ts: read, value } =
        read_outside(&n, b"\x01count", ReadConcern::Majority, Nanos::ZERO)
    else {
        panic!("served outside the core")
    };
    assert_eq!(value, Some((1, vec![1])));
    let FastRead::Found { value: none, .. } =
        read_outside(&n, b"\x01nothing", ReadConcern::Local, Nanos::ZERO)
    else {
        panic!("served outside the core")
    };
    assert_eq!(none, None);
    // The next write of the range goes above the read's mark (the read bound).
    let w = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    match n.reply(w) {
        Some(Ok(Reply::Mutated { ts, .. })) => assert!(*ts > read, "{ts:?} above {read:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_read_outside_the_core_goes_to_it_when_it_cannot_be_served_there() {
    let mut n = Node::new();
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    // A write in flight on the range: the read cannot see its latch.
    let w = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    assert!(matches!(
        read_outside(&n, b"\x01count", ReadConcern::Local, Nanos::ZERO),
        FastRead::Core
    ));
    n.settle();
    assert!(n.reply(w).is_some());
    // Far past what the core published as valid when the mark is taken (a thread stopped before
    // it): the lease may have gone, and the read goes to the core.
    let late = Nanos::from_secs(60);
    assert!(matches!(read_outside(&n, b"\x01count", ReadConcern::Local, late), FastRead::Core));
    // Linearizable reads and keys of another range always go to the core.
    let lin = read_outside(&n, b"\x01count", ReadConcern::Linearizable, Nanos::ZERO);
    assert!(matches!(lin, FastRead::Core));
    assert!(matches!(read_outside(&n, b"\x02zz", ReadConcern::Local, Nanos::ZERO), FastRead::Core));
    // Back to normal once the write is applied.
    assert!(matches!(
        read_outside(&n, b"\x01count", ReadConcern::Local, Nanos::ZERO),
        FastRead::Found { .. }
    ));
}

/// A point read outside the core looks only at the writes of its own key (H-010 of the native
/// tests, `spec/tla/HlcKeyLatches.tla`): a write in flight on another key does not send it to the
/// core, and the next write of the key it read goes above its mark. A scan over both keys still
/// sees the write in flight.
#[test]
fn a_point_read_outside_the_core_waits_only_for_the_writes_of_its_key() {
    use teseladb_range::latch::KeySlots;
    let (written, other) = (b"\x01count".as_slice(), b"\x01other".as_slice());
    assert_ne!(KeySlots::slot(written), KeySlots::slot(other));
    let mut n = Node::new();
    n.mutate(written, Box::new(Inc), MAJORITY);
    n.settle();
    let w = n.mutate(written, Box::new(Inc), MAJORITY);
    let FastRead::Found { ts: read, value } =
        read_outside(&n, other, ReadConcern::Local, Nanos::ZERO)
    else {
        panic!("a write of another key does not send the read to the core")
    };
    assert_eq!(value, None);
    assert!(matches!(read_outside(&n, written, ReadConcern::Local, Nanos::ZERO), FastRead::Core));
    assert!(scan_outside(&n, (b"\x01", b"\x02"), 10, ReadConcern::Local).is_none());
    n.settle();
    assert!(n.reply(w).is_some());
    // The key it read is written above its mark.
    let later = n.mutate(other, Box::new(Inc), MAJORITY);
    n.settle();
    match n.reply(later) {
        Some(Ok(Reply::Mutated { ts, .. })) => assert!(*ts > read, "{ts:?} above {read:?}"),
        other => panic!("{other:?}"),
    }
}

/// A point read whose key only shares its slot with a key a write holds is still served outside
/// the core: it looks at the latched keys themselves after it reads (H-013 of the native tests,
/// writes slowing the reads of other documents), and the next write of its key goes above it.
#[test]
fn a_point_read_outside_the_core_is_served_when_its_key_only_shares_a_slot() {
    use teseladb_range::latch::KeySlots;
    let written = b"\x01count".as_slice();
    let sharing = (0u32..)
        .map(|i| [b"\x01k".as_slice(), &i.to_be_bytes()].concat())
        .find(|k| KeySlots::slot(k) == KeySlots::slot(written))
        .expect("a key in the same slot");
    let mut n = Node::new();
    n.mutate(&sharing, Box::new(SetKey(sharing.clone(), Some(7))), MAJORITY);
    n.settle();
    let w = n.mutate(written, Box::new(Inc), MAJORITY);
    let FastRead::Found { ts: read, value } =
        read_outside(&n, &sharing, ReadConcern::Local, Nanos::ZERO)
    else {
        panic!("a write of another key in the same slot does not send the read to the core")
    };
    assert_eq!(value, Some((1, vec![7])));
    assert!(matches!(read_outside(&n, written, ReadConcern::Local, Nanos::ZERO), FastRead::Core));
    n.settle();
    assert!(n.reply(w).is_some());
    let later = n.mutate(&sharing, Box::new(SetKey(sharing.clone(), Some(8))), MAJORITY);
    n.settle();
    match n.reply(later) {
        Some(Ok(Reply::Mutated { ts, .. })) => assert!(*ts > read, "{ts:?} above {read:?}"),
        other => panic!("{other:?}"),
    }
}

/// A scan from a connection's thread (005 FR-028), as [`read_outside`]; no block is read.
fn scan_outside(
    n: &Node,
    span: (&[u8], &[u8]),
    limit: usize,
    concern: ReadConcern,
) -> Option<(Hlc, Vec<Row>)> {
    scan_outside_way(n, span, (limit, false), concern)
}

/// [`scan_outside`] in either direction (`backward`: from the span's last key down).
fn scan_outside_way(
    n: &Node,
    span: (&[u8], &[u8]),
    (limit, backward): (usize, bool),
    concern: ReadConcern,
) -> Option<(Hlc, Vec<Row>)> {
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let mut cache = ViewCache::default();
    let part = Part { docs: limit, backward, blocks: 64 };
    view.scan(&mut cache, span, part, || n.now, concern, |_| false).map(|s| (s.ts, s.rows))
}

#[test]
fn a_scan_outside_the_core_gives_the_documents_of_the_span_and_a_later_write_goes_above_it() {
    let mut n = Node::new();
    for (k, v) in
        [(&b"\x01b"[..], Some(2)), (b"\x01a", Some(1)), (b"\x01c", Some(3)), (b"\x01b", None)]
    {
        n.mutate(k, Box::new(SetKey(k.to_vec(), v)), MAJORITY);
        n.settle();
    }
    let (mark, rows) =
        scan_outside(&n, (b"\x01a", b"\x01y"), 10, ReadConcern::Local).expect("served outside");
    assert_eq!(rows, vec![(b"\x01a".to_vec(), (1, vec![1])), (b"\x01c".to_vec(), (1, vec![3]))]);
    let (_, first) =
        scan_outside(&n, (b"\x01a", b"\x01y"), 1, ReadConcern::Majority).expect("served outside");
    assert_eq!(first, vec![(b"\x01a".to_vec(), (1, vec![1]))], "no more than the limit");
    let (_, none) =
        scan_outside(&n, (b"\x01d", b"\x01y"), 10, ReadConcern::Local).expect("served outside");
    assert!(none.is_empty());
    // From the last key down, as `sort({_id: -1})` reads (H-003).
    let back = (b"\x01a".as_slice(), b"\x01y".as_slice());
    let (_, last) = scan_outside_way(&n, back, (1, true), ReadConcern::Local).expect("served");
    assert_eq!(last, vec![(b"\x01c".to_vec(), (1, vec![3]))]);
    let (_, all) = scan_outside_way(&n, back, (10, true), ReadConcern::Local).expect("served");
    assert_eq!(all, vec![(b"\x01c".to_vec(), (1, vec![3])), (b"\x01a".to_vec(), (1, vec![1]))]);
    // The next write of the range goes above the scan's mark (the read bound).
    let w = n.mutate(b"\x01d", Box::new(SetKey(b"\x01d".to_vec(), Some(4))), MAJORITY);
    n.settle();
    match n.reply(w) {
        Some(Ok(Reply::Mutated { ts, .. })) => assert!(*ts > mark, "{ts:?} above {mark:?}"),
        other => panic!("{other:?}"),
    }
}

/// A scan outside the core reads at the mark of the last entry its range applied (005 FR-031):
/// a write in flight, of a key of its span or not, does not send it to the core, because that
/// write's mark is above the scan's; the scan gives what was applied, and the write once it is.
#[test]
fn a_scan_outside_the_core_reads_what_was_applied_whatever_is_in_flight() {
    let mut n = Node::new();
    let first = n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let Some(Ok(Reply::Mutated { ts: applied, .. })) = n.reply(first).cloned() else {
        panic!("the first write")
    };
    // In flight: an insert of a key that has no version yet, and a second write of `a`.
    let insert = n.mutate(b"\x01m", Box::new(SetKey(b"\x01m".to_vec(), Some(2))), MAJORITY);
    let again = n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(9))), MAJORITY);
    let before = vec![(b"\x01a".to_vec(), (1, vec![1]))];
    for (span, way) in [
        ((&b"\x01a"[..], &b"\x01y"[..]), (10, false)),
        ((b"\x01a", b"\x01k"), (10, false)),
        ((b"\x01a", b"\x01y"), (1, true)),
    ] {
        let (mark, rows) = scan_outside_way(&n, span, way, ReadConcern::Local)
            .expect("a write in flight does not send the scan to the core");
        assert_eq!((mark, rows), (applied, before.clone()), "{span:?} {way:?}");
    }
    let (_, none) =
        scan_outside(&n, (b"\x01m", b"\x01m\x00"), 10, ReadConcern::Local).expect("served outside");
    assert!(none.is_empty(), "the insert in flight is not applied yet");
    n.settle();
    // Both writes took marks above the scan's, and a scan now reads them.
    for w in [insert, again] {
        match n.reply(w) {
            Some(Ok(Reply::Mutated { ts, .. })) => {
                assert!(*ts > applied, "{ts:?} above {applied:?}")
            }
            other => panic!("{other:?}"),
        }
    }
    let (mark, all) =
        scan_outside(&n, (b"\x01a", b"\x01y"), 10, ReadConcern::Local).expect("served outside");
    assert!(mark > applied);
    assert_eq!(all, vec![(b"\x01a".to_vec(), (1, vec![9])), (b"\x01m".to_vec(), (1, vec![2]))]);
}

/// A scan with a filter runs it on the stored bytes and copies only what it accepts (010
/// FR-027): it says how many documents it examined, how many it had examined at each one it
/// took, and where it goes on when it stops at its number of documents.
#[test]
fn a_matching_scan_outside_the_core_copies_only_what_its_filter_accepts() {
    let mut n = Node::new();
    for (k, v) in [(b"\x01a", 1), (b"\x01b", 2), (b"\x01c", 3), (b"\x01d", 4), (b"\x01e", 5)] {
        n.mutate(k, Box::new(SetKey(k.to_vec(), Some(v))), MAJORITY);
        n.settle();
    }
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let mut cache = ViewCache::default();
    let span: (&[u8], &[u8]) = (b"\x01a", b"\x01y");
    let odd = |v: &[u8]| v[0] % 2 == 1;
    let mut scan = |span, examine| {
        let most = (examine, 64);
        view.scan_matching(&mut cache, span, most, || n.now, ReadConcern::Local, |_| false, odd)
            .expect("served outside")
    };
    let all = scan(span, 10);
    let keys: Vec<&[u8]> = all.rows.iter().map(|(k, _)| k.as_slice()).collect();
    assert_eq!(keys, [&b"\x01a"[..], b"\x01c", b"\x01e"]);
    assert_eq!((all.examined, all.seen.as_slice(), &all.next), (5, &[1, 3, 5][..], &None));
    // It stops at its number of documents and says where the rest starts.
    let first = scan(span, 2);
    assert_eq!((first.rows.len(), first.examined, first.seen.as_slice()), (1, 2, &[1][..]));
    let from = first.next.expect("more to examine");
    assert_eq!(from, b"\x01b\x00");
    let rest = scan((&from, span.1), 10);
    assert_eq!((rest.rows.len(), rest.examined, &rest.next), (2, 3, &None));
    assert!(rest.ts == all.ts);
}

/// A node whose range holds `docs` documents of 8 KiB (`\x01k000`, `\x01k001`…) in a file of the
/// LSM, of which the first `deleted` were deleted afterwards, in a second file: about two
/// documents a block, none of them in the cache of the reads outside the core. The deletions go
/// in a file of their own so that the documents they delete stay in the blocks a scan passes:
/// in one file the flush would set them apart as history (004 FR-031).
fn node_with_a_file(docs: u32, deleted: u32) -> Node {
    fill_a_file(Node::new(), docs, deleted)
}

/// [`node_with_a_file`] with each document in the file of its key, a block of its own: values of
/// 8 KiB go to value files by default (004 FR-034), and a key file of references holds a hundred
/// versions a block.
fn node_with_a_file_of_documents(docs: u32, deleted: u32) -> Node {
    let lcfg = LsmConfig {
        dir: "core-00/lsm".into(),
        value_separation_bytes: 16 << 10,
        ..LsmConfig::default()
    };
    fill_a_file(Node::with_lsm(lcfg), docs, deleted)
}

fn fill_a_file(mut n: Node, docs: u32, deleted: u32) -> Node {
    let key = |i: u32| format!("\x01k{i:03}").into_bytes();
    for i in 0..docs {
        n.mutate(&key(i), Box::new(SetLarge(key(i), 8 << 10, i as u8)), MAJORITY);
        n.settle();
    }
    if deleted > 0 {
        n.host.lsm().borrow_mut().flush();
        n.settle();
    }
    for i in 0..deleted {
        n.mutate(&key(i), Box::new(SetKey(key(i), None)), MAJORITY);
        n.settle();
    }
    n.host.lsm().borrow_mut().flush();
    n.settle();
    // A write after the flush: the view the range publishes next has the file.
    n.mutate(b"\x01z", Box::new(SetKey(b"\x01z".to_vec(), Some(9))), MAJORITY);
    n.settle();
    n
}

/// A value for each of its keys.
struct SetEach(Vec<Vec<u8>>, u8);

impl Mutation for SetEach {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = |k: &Vec<u8>| Put { key: k.clone(), value_type: 1, value: vec![self.1] };
        let puts = self.0.iter().map(put).collect();
        Ok(Outcome { puts, deletes: vec![], change: vec![], result: vec![] })
    }
}

/// A write that waits for blocks of its keys reads nothing again until the reads it asked for
/// are back (005 FR-037, H-060 of the native tests). Every result of the disk polls the host,
/// and at each poll the write read all its missing keys again, 0,7 ms for 191 of them: with the
/// disk busy with the 250 file operations of a core change, its core spent 0,4 s in one turn.
#[test]
fn a_write_that_waits_for_blocks_reads_its_keys_again_once_they_are_back() {
    const KEYS: u32 = 10;
    let key = |i: u32| format!("\x01k{i:03}").into_bytes();
    let mut n = node_with_a_file(KEYS, 0);
    // A read of the first document brings its block and finds it there: from now on every block
    // the cache is asked for and lacks lowers its share of hits.
    for _ in 0..2 {
        let read = n.get(&key(0), ReadConcern::Linearizable);
        n.settle();
        assert!(matches!(n.reply(read), Some(Ok(Reply::Value { .. }))), "{:?}", n.reply(read));
    }
    let lsm = n.host.lsm().clone();
    let hits = |lsm: &RefCell<Lsm>| lsm.borrow().metrics().cache_hit_ratio;
    assert!(hits(&lsm) > 0.0, "the second read found the block");
    // The other documents are in other blocks of the file, two a block.
    let keys: Vec<Vec<u8>> = (2..KEYS).map(key).collect();
    let (now, wall) = (n.now, n.wall());
    let m = Box::new(SetEach(keys.clone(), 7));
    let w = WriteReq { g: G, keys, m, dedup: None, wc: MAJORITY };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    let asked = lsm.borrow().block_reads_asked();
    assert!(!lsm.borrow().block_reads_back(asked), "the blocks of its keys are being read");
    let before = hits(&lsm);
    for _ in 0..50 {
        n.host.poll(&mut n.core, now, wall);
    }
    assert_eq!(hits(&lsm), before, "it read its keys again while their blocks were not back");
    assert_eq!(n.reply(id), None);
    // The reads come back and the write goes on.
    n.settle();
    assert!(matches!(n.reply(id), Some(Ok(Reply::Mutated { .. }))), "{:?}", n.reply(id));
}

/// Reads the block `need` names from `disk` into the cache of the reads outside the core, as a
/// connection's thread does, and counts it in `read`.
fn supply_block(
    disk: &mut MemDisk,
    view: &teseladb_range::view::RangeView,
    need: &teseladb_lsm::BlockNeed,
    read: &mut usize,
) -> bool {
    use teseladb_raftlog::Disk;
    *read += 1;
    let Ok(file) = disk.read_all(std::path::Path::new(&need.path)) else { return false };
    let (from, len) = (need.offset as usize, need.len as usize);
    file.get(from..from + len).is_some_and(|b| view.lsm().supply(need, b).is_ok())
}

#[test]
fn a_scan_outside_the_core_stops_at_its_blocks_with_what_it_has_and_goes_on_from_there() {
    // 005 FR-033: forty documents in some twenty blocks, read four blocks at a time.
    let span: (&[u8], &[u8]) = (b"\x01k", b"\x01l");
    for backward in [false, true] {
        // A node for each direction: the blocks one scan read stay in the cache.
        let mut n = node_with_a_file(40, 0);
        let now = n.now;
        let view = n.host.views().get(G).expect("the view of a range this node leads");
        let mut cache = ViewCache::default();
        let (mut from, mut to) = (span.0.to_vec(), span.1.to_vec());
        let (mut keys, mut parts) = (Vec::new(), 0);
        loop {
            let mut read = 0;
            let part = Part { docs: 100, backward, blocks: 4 };
            let got = view
                .scan(
                    &mut cache,
                    (&from, &to),
                    part,
                    || now,
                    ReadConcern::Local,
                    |need| supply_block(&mut n.disk, &view, need, &mut read),
                )
                .expect("served outside");
            assert!(read <= 4, "{read} blocks in one part");
            assert!(!got.rows.is_empty(), "a part comes back with documents");
            keys.extend(got.rows.into_iter().map(|(k, _)| k));
            parts += 1;
            match (got.next, backward) {
                (Some(next), false) => from = next,
                (Some(next), true) => to = next,
                (None, _) => break,
            }
        }
        let mut wanted: Vec<Vec<u8>> =
            (0..40u32).map(|i| format!("\x01k{i:03}").into_bytes()).collect();
        if backward {
            wanted.reverse();
        }
        assert_eq!(keys, wanted, "every document once, in order");
        assert!(parts >= 4, "{parts} parts for some twenty blocks");
        // The blocks stay in the cache: the next scan reads none and gives everything at once.
        let mut cache = ViewCache::default();
        let part = Part { docs: 100, backward, blocks: 4 };
        let all = view
            .scan(&mut cache, span, part, || now, ReadConcern::Local, |_| false)
            .expect("served outside");
        assert_eq!((all.rows.len(), all.next), (40, None));
    }
}

#[test]
fn a_matching_scan_outside_the_core_stops_at_its_blocks_and_goes_on_from_there() {
    // 005 FR-033 with a filter (010 FR-027): the documents with an even first byte.
    let mut n = node_with_a_file(40, 0);
    let now = n.now;
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let mut cache = ViewCache::default();
    let end: &[u8] = b"\x01l";
    let mut from = b"\x01k".to_vec();
    let (mut kept, mut examined, mut parts) = (Vec::new(), 0, 0);
    loop {
        let mut read = 0;
        let got = view
            .scan_matching(
                &mut cache,
                (&from, end),
                (1_000, 4),
                || now,
                ReadConcern::Local,
                |need| supply_block(&mut n.disk, &view, need, &mut read),
                |v| v[0] % 2 == 0,
            )
            .expect("served outside");
        assert!(read <= 4 && got.examined > 0, "{read} blocks, {} examined", got.examined);
        examined += got.examined;
        kept.extend(got.rows.into_iter().map(|(k, _)| k));
        parts += 1;
        match got.next {
            Some(next) => from = next,
            None => break,
        }
    }
    let wanted: Vec<Vec<u8>> =
        (0..40u32).step_by(2).map(|i| format!("\x01k{i:03}").into_bytes()).collect();
    assert_eq!((kept, examined), (wanted, 40));
    assert!(parts >= 4, "{parts} parts");
}

#[test]
fn a_part_without_a_document_reads_on_and_too_many_blocks_send_the_scan_to_the_core() {
    // Forty documents deleted and one left after them: the deleted versions fill some twenty
    // blocks the scan has to pass.
    let mut n = node_with_a_file_of_documents(41, 40);
    let now = n.now;
    let view = n.host.views().get(G).expect("the view of a range this node leads");
    let span: (&[u8], &[u8]) = (b"\x01k", b"\x01l");
    let scan = |n: &mut Node, blocks: usize, read: &mut usize| {
        let mut cache = ViewCache::default();
        let part = Part { docs: 10, backward: false, blocks };
        view.scan(
            &mut cache,
            span,
            part,
            || now,
            ReadConcern::Local,
            |need| supply_block(&mut n.disk, &view, need, read),
        )
    };
    // A block that cannot be read sends the scan to the core.
    let mut cache = ViewCache::default();
    let part = Part { docs: 10, backward: false, blocks: 64 };
    assert!(view.scan(&mut cache, span, part, || now, ReadConcern::Local, |_| false).is_none());
    // One block a part: sixteen without a document, and the scan goes to the core.
    let mut read = 0;
    assert!(scan(&mut n, 1, &mut read).is_none());
    assert_eq!(read, 16);
    // Two a part: it reads on past its two blocks, since it has no document, and finds the one.
    let mut read = 0;
    let got = scan(&mut n, 2, &mut read).expect("served outside");
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.rows[0].0, b"\x01k040");
    assert!(read > 2, "{read} blocks");
}

/// Writes `value` under `key` with `w: 1` and lets the leader answer it from its log, without
/// running its disk: the write is answered and not applied.
fn answer_early(n: &mut Node, key: &[u8], value: u8) {
    let w1 = WriteConcern { w: W::Nodes(1), j: false };
    let w = n.mutate(key, Box::new(SetKey(key.to_vec(), Some(value))), w1);
    let (now, wall) = (n.now, n.wall());
    n.host.poll(&mut n.core, now, wall);
    let answered = n.host.take_replies();
    assert!(
        answered.iter().any(|(id, r)| *id == w && r.is_ok()),
        "answered once it is in the leader's log: {answered:?}"
    );
}

#[test]
fn a_scan_outside_the_core_goes_to_it_when_it_cannot_be_served_there() {
    let mut n = Node::new();
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let span: (&[u8], &[u8]) = (b"\x01a", b"\x01y");
    // A write its client already has the answer of, before it is applied (`w: 1`, DV-010): a
    // read that comes after that answer has to see it, and the view does not hold it yet.
    answer_early(&mut n, b"\x01a", 2);
    let early = || teseladb_range::view::decline_counts()[Decline::Early as usize];
    let before = early();
    assert!(scan_outside(&n, span, 10, ReadConcern::Local).is_none());
    assert!(early() > before, "counted with its own reason");
    n.settle();
    // A span past the range, a linearizable read and no limit go to the core.
    assert!(scan_outside(&n, (b"\x01a", b"\x02"), 10, ReadConcern::Local).is_none());
    assert!(scan_outside(&n, span, 10, ReadConcern::Linearizable).is_none());
    assert!(scan_outside(&n, span, 0, ReadConcern::Local).is_none());
    // Far past what the core published as valid.
    let view = n.host.views().get(G).expect("the view");
    let late = n.now + Nanos::from_secs(60);
    let mut cache = ViewCache::default();
    let part = Part { docs: 10, backward: false, blocks: 64 };
    let r = view.scan(&mut cache, span, part, || late, ReadConcern::Local, |_| false);
    assert!(r.is_none());
    // Back to normal once the write is applied.
    let (_, rows) = scan_outside(&n, span, 10, ReadConcern::Local).expect("served outside");
    assert_eq!(rows, vec![(b"\x01a".to_vec(), (1, vec![2]))]);
}

#[test]
fn a_view_from_before_an_answered_write_was_applied_does_not_serve_a_scan_that_began_after_it() {
    // 005 FR-031: the write is answered, the scan begins and the core applies the write before
    // the scan reads. Counting the writes answered and not applied let that scan read the view
    // of before the write: the count was back at zero and the view lacked the write.
    let mut n = Node::new();
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let span: (&[u8], &[u8]) = (b"\x01a", b"\x01y");
    let old = n.host.views().get(G).expect("the view");
    let then = n.now;
    answer_early(&mut n, b"\x01a", 2);
    let begun = old.begin();
    n.settle();
    assert!(!old.holds(&begun), "the view of before the write lacks it");
    let part = Part { docs: 10, backward: false, blocks: 64 };
    let mut cache = ViewCache::default();
    let early = || teseladb_range::view::decline_counts()[Decline::Early as usize];
    let before = early();
    let read =
        old.scan_begun(&mut cache, span, (part, &begun), || then, ReadConcern::Local, |_| false);
    assert!(read.is_none() && early() > before, "{read:?}");
    // The view the core published once it applied the write serves that scan.
    let new = n.host.views().get(G).expect("the view");
    assert!(new.holds(&begun));
    let read = new
        .scan_begun(&mut cache, span, (part, &begun), || n.now, ReadConcern::Local, |_| false)
        .expect("served outside");
    assert_eq!(read.rows, vec![(b"\x01a".to_vec(), (1, vec![2]))]);
}

#[test]
fn a_scan_outside_the_core_reads_the_view_that_holds_what_was_answered_when_it_began() {
    // 005 FR-031: the writes answered after a scan began do not hold it back. Under a steady
    // flow of `w: 1` writes every view lacks the newest of them.
    let mut n = Node::new();
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let span: (&[u8], &[u8]) = (b"\x01a", b"\x01y");
    answer_early(&mut n, b"\x01a", 2);
    let begun = n.host.views().get(G).expect("the view").begin();
    n.settle();
    // Another write answered and not applied: the view holds the first and lacks this one.
    answer_early(&mut n, b"\x01b", 3);
    let view = n.host.views().get(G).expect("the view");
    let part = Part { docs: 10, backward: false, blocks: 64 };
    let mut cache = ViewCache::default();
    let read = view
        .scan_begun(&mut cache, span, (part, &begun), || n.now, ReadConcern::Local, |_| false)
        .expect("served outside");
    assert_eq!(read.rows, vec![(b"\x01a".to_vec(), (1, vec![2]))]);
    // A scan that begins now has to see the second write too.
    assert!(!view.holds(&view.begin()));
    assert!(scan_outside(&n, span, 10, ReadConcern::Local).is_none());
    n.settle();
    let (_, rows) = scan_outside(&n, span, 10, ReadConcern::Local).expect("served outside");
    let both = vec![(b"\x01a".to_vec(), (1, vec![2])), (b"\x01b".to_vec(), (1, vec![3]))];
    assert_eq!(rows, both);
}

#[test]
fn a_write_answered_once_applied_is_noted_before_the_view_that_holds_it_is_published() {
    // 005 FR-031: whoever drives the host may hand out the answer of an applied write before
    // the host's next poll publishes the view with it. A scan that begins then does not read
    // the view of before the write.
    let mut n = Node::new();
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let span: (&[u8], &[u8]) = (b"\x01a", b"\x01y");
    let w = n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(2))), MAJORITY);
    let (now, wall) = (n.now, n.wall());
    n.host.poll(&mut n.core, now, wall);
    // The core's disk and what it applied reach the host, which does not poll.
    let mut answered = Vec::new();
    for _ in 0..100 {
        while let Some(op) = n.core.poll_io(now) {
            let done = teseladb_raftlog::Disk::execute(&mut n.disk, op);
            n.core.on_io_done(done, now);
        }
        for (op, r) in n.core.drain_actions().done {
            n.host.on_done(&mut n.core, op, r);
        }
        answered.extend(n.host.take_replies());
        if !answered.is_empty() {
            break;
        }
    }
    assert!(answered.iter().any(|(id, r)| *id == w && r.is_ok()), "once applied: {answered:?}");
    let old = n.host.views().get(G).expect("the view");
    assert!(!old.holds(&old.begin()), "published before the write was applied");
    assert!(scan_outside(&n, span, 10, ReadConcern::Local).is_none());
    n.host.poll(&mut n.core, now, wall);
    let (_, rows) = scan_outside(&n, span, 10, ReadConcern::Local).expect("served outside");
    assert_eq!(rows, vec![(b"\x01a".to_vec(), (1, vec![2]))]);
}

#[test]
fn a_view_of_another_leader_of_the_range_takes_what_that_leader_has_answered() {
    // 005 FR-031: a scan that began on one leader's view and reads another leader's (a new
    // term) has to see what the new one answered by now; the entry it took belongs to a log
    // the new leader applied before it published a view.
    let mut first = Node::new();
    first.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    first.settle();
    answer_early(&mut first, b"\x01a", 2);
    let begun = first.host.views().get(G).expect("the view").begin();
    let mut second = Node::new();
    let view = second.host.views().get(G).expect("the view");
    assert!(view.holds(&begun), "nothing answered there that its view lacks");
    answer_early(&mut second, b"\x01b", 3);
    let view = second.host.views().get(G).expect("the view");
    assert!(!view.holds(&begun), "a write answered there, not applied yet");
    second.settle();
    assert!(second.host.views().get(G).expect("the view").holds(&begun));
}

#[test]
fn the_newest_versions_of_keys_are_read_on_a_view_without_a_mark() {
    // 005 FR-035: what a connection's thread prepares the evaluation of a write on.
    let mut n = Node::new();
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), Some(1))), MAJORITY);
    n.settle();
    let view = n.host.views().get(G).expect("the view");
    let mut cache = ViewCache::default();
    let keys = vec![b"\x01a".to_vec(), b"\x01b".to_vec()];
    let Newest::Versions(versions) = view.newest(&mut cache, &keys) else {
        panic!("both keys are in the memtable or nowhere")
    };
    assert_eq!(versions, vec![Some((1, vec![1])), None]);
    // A write applied since shows in the same view: the memtable it went to is shared.
    n.mutate(b"\x01b", Box::new(SetKey(b"\x01b".to_vec(), Some(7))), MAJORITY);
    n.mutate(b"\x01a", Box::new(SetKey(b"\x01a".to_vec(), None)), MAJORITY);
    n.settle();
    let Newest::Versions(versions) = view.newest(&mut cache, &keys) else {
        panic!("both keys are in the memtable")
    };
    assert_eq!(versions, vec![None, Some((1, vec![7]))]);
    // A key of another range is not this view's to give.
    assert!(matches!(view.newest(&mut cache, &[b"\x02".to_vec()]), Newest::No));
}

#[test]
fn the_views_go_where_the_node_reads_them_and_reads_outside_count_as_load_when_asked() {
    let mut n = Node::new();
    let shared = teseladb_range::view::RangeViews::default();
    n.host.set_views(shared.clone());
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(shared.get(G).is_some(), "published where the node reads it");
    // Reads that do not count for the load (a range on one node) leave it alone.
    let keys = vec![b"\x01a".to_vec(), b"\x01b".to_vec()];
    n.host.count_outside(G, (300, &keys), false, n.now);
    n.host.count_outside(G, (1, &keys), false, n.now + Nanos::from_secs(1));
    assert_eq!(n.host.load(G), 0, "no load from reads that use no core");
    // 300 reads in one second, then one more a second later closes it.
    n.host.count_outside(G, (300, &keys), true, n.now + Nanos::from_secs(2));
    n.host.count_outside(G, (1, &keys), true, n.now + Nanos::from_secs(3));
    assert!(n.host.load(G) >= 300, "{}", n.host.load(G));
    // A range this node does not lead counts nothing.
    n.host.count_outside(GroupId(99), (300, &keys), true, n.now);
}

#[test]
fn a_merge_waits_until_no_read_outside_can_take_a_mark_through_the_last_view() {
    let mut n = Node::new();
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(n.host.views().get(G).is_some());
    // Published at `n.now`: a read outside may take a mark until `n.now + VIEW_SPAN`.
    assert!(!n.host.views_settled(G, n.now));
    assert!(!n.host.views_settled(G, n.now + VIEW_SPAN - Nanos(1)));
    assert!(n.host.views_settled(G, n.now + VIEW_SPAN));
    // A range never published has nothing to wait for.
    assert!(n.host.views_settled(GroupId(99), n.now));
}

/// The index of the newest entry of the range's log on this node.
fn last_index(n: &Node) -> u64 {
    n.core.status(G).expect("the range").last_index
}

#[test]
fn a_second_write_to_a_document_goes_on_the_first_pending_one() {
    // 005 FR-029: the second write is proposed before the first is applied, evaluated on its
    // pending value; each is answered once its own entry commits.
    let mut n = Node::new();
    let before = last_index(&n);
    let w1 = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    let w2 = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    assert_eq!(last_index(&n), before + 2, "both proposed before either applied");
    assert!(n.reply(w1).is_none() && n.reply(w2).is_none(), "neither answered before it commits");
    // A read waits for the pending writes of its key.
    let r = n.get(b"\x01count", ReadConcern::Majority);
    assert!(n.reply(r).is_none(), "the read waits");
    n.settle();
    let ts = |id| match n.reply(id) {
        Some(Ok(Reply::Mutated { ts, .. })) => *ts,
        other => panic!("{other:?}"),
    };
    assert!(ts(w2) > ts(w1), "the second above the first");
    match n.reply(r) {
        Some(Ok(Reply::Value { value, .. })) => {
            assert_eq!(value.as_ref().map(|v| v.1.clone()), Some(vec![2]))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_write_refused_before_the_log_leaves_nothing_for_the_next() {
    // A mutation that fails is never proposed: the next write to the document is evaluated on
    // what is applied, not on a value that never existed.
    let mut n = Node::new();
    let w1 = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    let refused = n.mutate(b"\x01count", Box::new(Refuse), MAJORITY);
    let w3 = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(matches!(n.reply(w1), Some(Ok(Reply::Mutated { .. }))));
    assert!(
        matches!(n.reply(refused), Some(Err(RangeError::Mutation(_)))),
        "{:?}",
        n.reply(refused)
    );
    assert!(matches!(n.reply(w3), Some(Ok(Reply::Mutated { .. }))));
    let r = n.get(b"\x01count", ReadConcern::Majority);
    n.settle();
    match n.reply(r) {
        Some(Ok(Reply::Value { value, .. })) => {
            assert_eq!(value.as_ref().map(|v| v.1.clone()), Some(vec![2]))
        }
        other => panic!("{other:?}"),
    }
}

/// Fails when the counter is at one: a compare-and-swap that finds another value.
struct FailAtOne;

impl Mutation for FailAtOne {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        match current[0] {
            Some((_, [1])) => Err(MutationError("found 1".into())),
            _ => Ok(Outcome { puts: vec![], deletes: vec![], change: vec![], result: vec![] }),
        }
    }
}

#[test]
fn a_write_that_fails_on_a_pending_one_answers_only_after_it_commits() {
    // 005 FR-029: a failure is a read of what the write found; found on a pending write, it is
    // not given before that write commits (one the leader then lost would never have existed).
    let mut n = Node::new();
    let w1 = n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    let w2 = n.mutate(b"\x01count", Box::new(FailAtOne), MAJORITY);
    assert!(n.reply(w2).is_none(), "not answered on the pending value");
    n.settle();
    let at = |id| n.replies.iter().position(|(r, _)| *r == id).expect("answered");
    assert!(at(w1) < at(w2), "the failure goes after the write it found");
    assert!(matches!(n.reply(w2), Some(Err(RangeError::Mutation(_)))), "{:?}", n.reply(w2));
}
