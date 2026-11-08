//! What a reader does when a read stops at an intent (008 FR-003, FR-005): it pushes the intent's
//! transaction above its mark by reading its record there (008 T043), learns its state from the
//! answer, reads again knowing it, and resolves the intents of a finished transaction it met (lazy
//! resolution).

use teseladb_hlc::Hlc;
use teseladb_range::host::TxnOp;
use teseladb_range::intents::{IntentOwner, TxnRecord, TxnStatus};
use teseladb_range::push::Known;

/// The push a read at `read_ts` sends for the transaction whose intent it met: a read of its
/// record at that mark, which the transaction then commits above (008 T043).
pub fn push_for(owner: &IntentOwner, read_ts: Hlc) -> TxnOp {
    TxnOp::PushByRead { txn: owner.txn, at: read_ts }
}

/// What the record a push left tells the reader.
pub fn learn(rec: &TxnRecord) -> Known {
    // Faulty variant (008 T029): the reader takes every answer for a push that went through and
    // reads below the intent, missing a commit at or below its mark.
    if cfg!(teseladb_bug = "reader_ignores_push_result") {
        return Known::Pushed;
    }
    match rec.status {
        TxnStatus::Pending => Known::Pushed,
        TxnStatus::Committed => Known::Committed(rec.commit_ts),
        TxnStatus::Aborted => Known::Aborted,
    }
}

/// The resolution of a finished transaction's intents on `keys`, which the reader sends without
/// waiting for it; nothing for a pending one.
pub fn lazy_resolve(rec: &TxnRecord, keys: Vec<Vec<u8>>) -> Option<TxnOp> {
    rec.status.is_final().then_some(TxnOp::ResolveIntents {
        txn: rec.txn,
        status: rec.status,
        commit_ts: rec.commit_ts,
        keys,
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_range::intents::{TxnId, record_key};

    fn record(status: TxnStatus) -> TxnRecord {
        let ts = Hlc { physical: 9, logical: 0 };
        TxnRecord {
            txn: TxnId(4),
            lsid: 0,
            txn_number: 0,
            status,
            read_ts: ts,
            min_commit_ts: ts,
            commit_ts: ts,
            heartbeat_ts: ts,
            spans: vec![],
        }
    }

    #[test]
    fn a_reader_pushes_learns_and_resolves() {
        let owner = IntentOwner {
            txn: TxnId(4),
            record_key: record_key(TxnId(4)),
            key: vec![1, 4],
            ts: Hlc { physical: 1, logical: 0 },
        };
        let at = Hlc { physical: 5, logical: 2 };
        assert_eq!(
            push_for(&owner, at),
            TxnOp::PushByRead { txn: TxnId(4), at: Hlc { physical: 5, logical: 2 } }
        );
        assert_eq!(learn(&record(TxnStatus::Pending)), Known::Pushed);
        assert_eq!(learn(&record(TxnStatus::Aborted)), Known::Aborted);
        let c = record(TxnStatus::Committed);
        assert_eq!(learn(&c), Known::Committed(c.commit_ts));
        assert_eq!(lazy_resolve(&record(TxnStatus::Pending), vec![vec![1]]), None);
        assert_eq!(
            lazy_resolve(&c, vec![vec![1]]),
            Some(TxnOp::ResolveIntents {
                txn: TxnId(4),
                status: TxnStatus::Committed,
                commit_ts: c.commit_ts,
                keys: vec![vec![1]]
            })
        );
    }
}
