//! `splits` workload (007 T016, FR-023, SC-006): the ranges of `multirange` split while
//! transactions of 008 write intents on both sides of the cuts. Its single-document clients and
//! its splits are those of `multirange`, without merges, over a space that also holds the
//! transaction records (`[01, 03)`); the cuts fall among the data keys and among the records, so
//! a transaction's record may move to another range while the transaction runs (its coordinator
//! sends again what a range answers `OutOfRange`).
//!
//! - A writer runs one transaction at a time with the coordinator of `teseladb-txn`: it writes its
//!   tag on two keys, usually in two ranges, and commits.
//! - A reader reads every transaction key at one mark; a read that meets an intent pushes its
//!   transaction above the mark, learns its state and reads again, resolving the intents of a
//!   finished one.
//!
//! The `intents` checker compares what the readers saw with how each transaction ended: a read at
//! a mark sees, on each key, the write of the newest transaction committed at or below it, never
//! one of a transaction that did not commit nor one committed above the mark. A split that loses
//! an intent leaves a committed write invisible. The history of the single documents goes to the
//! progress, durability and linearizability checkers.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_raftlog::Completions;
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::{RangeError, ReadSpec, Reply, ReqId, TxnOp, TxnReq};
use teseladb_range::intents::{IntentOp, TxnId};
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_txn::coordinator::{Coordinator, Event, Outcome, ReqNo, Route};
use teseladb_txn::read::{lazy_resolve, learn, push_for};
use teseladb_types::{GroupId, NodeId};

use crate::checkers::Violation;
use crate::history::History;
use crate::workloads::Workload;
use crate::workloads::multirange::{CLIENTS, MultiRangeWorkload, Profile};
use crate::world::World;

/// Name of the checker in its violations.
pub const NAME: &str = "intents";
/// The space the ranges split: data keys and, past `02`, the transaction records.
const PROFILE: Profile = Profile {
    name: "splits",
    end: b"\x03",
    merges: false,
    max_ranges: 8,
    cuts: &[1, 2],
    adverse: false,
};
/// Keys the transactions write, spread over the data keys and never a single document's.
const TXN_KEYS: u64 = 8;
/// Writers and readers of transactions.
const TXN_CLIENTS: u32 = 4;
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
/// Value type of a transaction's write.
const VALUE_TYPE: u8 = 1;

fn txn_key(j: u64) -> Vec<u8> {
    vec![1, (j * 32 + 4) as u8]
}

fn tag_value(tag: u64) -> Vec<u8> {
    format!("t{tag}").into_bytes()
}

fn tag_of(value: &[u8]) -> Option<u64> {
    std::str::from_utf8(value).ok()?.strip_prefix('t')?.parse().ok()
}

/// How a writer's transaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Committed(Hlc),
    /// It never committed: aborted, or refused before its commit.
    Not,
    /// The commit's answer was lost, or the run ended first.
    Unknown,
}

/// What a reader saw: its mark, the key and the tag there (`None`: absent).
type Seen = (Hlc, Vec<u8>, Option<u64>);

/// A writer's transaction.
struct Writer {
    coord: Coordinator,
    node: u32,
    tag: u64,
}

/// A reader going through the keys.
struct Reader {
    at: Hlc,
    next: u64,
    view: TxnView,
}

/// Who waits for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asker {
    Coord(u32, ReqNo),
    Read(u32),
    Push(u32),
    /// A lazy resolution nobody waits for.
    Nobody,
}

/// The table of ranges, as the coordinator asks it.
struct Table<'a>(&'a MultiRangeWorkload);

impl Route for Table<'_> {
    fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        self.0.range_of(key)
    }
}

/// The `splits` workload.
pub struct SplitsWorkload {
    engine: MultiRangeWorkload,
    writers: BTreeMap<u32, Writer>,
    readers: BTreeMap<u32, Reader>,
    pending: BTreeMap<(u32, ReqId), Asker>,
    next_tag: u64,
    /// How each writer's transaction ended, by tag.
    fates: BTreeMap<u64, Fate>,
    /// The keys each tag was written on.
    keys: BTreeMap<u64, Vec<Vec<u8>>>,
    seen: Vec<Seen>,
}

impl SplitsWorkload {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(SplitsWorkload::new(nodes, cores))
    }

    fn new(nodes: u32, cores: u16) -> SplitsWorkload {
        SplitsWorkload {
            engine: MultiRangeWorkload::new(nodes, cores, PROFILE),
            writers: BTreeMap::new(),
            readers: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_tag: 1,
            fates: BTreeMap::new(),
            keys: BTreeMap::new(),
            seen: Vec::new(),
        }
    }

    /// The node that leads the range of `key`, with its host.
    fn leader_of(&mut self, w: &mut World, key: &[u8]) -> Option<u32> {
        let g = self.engine.range_of(key)?;
        self.engine.leader(w, g).filter(|n| self.engine.host(*n).is_some())
    }

    /// Starts a client's transaction: a reader one time in three, a writer otherwise.
    fn begin(&mut self, w: &mut World, client: u32) {
        let read = w.rng.chance(0.35);
        let a = w.rng.range(0, TXN_KEYS);
        let b = (a + 1 + w.rng.range(0, TXN_KEYS - 1)) % TXN_KEYS;
        // The mark comes from the leader of the first range the transaction touches, as a client
        // takes the cluster time from the server it asks: a leader gives marks under the clock
        // rule (005 FR-006). A node out of margin or cut off from its peers gives none here, which
        // would carry the HLC of the ranges it reads ahead of every clock.
        let first = txn_key(if read { 0 } else { a });
        let Some(node) = self.leader_of(w, &first) else { return };
        let local = w.local(node);
        let core = self.engine.core();
        let Some(at) = w.cluster_core(node, core).and_then(|c| c.read_mark(local)) else { return };
        if read {
            let view = TxnView { own: None, known: BTreeMap::new() };
            self.readers.insert(client, Reader { at, next: 0, view });
            return self.read_next(w, client);
        }
        let tag = self.next_tag;
        self.next_tag += 1;
        self.fates.insert(tag, Fate::Unknown);
        self.keys.insert(tag, vec![txn_key(a), txn_key(b)]);
        // The tag's low byte goes first, so that the records spread over the cuts of `02`.
        let txn = TxnId(u128::from(tag).rotate_right(8));
        let mut coord = Coordinator::new(txn, (u128::from(client) + 1, 1), at, local);
        let put = || IntentOp::Put { value_type: VALUE_TYPE, value: tag_value(tag) };
        let writes = vec![(txn_key(a), put()), (txn_key(b), put())];
        if coord.write(&Table(&self.engine), writes).is_err() {
            self.fates.insert(tag, Fate::Not);
            return;
        }
        self.writers.insert(client, Writer { coord, node, tag });
        self.flush(w, client);
    }

    /// Sends `op` for `g` to its leader; with none, the asker gets `NotLeader` at once.
    fn send(
        &mut self,
        w: &mut World,
        g: GroupId,
        op: TxnOp,
        asker: Asker,
    ) -> Option<(Asker, Result<Reply, RangeError>)> {
        let not_leader = Err(RangeError::NotLeader { hint: NodeId(0) });
        let Some(node) = self.engine.leader(w, g) else { return Some((asker, not_leader)) };
        let (now, wall) = MultiRangeWorkload::times(w, node);
        let core = self.engine.core();
        let (Some(host), Some(c)) = (self.engine.host(node), w.cluster_core(node, core)) else {
            return Some((asker, not_leader));
        };
        let id = host.txn(c, TxnReq { g, op, wc: MAJORITY }, now, wall);
        self.pending.insert((node, id), asker);
        self.engine.pump(w, node);
        w.settle(node, core);
        None
    }

    /// Sends the coordinator's requests and handles its events.
    fn flush(&mut self, w: &mut World, client: u32) {
        loop {
            let Some(wr) = self.writers.get_mut(&client) else { return };
            let reqs = wr.coord.take_requests();
            let events = wr.coord.take_events();
            if reqs.is_empty() && events.is_empty() {
                return;
            }
            let mut now_answers = Vec::new();
            for r in reqs {
                if let Some(a) = self.send(w, r.g, r.op, Asker::Coord(client, r.id)) {
                    now_answers.push(a);
                }
            }
            for e in events {
                self.event(w, client, e);
            }
            for (asker, r) in now_answers {
                self.answer(w, asker, r);
            }
        }
    }

    fn event(&mut self, w: &mut World, client: u32, e: Event) {
        let Some(node) = self.writers.get(&client).map(|wr| wr.node) else { return };
        // Its node's clock as the rule lets it give a mark; without one, the intents' marks.
        let (local, core) = (w.local(node), self.engine.core());
        let clock = w.cluster_core(node, core).and_then(|c| c.read_mark(local));
        let wr = self.writers.get_mut(&client).expect("the writer found above");
        match e {
            Event::Written { result: Ok(()), .. } => {
                wr.coord.commit(clock.unwrap_or(Hlc { physical: 0, logical: 0 }));
            }
            Event::Written { result: Err(_), .. } => wr.coord.abort(),
            Event::Ended(outcome) => {
                let fate = match outcome {
                    Outcome::Committed(ts) => Fate::Committed(ts),
                    Outcome::Aborted | Outcome::Failed(_) => Fate::Not,
                    Outcome::Unknown(_) => Fate::Unknown,
                };
                self.fates.insert(wr.tag, fate);
                // Its resolutions go on without it; readers resolve what they leave.
                self.writers.remove(&client);
                self.pending.retain(|_, a| !matches!(a, Asker::Coord(c, _) if *c == client));
            }
            Event::Resolved => {}
        }
    }

    /// Reads the reader's next key, or ends it after the last one.
    fn read_next(&mut self, w: &mut World, client: u32) {
        let Some(r) = self.readers.get(&client) else { return };
        if r.next == TXN_KEYS {
            self.readers.remove(&client);
            return;
        }
        let key = txn_key(r.next);
        let spec = |g| ReadSpec {
            g,
            key: key.clone(),
            scan: None,
            at: Some(r.at),
            concern: ReadConcern::Majority,
            view: r.view.clone(),
            uncertain_to: None,
        };
        let leader = self.engine.range_of(&key).and_then(|g| Some((g, self.engine.leader(w, g)?)));
        let Some((g, node)) = leader else {
            self.readers.remove(&client);
            return;
        };
        let spec = spec(g);
        let (now, wall) = MultiRangeWorkload::times(w, node);
        let core = self.engine.core();
        let (Some(host), Some(c)) = (self.engine.host(node), w.cluster_core(node, core)) else {
            self.readers.remove(&client);
            return;
        };
        let id = host.read_with(c, spec, now, wall);
        self.pending.insert((node, id), Asker::Read(client));
        self.engine.pump(w, node);
        w.settle(node, core);
    }

    fn read_done(&mut self, w: &mut World, client: u32, r: Result<Reply, RangeError>) {
        let Some(rd) = self.readers.get_mut(&client) else { return };
        let key = txn_key(rd.next);
        match r {
            Ok(Reply::Value { value, .. }) => {
                let tag = value.map(|(_, v)| tag_of(&v).unwrap_or(0));
                self.seen.push((rd.at, key, tag));
                rd.next += 1;
                self.read_next(w, client);
            }
            Err(RangeError::ReadIntent { owner, .. }) => {
                let op = push_for(&owner, rd.at);
                let Some(g) = self.engine.range_of(&owner.record_key) else {
                    self.readers.remove(&client);
                    return;
                };
                if let Some((asker, r)) = self.send(w, g, op, Asker::Push(client)) {
                    self.answer(w, asker, r);
                }
            }
            Ok(_) | Err(_) => {
                self.readers.remove(&client);
            }
        }
    }

    fn push_done(&mut self, w: &mut World, client: u32, r: Result<Reply, RangeError>) {
        let Some(rd) = self.readers.get_mut(&client) else { return };
        let Ok(Reply::Record(rec)) = r else {
            self.readers.remove(&client);
            return;
        };
        rd.view.known.insert(rec.txn, learn(&rec));
        let key = txn_key(rd.next);
        if let Some(op) = lazy_resolve(&rec, vec![key.clone()])
            && let Some(g) = self.engine.range_of(&key)
        {
            let _ = self.send(w, g, op, Asker::Nobody);
        }
        self.read_next(w, client);
    }

    fn answer(&mut self, w: &mut World, asker: Asker, r: Result<Reply, RangeError>) {
        match asker {
            Asker::Nobody => {}
            Asker::Coord(client, no) => {
                if let Some(wr) = self.writers.get_mut(&client) {
                    wr.coord.on_reply(&Table(&self.engine), no, r);
                    self.flush(w, client);
                }
            }
            Asker::Read(client) => self.read_done(w, client, r),
            Asker::Push(client) => self.push_done(w, client, r),
        }
    }

    /// Hands the transactions the answers the hosts gave them, and lets the coordinators beat.
    fn drain(&mut self, w: &mut World) {
        let clients: Vec<u32> = self.writers.keys().copied().collect();
        for client in clients {
            if let Some(wr) = self.writers.get_mut(&client) {
                let (now, wall) = (w.local(wr.node), w.wall_ms(wr.node));
                wr.coord.tick(&Table(&self.engine), now, wall);
            }
            self.flush(w, client);
        }
        while !self.engine.foreign.is_empty() {
            for (node, req, r) in std::mem::take(&mut self.engine.foreign) {
                if let Some(a) = self.pending.remove(&(node, req)) {
                    self.answer(w, a, r);
                }
            }
        }
    }

    /// The `intents` checker over what the readers saw.
    fn check(&self) -> Result<(), Violation> {
        // The committed writes of each key, by commit mark.
        let mut writes: BTreeMap<Vec<u8>, Vec<(Hlc, u64)>> = BTreeMap::new();
        for (tag, fate) in &self.fates {
            if let Fate::Committed(ts) = fate {
                for key in self.keys_of(*tag) {
                    writes.entry(key).or_default().push((*ts, *tag));
                }
            }
        }
        for (at, key, tag) in &self.seen {
            let fate = tag.map(|t| (t, self.fates.get(&t).copied().unwrap_or(Fate::Not)));
            if let Some((t, Fate::Not)) = fate {
                return Err(Violation::safety(
                    NAME,
                    format!(
                        "a read at {at:?} sees on {key:?} the write of t{t}, which never committed"
                    ),
                ));
            }
            if let Some((t, Fate::Committed(ts))) = fate
                && ts > *at
            {
                return Err(Violation::safety(
                    NAME,
                    format!("a read at {at:?} sees on {key:?} t{t}, committed above it at {ts:?}"),
                ));
            }
            let newest = writes.get(key).and_then(|v| v.iter().filter(|(ts, _)| ts <= at).max());
            let Some(&(want_ts, want)) = newest else { continue };
            let lost = match fate {
                None => true,
                Some((_, Fate::Committed(ts))) => ts < want_ts,
                Some((_, Fate::Unknown | Fate::Not)) => false,
            };
            if lost {
                return Err(Violation::safety(
                    NAME,
                    format!(
                        "a read at {at:?} sees on {key:?} {tag:?} and not t{want}, committed at \
                         {want_ts:?}: its intent was lost"
                    ),
                ));
            }
        }
        let committed = self.fates.values().any(|f| matches!(f, Fate::Committed(_)));
        if !committed || self.seen.is_empty() {
            return Err(Violation::liveness(NAME, "no transaction committed or no read ended"));
        }
        Ok(())
    }

    /// The keys tag `tag` was written on.
    fn keys_of(&self, tag: u64) -> Vec<Vec<u8>> {
        self.keys.get(&tag).cloned().unwrap_or_default()
    }
}

impl Workload for SplitsWorkload {
    fn name(&self) -> &'static str {
        PROFILE.name
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        self.engine.start(w);
        for client in CLIENTS..CLIENTS + TXN_CLIENTS {
            MultiRangeWorkload::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client < CLIENTS {
            self.engine.on_client(w, client);
        } else {
            let busy = self.writers.contains_key(&client) || self.readers.contains_key(&client);
            if !busy {
                self.begin(w, client);
            }
            MultiRangeWorkload::next_turn(w, client);
        }
        self.drain(w);
    }

    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.engine.on_completions(w, node, core, c);
        self.drain(w);
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.engine.on_crash(w, node);
        self.drain(w);
        let lost: Vec<Asker> =
            self.pending.iter().filter(|((n, _), _)| *n == node).map(|(_, a)| *a).collect();
        self.pending.retain(|(n, _), _| *n != node);
        for asker in lost {
            self.answer(w, asker, Err(RangeError::Unavailable));
        }
        self.drain(w);
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.engine.on_restart(w, node)
    }

    fn on_heal(&mut self, w: &mut World) {
        self.engine.on_heal(w);
    }

    fn history(&self) -> Option<&History> {
        self.engine.history()
    }

    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        self.engine.finish(w)?;
        self.check()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_range::intents::IntentOwner;

    fn at(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    /// Tag 1 committed at 10 and tag 2 at 20, both on key 0; tag 3 never committed; tag 4's end
    /// is unknown.
    fn workload(seen: Vec<Seen>) -> SplitsWorkload {
        let mut wl = SplitsWorkload::new(3, 2);
        for (tag, fate) in [
            (1, Fate::Committed(at(10))),
            (2, Fate::Committed(at(20))),
            (3, Fate::Not),
            (4, Fate::Unknown),
        ] {
            wl.fates.insert(tag, fate);
            wl.keys.insert(tag, vec![txn_key(0), txn_key(1)]);
        }
        wl.seen = seen;
        wl
    }

    fn checker(seen: Vec<Seen>) -> Result<(), String> {
        workload(seen).check().map_err(|v| v.detail)
    }

    #[test]
    fn a_read_sees_the_newest_commit_at_or_below_its_mark() {
        let k = txn_key(0);
        assert_eq!(
            checker(vec![(at(15), k.clone(), Some(1)), (at(25), k.clone(), Some(2))]),
            Ok(())
        );
        assert_eq!(checker(vec![(at(5), k.clone(), None)]), Ok(()), "before any commit");
        // A transaction whose end is unknown may be what the read saw.
        assert_eq!(checker(vec![(at(25), k.clone(), Some(4))]), Ok(()));
        assert_eq!(checker(vec![(at(25), txn_key(5), None)]), Ok(()), "a key nobody wrote");
    }

    #[test]
    fn a_lost_or_foreign_write_is_a_violation() {
        let k = txn_key(0);
        let lost = checker(vec![(at(25), k.clone(), Some(1))]).unwrap_err();
        assert!(lost.contains("not t2") && lost.contains("lost"), "{lost}");
        let gone = checker(vec![(at(25), k.clone(), None)]).unwrap_err();
        assert!(gone.contains("not t2"), "{gone}");
        let aborted = checker(vec![(at(25), k.clone(), Some(3))]).unwrap_err();
        assert!(aborted.contains("never committed"), "{aborted}");
        let unknown_tag = checker(vec![(at(25), k.clone(), Some(99))]).unwrap_err();
        assert!(unknown_tag.contains("never committed"), "{unknown_tag}");
        let above = checker(vec![(at(15), k, Some(2))]).unwrap_err();
        assert!(above.contains("committed above it"), "{above}");
    }

    #[test]
    fn without_a_commit_or_a_read_it_is_a_liveness_violation() {
        let v = workload(Vec::new()).check().unwrap_err();
        assert!(v.liveness);
        let mut wl = workload(vec![(at(25), txn_key(0), None)]);
        wl.fates.retain(|_, f| !matches!(f, Fate::Committed(_)));
        assert!(wl.check().unwrap_err().liveness);
    }

    #[test]
    fn answers_and_events_for_clients_that_left_change_nothing() {
        let mut w = World::new(1, 3, 2, false);
        let mut wl = SplitsWorkload::new(3, 2);
        // No writer, reader or pusher is client 7 any more.
        wl.event(&mut w, 7, Event::Resolved);
        wl.read_next(&mut w, 7);
        wl.read_done(&mut w, 7, Err(RangeError::Unavailable));
        wl.push_done(&mut w, 7, Err(RangeError::Unavailable));
        wl.answer(&mut w, Asker::Coord(7, 1), Err(RangeError::Unavailable));
        assert!(wl.writers.is_empty() && wl.readers.is_empty() && wl.pending.is_empty());
    }

    #[test]
    fn a_reader_stopped_by_an_intent_whose_record_no_range_holds_ends() {
        let mut w = World::new(1, 3, 2, false);
        let mut wl = SplitsWorkload::new(3, 2);
        wl.readers.insert(7, Reader { at: at(30), next: 0, view: TxnView::default() });
        // A record outside the data space: no range holds it.
        let owner =
            IntentOwner { txn: TxnId(1), record_key: vec![0xff], key: txn_key(0), ts: at(20) };
        wl.read_done(&mut w, 7, Err(RangeError::ReadIntent { owner, at: at(30) }));
        assert!(wl.readers.is_empty());
    }

    #[test]
    fn tags_round_trip_and_other_values_are_no_tag() {
        assert_eq!(tag_of(&tag_value(42)), Some(42));
        assert_eq!(tag_of(b"x42"), None);
        assert_eq!(tag_of(&[0xff]), None);
    }
}
