//! The error reported when a wait or an operation passes its deadline.
//!
//! [`TimedOutError`] is what [`with_timeout`](crate::engine::core::with_timeout), timed semaphore
//! waits, condition variables and abort-on-expiry deliver when their timer fires first.
//! [`DefaultTimeoutExceptionFactory`] is the default policy used by those primitives to build it.

/// The operation did not finish before its deadline.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimedOutError;

impl TimedOutError {
    /// Returns a timeout error.
    #[inline]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the error message, `"timedout"`.
    #[inline]
    pub const fn what(&self) -> &'static str {
        "timedout"
    }
}

/// Formats the error as its [`what`](TimedOutError::what) message.
impl std::fmt::Display for TimedOutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for TimedOutError {}

/// Default factory for the error of a timed-out wait.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultTimeoutExceptionFactory;

impl DefaultTimeoutExceptionFactory {
    /// Returns the error delivered to a waiter whose deadline passed: a [`TimedOutError`].
    #[inline]
    pub fn timeout() -> TimedOutError {
        TimedOutError::new()
    }
}
