//! Reporting of an error that has no caller left to handle it.
//!
//! [`report_exception`] writes a message and the error to the engine's logger at error level. It is
//! used where a failure is detected but cannot be propagated, for example in a fire-and-forget
//! continuation.

/// Logs `message` followed by the error held in `eptr`, at error level, on the engine logger.
///
/// The line has the form `"<message>: <error>"`. It never fails and never panics.
pub use crate::engine::core::reactor::report_exception;
