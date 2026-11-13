//! The two service classes of a core's requests (012 FR-030): document operations and
//! transactions go ahead of long aggregations and scans, which run only in the time the first
//! leave. A core runs a background request only when its mailbox has nothing new, one per turn,
//! so a foreground request that arrives meanwhile waits at most for one of them.

/// Which class a message to a core belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceClass {
    /// Document operations, transactions and everything that is not a long read.
    Foreground,
    /// Long aggregations and scans.
    Background,
}
