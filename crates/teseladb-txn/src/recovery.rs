//! The decision on a client transaction found from another node (008): a `commitTransaction` that
//! reaches a node without the transaction, with its `recoveryToken`, as a second `refrouter` gets it
//! from the shard. A client transaction's identifier comes from its session and number
//! ([`client_txn_id`]), so any node finds its record, which Raft keeps in its range. The recovery
//! pushes the record with [`Push::Abort`]: a committed one answers committed, a running one
//! aborts, as a shard of a `refrouter` 9.0.4 aborts it (measured), and one that does not exist is
//! created aborted, so that the transaction's coordinator, if it is still alive, cannot begin or
//! commit it after the answer.
//!
//! A transaction that held its writes until the commit and committed them in one phase has no
//! record (008 FR-022): its decision is the idempotence record that commit left in its range. So
//! the recovery first reads the record; when there is none it asks every range with
//! `DecideOnePhase`, which answers the commit's mark where it committed and elsewhere leaves the
//! record that refuses the commit from then on. It asks again the ranges that did not answer and
//! those that appeared meanwhile (a split copies the records to the new range), and only once
//! every range refused it pushes the record, which is created aborted.
//!
//! Two transactions get the same identifier only if SHA-256 gives the same first 128 bits for two
//! different inputs: with `n` client transactions the chance is at most `n² / 2^129`, about
//! `10^-21` for ten thousand million of them, the same bound as the random identifiers of the
//! other transactions, with which they share the space.

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{Push, TxnId, TxnRecord, TxnStatus, record_key};

use std::collections::{BTreeMap, BTreeSet};

use teseladb_types::GroupId;

use crate::coordinator::{ReqNo, Request, Route};

/// The read mark of a record no coordinator began.
const NEVER_BEGUN: Hlc = Hlc { physical: 0, logical: 0 };

/// What the hash of a client transaction's identifier starts with, so that no other use of the
/// same bytes gives the same value.
const DOMAIN: &[u8] = b"teseladb client transaction";

/// The identifier of client transaction `txn_number` of session `lsid`: the first 128 bits of the
/// SHA-256 of [`DOMAIN`], the session and the number.
pub fn client_txn_id(lsid: u128, txn_number: i64) -> TxnId {
    let input = [DOMAIN, &lsid.to_be_bytes(), &txn_number.to_be_bytes()].concat();
    let digest = teseladb_hash::sha256(&input);
    let mut first = [0; 16];
    first.copy_from_slice(&digest[..16]);
    TxnId(u128::from_be_bytes(first))
}

/// What the recovery found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// It committed at this mark.
    Committed(Hlc),
    /// It aborted, or it was running and the recovery aborted it.
    Aborted,
    /// The session never began a transaction of that number: the recovery's record stops it.
    Unknown,
}

/// The recovery of one transaction's decision, as a sans-I/O state machine: its embedder sends
/// [`Recovery::take_requests`] and hands back the answers, and calls [`Recovery::tick`] to send
/// again a request that failed.
#[derive(Debug)]
pub struct Recovery {
    txn: TxnId,
    lsid: u128,
    txn_number: i64,
    out: Vec<Request>,
    waiting: Option<ReqNo>,
    next: ReqNo,
    decision: Option<Decision>,
    step: Step,
    /// The ranges that refused the one-phase commit.
    refused: BTreeSet<GroupId>,
    /// The requests to ranges in flight, with their range.
    asking: BTreeMap<ReqNo, GroupId>,
}

/// Where the recovery is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// It reads the record.
    Read,
    /// No record: it asks every range for a one-phase commit.
    Ranges,
    /// It pushes the record.
    Push,
}

impl Recovery {
    /// The recovery of client transaction `txn_number` of session `lsid`; its first request goes
    /// at the first [`Recovery::tick`].
    pub fn new(lsid: u128, txn_number: i64) -> Recovery {
        // Faulty variant (008): the recovery looks for another number's record, as if the
        // transaction's identifier did not come from its session and number.
        #[cfg(teseladb_bug = "decision_not_durable")]
        let txn_number = txn_number ^ 1;
        Recovery {
            txn: client_txn_id(lsid, txn_number),
            lsid,
            txn_number,
            out: Vec::new(),
            waiting: None,
            next: 0,
            decision: None,
            step: Step::Read,
            refused: BTreeSet::new(),
            asking: BTreeMap::new(),
        }
    }

    /// Sends the push to the record's range, if no answer came yet and none is in flight; a
    /// range it does not know yet waits for the next tick.
    pub fn tick(&mut self, route: &dyn Route) {
        if self.decision.is_some() || self.waiting.is_some() || !self.asking.is_empty() {
            return;
        }
        if self.step == Step::Ranges {
            // The ranges still to ask, those that appeared since the last round among them.
            let left: Vec<(GroupId, Vec<u8>)> =
                route.ranges().into_iter().filter(|(g, _)| !self.refused.contains(g)).collect();
            for (g, at) in left {
                let id = self.next;
                self.next += 1;
                self.asking.insert(id, g);
                let (lsid, txn_number) = (self.lsid, self.txn_number);
                self.out.push(Request {
                    id,
                    g,
                    op: TxnOp::DecideOnePhase { lsid, txn_number, at },
                });
            }
            if !self.asking.is_empty() {
                return;
            }
            // Every range refused the commit: the record can be created aborted.
            self.step = Step::Push;
        }
        let Some(g) = route.range_of(&record_key(self.txn)) else { return };
        let id = self.next;
        self.next += 1;
        self.waiting = Some(id);
        let op = match self.step {
            Step::Read => TxnOp::ReadRecord { txn: self.txn },
            Step::Ranges | Step::Push => {
                let push = Push::Abort { lsid: self.lsid, txn_number: self.txn_number };
                TxnOp::PushTxn { txn: self.txn, push }
            }
        };
        self.out.push(Request { id, g, op });
    }

    /// The requests to send.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.out)
    }

    /// The answer to request `id`; a refusal (another leader, a range that moved, a timeout) is
    /// sent again at the next tick.
    pub fn on_reply(&mut self, id: ReqNo, r: Result<Reply, RangeError>) {
        if let Some(g) = self.asking.remove(&id) {
            match r {
                // It committed in one phase in this range.
                Ok(Reply::Intents { ts }) => {
                    self.decision = Some(Decision::Committed(ts));
                    self.asking.clear();
                }
                // Refused now or before: no commit of it comes to this range.
                Ok(Reply::Batch(_)) | Err(RangeError::NoSuchTransaction) => {
                    self.refused.insert(g);
                }
                // Another leader, a range that moved, a timeout: asked again at the next tick.
                Ok(_) | Err(_) => {}
            }
            return;
        }
        if self.waiting != Some(id) {
            return;
        }
        self.waiting = None;
        match (self.step, r) {
            // A record: the push reads its decision, as before.
            (Step::Read, Ok(Reply::Record(_))) => self.step = Step::Push,
            // None: a one-phase commit may have left its decision in a range.
            (Step::Read, Err(RangeError::NoSuchTransaction)) => self.step = Step::Ranges,
            (Step::Push, Ok(Reply::Record(rec))) => self.decision = Some(Self::decide(&rec)),
            (Step::Read | Step::Ranges | Step::Push, _) => {}
        }
    }

    /// A record that never had a read mark was never begun: the push created it, or a reader's
    /// push before the coordinator's `Begin`.
    fn decide(rec: &TxnRecord) -> Decision {
        match rec.status {
            TxnStatus::Committed => Decision::Committed(rec.commit_ts),
            TxnStatus::Aborted if rec.read_ts != NEVER_BEGUN => Decision::Aborted,
            TxnStatus::Aborted | TxnStatus::Pending => Decision::Unknown,
        }
    }

    /// What it found, once it found it.
    pub fn decision(&self) -> Option<Decision> {
        self.decision
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_types::GroupId;

    struct One;

    impl Route for One {
        fn range_of(&self, _: &[u8]) -> Option<GroupId> {
            Some(GroupId(4))
        }
    }

    struct Nowhere;

    impl Route for Nowhere {
        fn range_of(&self, _: &[u8]) -> Option<GroupId> {
            None
        }
    }

    const ZERO: Hlc = Hlc { physical: 0, logical: 0 };

    fn record(txn: TxnId, read_ts: Hlc, status: TxnStatus, commit_ts: Hlc) -> TxnRecord {
        TxnRecord {
            txn,
            lsid: 7,
            txn_number: 3,
            status,
            read_ts,
            min_commit_ts: ZERO,
            commit_ts,
            heartbeat_ts: ZERO,
            spans: Vec::new(),
        }
    }

    #[test]
    fn a_client_transaction_has_one_identifier_per_session_and_number() {
        let a = client_txn_id(7, 3);
        assert_eq!(a, client_txn_id(7, 3));
        assert_ne!(a, client_txn_id(7, 4));
        assert_ne!(a, client_txn_id(8, 3));
        assert_ne!(client_txn_id(0, 0), TxnId(0));
    }

    #[test]
    fn the_recovery_pushes_the_record_and_reads_its_decision() {
        let txn = client_txn_id(7, 3);
        let at = Hlc { physical: 9, logical: 1 };
        let cases = [
            (record(txn, at, TxnStatus::Committed, at), Decision::Committed(at)),
            (record(txn, at, TxnStatus::Aborted, ZERO), Decision::Aborted),
            (record(txn, ZERO, TxnStatus::Aborted, ZERO), Decision::Unknown),
        ];
        for (rec, want) in cases {
            let mut r = Recovery::new(7, 3);
            // It reads the record first, and pushes the one it finds.
            r.tick(&One);
            let read = r.take_requests();
            assert!(matches!(read[..], [Request { op: TxnOp::ReadRecord { .. }, .. }]), "{read:?}");
            r.on_reply(read[0].id, Ok(Reply::Record(rec.clone())));
            assert_eq!(r.decision(), None);
            r.tick(&One);
            let reqs = r.take_requests();
            let [Request { id, g: GroupId(4), op: TxnOp::PushTxn { txn: t, push } }] = reqs[..]
            else {
                panic!("{reqs:?}")
            };
            assert_eq!(t, txn);
            assert_eq!(push, Push::Abort { lsid: 7, txn_number: 3 });
            r.tick(&One);
            assert!(r.take_requests().is_empty(), "one in flight");
            r.on_reply(id + 1, Ok(Reply::Record(rec.clone())));
            assert_eq!(r.decision(), None, "not its answer");
            r.on_reply(id, Ok(Reply::Record(rec)));
            assert_eq!(r.decision(), Some(want));
            r.tick(&One);
            assert!(r.take_requests().is_empty(), "decided");
        }
    }

    #[test]
    fn a_refused_push_goes_again_where_the_record_is_now() {
        let mut r = Recovery::new(7, 3);
        r.tick(&Nowhere);
        assert!(r.take_requests().is_empty(), "no range known yet");
        r.tick(&One);
        let id = r.take_requests()[0].id;
        r.on_reply(id, Err(RangeError::NotLeader { hint: teseladb_types::NodeId(2) }));
        assert_eq!(r.decision(), None);
        r.tick(&One);
        let again = r.take_requests();
        assert_eq!(again.len(), 1);
        r.on_reply(again[0].id, Ok(Reply::Intents { ts: ZERO }));
        assert_eq!(r.decision(), None, "an answer that is no record");
    }

    /// Ranges 4 (the records) and 5 to 5 + `extra`, each with a key it holds.
    struct Ranges(std::cell::Cell<u64>);

    impl Route for Ranges {
        fn range_of(&self, _: &[u8]) -> Option<GroupId> {
            Some(GroupId(4))
        }

        fn ranges(&self) -> Vec<(GroupId, Vec<u8>)> {
            (4..=5 + self.0.get()).map(|g| (GroupId(g), vec![g as u8])).collect()
        }
    }

    /// The requests of one tick, which reads no record first.
    fn asked(r: &mut Recovery, route: &Ranges) -> Vec<Request> {
        r.tick(route);
        r.take_requests()
    }

    fn without_record(route: &Ranges) -> Recovery {
        let mut r = Recovery::new(7, 3);
        let read = asked(&mut r, route);
        r.on_reply(read[0].id, Err(RangeError::NoSuchTransaction));
        r
    }

    #[test]
    fn without_a_record_every_range_is_asked_before_the_record_is_created_aborted() {
        let route = Ranges(std::cell::Cell::new(0));
        let mut r = without_record(&route);
        let reqs = asked(&mut r, &route);
        let places: Vec<(GroupId, &TxnOp)> = reqs.iter().map(|q| (q.g, &q.op)).collect();
        let decide = |g: u64| TxnOp::DecideOnePhase { lsid: 7, txn_number: 3, at: vec![g as u8] };
        assert_eq!(places, [(GroupId(4), &decide(4)), (GroupId(5), &decide(5))]);
        // One refuses, the other does not answer: it is asked again, alone.
        r.on_reply(reqs[0].id, Ok(Reply::Batch(Vec::new())));
        r.on_reply(reqs[1].id, Err(RangeError::Timeout));
        let again = asked(&mut r, &route);
        assert_eq!(again.iter().map(|q| q.g).collect::<Vec<_>>(), [GroupId(5)]);
        // A split made a range meanwhile: it is asked too before anything is decided.
        route.0.set(1);
        r.on_reply(again[0].id, Err(RangeError::NoSuchTransaction));
        let new = asked(&mut r, &route);
        assert_eq!(new.iter().map(|q| q.g).collect::<Vec<_>>(), [GroupId(6)]);
        r.on_reply(new[0].id, Ok(Reply::Batch(Vec::new())));
        assert_eq!(r.decision(), None);
        // Every range refused the commit: now the push creates the record aborted.
        let push = asked(&mut r, &route);
        assert!(matches!(push[..], [Request { op: TxnOp::PushTxn { .. }, .. }]), "{push:?}");
        let created = record(client_txn_id(7, 3), ZERO, TxnStatus::Aborted, ZERO);
        r.on_reply(push[0].id, Ok(Reply::Record(created)));
        assert_eq!(r.decision(), Some(Decision::Unknown));
    }

    #[test]
    fn a_range_that_holds_the_one_phase_commit_decides_it() {
        let route = Ranges(std::cell::Cell::new(0));
        let mut r = without_record(&route);
        let reqs = asked(&mut r, &route);
        let at = Hlc { physical: 40, logical: 2 };
        r.on_reply(reqs[1].id, Ok(Reply::Intents { ts: at }));
        assert_eq!(r.decision(), Some(Decision::Committed(at)));
        // The other range's answer changes nothing, and nothing else is sent.
        r.on_reply(reqs[0].id, Ok(Reply::Batch(Vec::new())));
        assert_eq!(r.decision(), Some(Decision::Committed(at)));
        assert!(asked(&mut r, &route).is_empty());
    }
}
