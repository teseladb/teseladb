//! The decision on a client transaction found from another node (008): a `commitTransaction` that
//! reaches a node without the transaction, with its `recoveryToken`, as a second `refrouter` gets it
//! from the shard. A client transaction's identifier comes from its session and number
//! ([`client_txn_id`]), so any node finds its record, which Raft keeps in its range. The recovery
//! pushes the record with [`Push::Abort`]: a committed one answers committed, a running one
//! aborts, as a shard of a `refrouter` 9.0.4 aborts it (measured), and one that does not exist is
//! created aborted, so that the transaction's coordinator, if it is still alive, cannot begin or
//! commit it after the answer.
//!
//! Two transactions get the same identifier only if SHA-256 gives the same first 128 bits for two
//! different inputs: with `n` client transactions the chance is at most `n² / 2^129`, about
//! `10^-21` for ten thousand million of them, the same bound as the random identifiers of the
//! other transactions, with which they share the space.

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{Push, TxnId, TxnRecord, TxnStatus, record_key};

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
        }
    }

    /// Sends the push to the record's range, if no answer came yet and none is in flight; a
    /// range it does not know yet waits for the next tick.
    pub fn tick(&mut self, route: &dyn Route) {
        if self.decision.is_some() || self.waiting.is_some() {
            return;
        }
        let Some(g) = route.range_of(&record_key(self.txn)) else { return };
        let id = self.next;
        self.next += 1;
        self.waiting = Some(id);
        let push = Push::Abort { lsid: self.lsid, txn_number: self.txn_number };
        self.out.push(Request { id, g, op: TxnOp::PushTxn { txn: self.txn, push } });
    }

    /// The requests to send.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.out)
    }

    /// The answer to request `id`; a refusal (another leader, a range that moved, a timeout) is
    /// sent again at the next tick.
    pub fn on_reply(&mut self, id: ReqNo, r: Result<Reply, RangeError>) {
        if self.waiting != Some(id) {
            return;
        }
        self.waiting = None;
        if let Ok(Reply::Record(rec)) = r {
            self.decision = Some(Self::decide(&rec));
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
}
