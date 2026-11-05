//! One replica of a range over `[01, 03)` that leads on its own, driven as an embedder would,
//! for the tests of transactions on the host (008).

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::Put;
use teseladb_range::host::{
    DeleteReq, Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, TxnOp, TxnReq,
    WriteReq,
};
use teseladb_range::intents::{IntentOp, TxnId};
use teseladb_range::mutate::Outcome;
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

pub const G: GroupId = GroupId(7);
/// Rounds of settling a request may take: each lets at least 1 ms go by.
const ANSWER_ROUNDS: usize = 2_000;
pub const START: &[u8] = b"\x01";
pub const END: &[u8] = b"\x03";
pub const WALL: u64 = 1_800_000_000_000;
pub const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
pub const T1: TxnId = TxnId(11);
pub const T2: TxnId = TxnId(22);
pub const DOC: u8 = 1;
pub const A: &[u8] = b"\x01a";
pub const B: &[u8] = b"\x01b";

pub struct Node {
    pub disk: MemDisk,
    pub core: MultiRaftCore<RangeSm>,
    pub host: RangeHost,
    pub now: Nanos,
    pub replies: Vec<(ReqId, Result<Reply, RangeError>)>,
}

impl Node {
    pub fn new() -> Node {
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

    pub fn wall(&self) -> u64 {
        WALL + self.now.0 / 1_000_000
    }

    pub fn settle(&mut self) {
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

    /// Sends `op` and settles: its answer.
    pub fn txn(&mut self, op: TxnOp) -> Result<Reply, RangeError> {
        let (now, wall) = (self.now, self.wall());
        let id = self.host.txn(&mut self.core, TxnReq { g: G, op, wc: MAJORITY }, now, wall);
        self.answer(id)
    }

    /// Runs the node until request `id` is answered (a read at a mark ahead of the clock waits
    /// for it): its answer.
    pub fn answer(&mut self, id: ReqId) -> Result<Reply, RangeError> {
        for _ in 0..ANSWER_ROUNDS {
            self.settle();
            if let Some(at) = self.replies.iter().position(|(r, _)| *r == id) {
                return self.replies.remove(at).1;
            }
        }
        panic!("request {id} never answered")
    }

    pub fn write(&mut self, key: &[u8], value: &[u8]) -> Hlc {
        let (now, wall) = (self.now, self.wall());
        let m = Box::new(Set(key.to_vec(), value.to_vec()));
        let w = WriteReq { g: G, keys: vec![key.to_vec()], m, dedup: None, wc: MAJORITY };
        let id = self.host.mutate(&mut self.core, w, now, wall);
        match self.answer(id) {
            Ok(Reply::Mutated { ts, .. }) => ts,
            other => panic!("not written: {other:?}"),
        }
    }

    pub fn delete(&mut self, key: &[u8]) -> Hlc {
        let (now, wall) = (self.now, self.wall());
        let d = DeleteReq { g: G, key: key.to_vec(), dedup: None, wc: MAJORITY };
        let id = self.host.delete(&mut self.core, d, now, wall);
        match self.answer(id) {
            Ok(Reply::Mutated { ts, .. }) => ts,
            other => panic!("not deleted: {other:?}"),
        }
    }

    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let (now, wall) = (self.now, self.wall());
        let id = self.host.get(&mut self.core, G, key, ReadConcern::Majority, now, wall);
        match self.answer(id) {
            Ok(Reply::Value { value, .. }) => value.map(|(_, v)| v),
            other => panic!("not a value: {other:?}"),
        }
    }

    pub fn live(&self) -> u64 {
        self.core.state_machine(G).unwrap().live_bytes()
    }
}

/// Writes a document's value.
pub struct Set(pub Vec<u8>, pub Vec<u8>);

impl Mutation for Set {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome {
            puts: vec![Put { key: self.0.clone(), value_type: DOC, value: self.1.clone() }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

pub fn put(v: &[u8]) -> IntentOp {
    IntentOp::Put { value_type: DOC, value: v.to_vec() }
}

pub fn intents(txn: TxnId, read_ts: Hlc, writes: &[(&[u8], IntentOp)]) -> TxnOp {
    let writes = writes.iter().map(|(k, op)| (k.to_vec(), op.clone())).collect();
    TxnOp::PutIntents { txn, read_ts, writes }
}

pub fn intents_mark(r: Result<Reply, RangeError>) -> Hlc {
    match r {
        Ok(Reply::Intents { ts }) => ts,
        other => panic!("not written: {other:?}"),
    }
}

pub fn far() -> Hlc {
    Hlc { physical: u64::MAX >> 1, logical: 0 }
}
