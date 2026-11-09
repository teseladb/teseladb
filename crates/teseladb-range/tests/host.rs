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
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_range::view::FastRead;
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
        let mut disk = MemDisk::new();
        let clock = NodeClock::for_node(NodeId(1));
        let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
        let cfg = CoreConfig { clock: clock.clone(), ..CoreConfig::new(NodeId(1), 0) };
        let mut core = MultiRaftCore::new(cfg, store, 1);
        let lcfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
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
            if !moved {
                return;
            }
        }
        panic!("the node never settled");
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

#[test]
fn the_views_go_where_the_node_reads_them_and_reads_outside_count_as_load() {
    let mut n = Node::new();
    let shared = teseladb_range::view::RangeViews::default();
    n.host.set_views(shared.clone());
    n.mutate(b"\x01count", Box::new(Inc), MAJORITY);
    n.settle();
    assert!(shared.get(G).is_some(), "published where the node reads it");
    // 300 reads in one second, then one more a second later closes it.
    let keys = vec![b"\x01a".to_vec(), b"\x01b".to_vec()];
    n.host.count_outside(G, 300, &keys, n.now);
    n.host.count_outside(G, 1, &keys, n.now + Nanos::from_secs(1));
    assert!(n.host.load(G) >= 300, "{}", n.host.load(G));
    // A range this node does not lead counts nothing.
    n.host.count_outside(GroupId(99), 300, &keys, n.now);
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
