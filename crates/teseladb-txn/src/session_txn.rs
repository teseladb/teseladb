//! A session's transactions as the reference server 9.0 numbers them (008 FR-009): `txnNumber`,
//! `startTransaction` with `autocommit: false`, the statements that continue a transaction, and
//! `commitTransaction` and `abortTransaction`, with the errors 9.0.4 gives for each misuse
//! (measured through the reference router).

/// What a command of the session asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// A statement with `startTransaction: true`.
    Start,
    /// A statement of a running transaction.
    Continue,
    /// `commitTransaction`.
    Commit,
    /// `abortTransaction`.
    Abort,
}

/// What the session does with the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A new transaction begins with this statement.
    Begin,
    /// The statement runs in the transaction.
    Statement,
    /// The transaction commits.
    Commit,
    /// The commit of a transaction that already committed is asked again: the same answer.
    RetryCommit,
    /// The transaction aborts.
    Abort,
}

/// Why the session refuses the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// `autocommit: true`, or `startTransaction` without `autocommit` (72, `InvalidOptions`).
    InvalidOptions(&'static str),
    /// A `txnNumber` below the session's highest (225, `TransactionTooOld`).
    TooOld {
        /// The number asked.
        asked: i64,
        /// The highest one seen.
        last: i64,
    },
    /// `startTransaction` again with the running number (117, `ConflictingOperationInProgress`).
    AlreadyStarted(i64),
    /// No running transaction with that number (251, `NoSuchTransaction`); the reference server's
    /// message names the session's last transaction number, `last` (none before the first).
    NoSuchTransaction {
        /// The session's last transaction number.
        last: Option<i64>,
    },
    /// `commitTransaction` of a number the session has no record of: a reference router 9.0.4
    /// recovers its decision with the `recoveryToken` (measured). The number becomes the session's
    /// last, and `last` is the one before it.
    Unrecorded {
        /// The number asked.
        asked: i64,
        /// The session's last transaction number before it.
        last: Option<i64>,
    },
    /// `abortTransaction` of a committed transaction (256, `TransactionCommitted`).
    Committed(i64),
}

/// `errmsg` of `autocommit: true`.
pub const AUTOCOMMIT_TRUE: &str = "Specifying autocommit=true is not allowed.";
/// `errmsg` of `startTransaction` without `autocommit`.
pub const START_WITHOUT_AUTOCOMMIT: &str =
    "'startTransaction' field requires 'autocommit' field to also be specified";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running(i64),
    Committed(i64),
    Aborted(i64),
}

/// The transactions of one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionTxn {
    highest: Option<i64>,
    state: Option<State>,
}

impl SessionTxn {
    /// Checks a command with `txn_number` and `autocommit` that asks `ask`.
    pub fn check(
        &mut self,
        txn_number: i64,
        autocommit: Option<bool>,
        ask: Ask,
    ) -> Result<Verdict, SessionError> {
        if autocommit == Some(true) {
            return Err(SessionError::InvalidOptions(AUTOCOMMIT_TRUE));
        }
        if ask == Ask::Start && autocommit.is_none() {
            return Err(SessionError::InvalidOptions(START_WITHOUT_AUTOCOMMIT));
        }
        if let Some(last) = self.highest.filter(|&h| txn_number < h) {
            return Err(SessionError::TooOld { asked: txn_number, last });
        }
        let current = self.state.filter(|s| match s {
            State::Running(n) | State::Committed(n) | State::Aborted(n) => *n == txn_number,
        });
        let last = self.highest;
        match (ask, current) {
            (Ask::Start, Some(_)) => Err(SessionError::AlreadyStarted(txn_number)),
            (Ask::Start, None) => {
                self.highest = Some(txn_number);
                self.state = Some(State::Running(txn_number));
                Ok(Verdict::Begin)
            }
            (Ask::Continue, Some(State::Running(_))) => Ok(Verdict::Statement),
            (Ask::Commit, Some(State::Running(_))) => Ok(Verdict::Commit),
            (Ask::Commit, Some(State::Committed(_))) => Ok(Verdict::RetryCommit),
            (Ask::Abort, Some(State::Running(_))) => Ok(Verdict::Abort),
            (Ask::Abort, Some(State::Committed(_))) => Err(SessionError::Committed(txn_number)),
            (Ask::Commit, None) => {
                self.highest = Some(txn_number);
                Err(SessionError::Unrecorded { asked: txn_number, last })
            }
            (Ask::Continue | Ask::Commit | Ask::Abort, _) => {
                Err(SessionError::NoSuchTransaction { last })
            }
        }
    }

    /// The session's last transaction number, if it ever had one.
    pub fn last(&self) -> Option<i64> {
        self.highest
    }

    /// The running transaction committed.
    pub fn committed(&mut self, txn_number: i64) {
        self.state = Some(State::Committed(txn_number));
    }

    /// The running transaction aborted (by the client, an error or its time limit).
    pub fn aborted(&mut self, txn_number: i64) {
        self.state = Some(State::Aborted(txn_number));
    }

    /// Whether the session's last transaction is `txn_number` and it aborted.
    pub fn is_aborted(&self, txn_number: i64) -> bool {
        self.state == Some(State::Aborted(txn_number))
    }

    /// The number of the running transaction, if one runs.
    pub fn running(&self) -> Option<i64> {
        match self.state {
            Some(State::Running(n)) => Some(n),
            Some(State::Committed(_) | State::Aborted(_)) | None => None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_the_reference() {
        let mut s = SessionTxn::default();
        assert_eq!(
            s.check(5, Some(true), Ask::Start),
            Err(SessionError::InvalidOptions(AUTOCOMMIT_TRUE))
        );
        assert_eq!(
            s.check(5, None, Ask::Start),
            Err(SessionError::InvalidOptions(START_WITHOUT_AUTOCOMMIT))
        );
        assert_eq!(
            s.check(5, Some(false), Ask::Continue),
            Err(SessionError::NoSuchTransaction { last: None })
        );
        assert_eq!(s.check(5, Some(false), Ask::Start), Ok(Verdict::Begin));
        assert_eq!(s.running(), Some(5));
        assert_eq!(s.check(5, Some(false), Ask::Start), Err(SessionError::AlreadyStarted(5)));
        assert_eq!(s.check(5, Some(false), Ask::Continue), Ok(Verdict::Statement));
        assert_eq!(
            s.check(4, Some(false), Ask::Continue),
            Err(SessionError::TooOld { asked: 4, last: 5 })
        );
        assert_eq!(
            s.check(6, Some(false), Ask::Continue),
            Err(SessionError::NoSuchTransaction { last: Some(5) })
        );
        assert_eq!(s.check(5, Some(false), Ask::Commit), Ok(Verdict::Commit));
        s.committed(5);
        assert_eq!(s.running(), None);
        assert_eq!(s.check(5, Some(false), Ask::Commit), Ok(Verdict::RetryCommit));
        assert_eq!(s.check(5, Some(false), Ask::Abort), Err(SessionError::Committed(5)));
        assert_eq!(
            s.check(5, Some(false), Ask::Continue),
            Err(SessionError::NoSuchTransaction { last: Some(5) })
        );
        assert_eq!(s.check(7, Some(false), Ask::Start), Ok(Verdict::Begin));
        assert_eq!(s.check(7, Some(false), Ask::Abort), Ok(Verdict::Abort));
        assert!(!s.is_aborted(7));
        s.aborted(7);
        assert!(s.is_aborted(7) && !s.is_aborted(5));
        let gone = Err(SessionError::NoSuchTransaction { last: Some(7) });
        assert_eq!(s.check(7, Some(false), Ask::Abort), gone);
        assert_eq!(s.check(7, Some(false), Ask::Commit), gone);
        assert_eq!(s.running(), None);
        // A commit of a number it never ran makes that number its last.
        assert_eq!(
            s.check(9, Some(false), Ask::Commit),
            Err(SessionError::Unrecorded { asked: 9, last: Some(7) })
        );
        assert_eq!(s.last(), Some(9));
        assert_eq!(
            s.check(8, Some(false), Ask::Start),
            Err(SessionError::TooOld { asked: 8, last: 9 })
        );
        let fresh = SessionTxn::default().check(3, Some(false), Ask::Commit);
        assert_eq!(fresh, Err(SessionError::Unrecorded { asked: 3, last: None }));
    }
}
