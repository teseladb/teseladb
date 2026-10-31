//! Checkers. Those of 002 (documents, transactions, indexes) are added with them.

pub mod changestream_delivery;
pub mod durability;
pub mod index_consistency;
pub mod linearizable;
pub mod liveness;
pub mod raft_safety;
pub mod snapshot_isolation;
pub mod storage;

/// A property that does not hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Checker.
    pub checker: &'static str,
    /// Readable detail.
    pub detail: String,
    /// It is a liveness one (not safety).
    pub liveness: bool,
}

impl Violation {
    /// Safety violation.
    pub fn safety(checker: &'static str, detail: impl Into<String>) -> Violation {
        Violation { checker, detail: detail.into(), liveness: false }
    }

    /// Liveness violation.
    pub fn liveness(checker: &'static str, detail: impl Into<String>) -> Violation {
        Violation { checker, detail: detail.into(), liveness: true }
    }
}
