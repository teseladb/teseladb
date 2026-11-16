//! Assertions that stay active in release builds.
//!
//! [`engine_assert!`](crate::engine_assert) checks a condition in every build profile
//! (unlike `debug_assert!`). On failure it prints the expression and its location to
//! standard output and aborts the process through [`internal::assert_fail`].

/// Support functions for the assertion macros; not part of the stable API.
pub mod internal {
    /// Reports a failed assertion and aborts; implemented by the logging module.
    pub use crate::engine::util::log::assert_fail;
}

/// Asserts that `$x` is true in all build profiles.
///
/// # Panics
///
/// Does not unwind: when `$x` is false it prints the stringified expression, file, line
/// and module, then aborts the process.
#[macro_export]
macro_rules! engine_assert {
    ($x:expr) => {
        if !($x) {
            $crate::engine::util::assert::internal::assert_fail(
                stringify!($x),
                file!(),
                line!() as i32,
                module_path!(),
            );
        }
    };
}
