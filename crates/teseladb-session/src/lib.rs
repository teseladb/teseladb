//! TeselaDB's logical sessions, cursors and retryable writes (006 FR-016 to FR-018), without
//! I/O: the caller passes the cluster time and the random numbers.
//!
//! - [`session`]: sessions by `lsid` with their last use and transaction number, expiring after
//!   30 minutes without use.
//! - [`cursor`]: the cursors of a node, those outside any session expiring after 10 minutes
//!   without use unless `noCursorTimeout`, with their node in the identifier's high bits.
//! - [`retry`]: the statement key of a retryable write and the checks of its `txnNumber`.
//! - [`tailable`]: the tailable cursors of capped collections (015).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod cursor;
pub mod retry;
pub mod session;
pub mod tailable;
