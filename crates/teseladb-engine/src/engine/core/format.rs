//! Formatting into an [`Sstring`], the engine's small-string-optimised string type.
//!
//! [`format`](fn@format) and the [`format_sstring!`](crate::format_sstring) macro take the standard Rust
//! format syntax and produce an [`Sstring`] directly, so short results stay inline without a heap
//! allocation.

use crate::engine::core::sstring::Sstring;

/// Evaluates pre-built format arguments into a new [`Sstring`].
///
/// `args` comes from [`std::format_args!`]; the result holds the formatted text. Writing into an
/// [`Sstring`] cannot fail, so the function has no error case.
#[inline]
pub fn format(args: std::fmt::Arguments<'_>) -> Sstring {
    let mut out = Sstring::new();
    // Writing into an sstring cannot fail.
    let _ = std::fmt::Write::write_fmt(&mut out, args);
    out
}

/// Formats its arguments with the standard Rust format syntax into an [`Sstring`]:
/// `format_sstring!("{} {}", a, b)`.
#[macro_export]
macro_rules! format_sstring {
    ($($arg:tt)*) => {
        $crate::engine::core::format::format(::std::format_args!($($arg)*))
    };
}
