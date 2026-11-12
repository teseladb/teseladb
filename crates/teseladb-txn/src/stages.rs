//! The stage of 012 FR-015 that each request of a coordinator counts in.

use teseladb_range::host::TxnOp;
use teseladb_range::intents::RecordOp;
use teseladb_stages::Stage;

/// The stage a request `op` of a coordinator is timed as; `None` for the requests of readers and
/// of the record's upkeep, which are not part of a transaction's own path.
pub fn stage_of(op: &TxnOp) -> Option<Stage> {
    match op {
        TxnOp::PutIntents { .. } => Some(Stage::TxnIntents),
        TxnOp::LockKeys { .. } | TxnOp::UnlockKeys { .. } => Some(Stage::TxnLock),
        TxnOp::CheckIntents { .. } => Some(Stage::TxnCheck),
        TxnOp::OnePhase { .. } => Some(Stage::TxnOnePhase),
        TxnOp::ResolveIntents { .. } => Some(Stage::TxnResolve),
        TxnOp::EndTxn { op: RecordOp::Begin { .. }, .. } => Some(Stage::TxnBegin),
        TxnOp::EndTxn { op: RecordOp::Commit { .. } | RecordOp::Abort { .. }, .. } => {
            Some(Stage::TxnEnd)
        }
        TxnOp::EndTxn { op: RecordOp::Heartbeat { .. } | RecordOp::Resolved, .. }
        | TxnOp::PushTxn { .. }
        | TxnOp::FindIntents { .. }
        | TxnOp::PushByRead { .. }
        | TxnOp::ReadRecord { .. }
        | TxnOp::ForgetRecord { .. } => None,
    }
}

/// Sends `op` with `send` and counts how long it took in its stage, measured with `micros`.
pub fn timed<R>(op: TxnOp, micros: impl Fn() -> u64, send: impl FnOnce(TxnOp) -> R) -> R {
    let stage = stage_of(&op);
    let start = micros();
    let out = send(op);
    if let Some(s) = stage {
        teseladb_stages::record(s, micros().saturating_sub(start).saturating_mul(1_000));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use teseladb_keys::Hlc;
    use teseladb_range::intents::TxnId;

    #[test]
    fn each_request_of_the_path_has_its_stage() {
        let txn = TxnId(7);
        let begin = TxnOp::EndTxn {
            txn,
            op: RecordOp::Begin {
                lsid: 1,
                txn_number: 1,
                read_ts: Hlc { physical: 1, logical: 0 },
            },
        };
        assert_eq!(stage_of(&begin), Some(Stage::TxnBegin));
        let commit = TxnOp::EndTxn {
            txn,
            op: RecordOp::Commit { ts: Hlc { physical: 1, logical: 0 }, spans: Vec::new() },
        };
        assert_eq!(stage_of(&commit), Some(Stage::TxnEnd));
        let beat = TxnOp::EndTxn { txn, op: RecordOp::Heartbeat { spans: Vec::new() } };
        assert_eq!(stage_of(&beat), None);
        let check = TxnOp::CheckIntents { txn, keys: Vec::new() };
        assert_eq!(stage_of(&check), Some(Stage::TxnCheck));
    }

    #[test]
    fn a_timed_request_counts_once_in_its_stage() {
        let before = teseladb_stages::count(Stage::TxnCheck);
        let clock = std::cell::Cell::new(100u64);
        let out = timed(
            TxnOp::CheckIntents { txn: TxnId(1), keys: Vec::new() },
            || {
                let t = clock.get();
                clock.set(t + 50);
                t
            },
            |_| 3,
        );
        assert_eq!(out, 3);
        assert_eq!(teseladb_stages::count(Stage::TxnCheck), before + 1);
    }
}
