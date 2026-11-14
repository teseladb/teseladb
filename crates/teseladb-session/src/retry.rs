//! Retryable writes (FR-018): a write with `lsid` and `txnNumber` outside a transaction is
//! retryable; each of its statements has a key (`lsid`, `txnNumber`, `stmtId`) that the range
//! keeps with the result (005 FR-011), so a retry gets that result instead of writing again.
//! A `txnNumber` below the session's highest is too old, as in the reference server.

use crate::session::Session;

/// The key of one statement of a retryable write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatementKey {
    /// Session.
    pub lsid: u128,
    /// Transaction number.
    pub txn_number: i64,
    /// Statement within it.
    pub stmt_id: u32,
}

/// What a `txnNumber` is for its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnCheck {
    /// Higher than every one seen: a new write.
    New,
    /// The highest seen: a retry, or more statements of the same write.
    Same,
    /// Below the highest seen (`TransactionTooOld`, 225).
    TooOld {
        /// The session's highest.
        highest: i64,
    },
}

/// Checks `txn_number` against `session` and records it when it is new.
pub fn check_txn_number(session: &mut Session, txn_number: i64) -> TxnCheck {
    match session.txn_number {
        Some(h) if txn_number < h => TxnCheck::TooOld { highest: h },
        Some(h) if txn_number == h => TxnCheck::Same,
        _ => {
            session.txn_number = Some(txn_number);
            TxnCheck::New
        }
    }
}

/// The statement ids of a write of `n` statements that starts at `first` (`stmtId` 0 when the
/// driver gives none, as the reference server numbers them in order).
pub fn statement_keys(lsid: u128, txn_number: i64, first: u32, n: usize) -> Vec<StatementKey> {
    (0..n)
        .map(|i| StatementKey { lsid, txn_number, stmt_id: first.wrapping_add(i as u32) })
        .collect()
}
