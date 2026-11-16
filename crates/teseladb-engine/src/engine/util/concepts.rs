//! Conditional-constraint macros kept for backward compatibility.
//!
//! Generic constraints are always expressed with trait bounds, so these macros are
//! trivial: [`engine_concept!`](crate::engine_concept) keeps its input and
//! [`engine_no_concept!`](crate::engine_no_concept) drops it.

/// Expands to its input unchanged (a constraint that is always enabled).
#[macro_export]
macro_rules! engine_concept {
    ($($x:tt)*) => { $($x)* };
}

/// Expands to nothing (the fallback for code written for builds without constraints).
#[macro_export]
macro_rules! engine_no_concept {
    ($($x:tt)*) => {};
}
