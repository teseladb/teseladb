//! Scoped control of the abort-on-internal-error setting.
//!
//! Internal errors normally abort the process when `abort_on_internal_error` is enabled.
//! Tests that exercise those error paths hold a
//! [`testing::ScopedNoAbortOnInternalError`] so the error is raised instead, while the
//! setting stays as configured for the rest of the program.

/// The scope guard.
pub mod testing {
    use crate::engine::core::on_internal_error::set_abort_on_internal_error;

    /// Disables aborting on internal errors while it lives.
    ///
    /// Intended for tests that check error paths reaching `on_internal_error` without
    /// aborting, while other, indirectly affected code paths keep the configured
    /// behaviour once the guard is dropped. The previous setting is restored on drop.
    pub struct ScopedNoAbortOnInternalError {
        _prev: bool,
    }

    impl ScopedNoAbortOnInternalError {
        /// Disables the abort, remembering the previous setting.
        #[inline]
        pub fn new() -> Self {
            ScopedNoAbortOnInternalError { _prev: set_abort_on_internal_error(false) }
        }
    }

    impl Default for ScopedNoAbortOnInternalError {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for ScopedNoAbortOnInternalError {
        fn drop(&mut self) {
            set_abort_on_internal_error(self._prev);
        }
    }
}
