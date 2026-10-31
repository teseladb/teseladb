//! The answer to `commitTransaction` (008 FR-009, contract `txn-errors.md`): a commit whose result
//! is lost carries `UnknownTransactionCommitResult` so that the driver asks again, and a commit
//! asked again gets the result the first one had, from the record.

use teseladb_hlc::Hlc;
use teseladb_range::host::RangeError;

use crate::coordinator::{Outcome, TxnError};

/// What a commit answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitAnswer {
    /// Committed at this mark (`operationTime`).
    Committed(Hlc),
    /// The transaction no longer exists: `NoSuchTransaction` with `TransientTransactionError`.
    NoSuchTransaction,
    /// A write conflict ended it: `WriteConflict` with `TransientTransactionError`.
    WriteConflict,
    /// The result is unknown: the range error with `UnknownTransactionCommitResult`; the
    /// deadline of the command (`maxTimeMS`) gives `MaxTimeMSExpired` (50).
    Unknown(RangeError),
}

/// The answer for the outcome the coordinator gave.
pub fn commit_answer(outcome: &Outcome) -> CommitAnswer {
    match outcome {
        Outcome::Committed(ts) => CommitAnswer::Committed(*ts),
        // Too large, it was aborted: as RefDb, its commit finds no transaction.
        Outcome::Aborted | Outcome::Failed(TxnError::NoSuchTransaction | TxnError::TooLarge) => {
            CommitAnswer::NoSuchTransaction
        }
        Outcome::Failed(TxnError::WriteConflict) => CommitAnswer::WriteConflict,
        Outcome::Failed(TxnError::NoRange) => CommitAnswer::Unknown(RangeError::OutOfRange),
        Outcome::Failed(TxnError::Range(e)) | Outcome::Unknown(e) => {
            CommitAnswer::Unknown(e.clone())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_outcome_has_its_answer() {
        let ts = Hlc { physical: 3, logical: 1 };
        let cases = [
            (Outcome::Committed(ts), CommitAnswer::Committed(ts)),
            (Outcome::Aborted, CommitAnswer::NoSuchTransaction),
            (Outcome::Failed(TxnError::NoSuchTransaction), CommitAnswer::NoSuchTransaction),
            (Outcome::Failed(TxnError::WriteConflict), CommitAnswer::WriteConflict),
            (Outcome::Failed(TxnError::NoRange), CommitAnswer::Unknown(RangeError::OutOfRange)),
            (
                Outcome::Failed(TxnError::Range(RangeError::Timeout)),
                CommitAnswer::Unknown(RangeError::Timeout),
            ),
            (
                Outcome::Unknown(RangeError::Unavailable),
                CommitAnswer::Unknown(RangeError::Unavailable),
            ),
        ];
        for (o, a) in cases {
            assert_eq!(commit_answer(&o), a, "{o:?}");
        }
    }
}
