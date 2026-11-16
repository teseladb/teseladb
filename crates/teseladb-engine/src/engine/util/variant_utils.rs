//! Visiting sum types.
//!
//! Alternatives are modelled as Rust `enum`s and visited with `match`.
//! [`engine_visit!`](crate::engine_visit) is a thin macro over that `match`, kept so that
//! visitor-style call sites read uniformly. Widening one enum into another with more
//! variants is done with a `From` impl written next to the enums.

/// Matches `$variant` against the given arms and returns the value of the arm taken.
///
/// The arms must cover every variant (the `match` is exhaustive) and all bodies must have
/// the same type, which is the type of the whole expression.
///
/// Usage: `engine_visit!(variant, Enum::A(x) => ..., Enum::B(y) => ...)`.
#[macro_export]
macro_rules! engine_visit {
    ($variant:expr, $($pat:pat => $body:expr),+ $(,)?) => {
        match $variant {
            $($pat => $body),+
        }
    };
}
