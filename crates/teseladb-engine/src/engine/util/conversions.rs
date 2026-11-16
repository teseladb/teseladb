//! String conversions used by configuration parsing.
//!
//! [`parse_memory_size`] reads sizes such as `"4k"` or `"1TiB"` from command-line options;
//! [`string2vector`] produces a NUL-terminated byte vector for system calls. Parse failures
//! are reported as [`BadLexicalCast`].

use std::fmt;

use crate::engine::core::future::ExceptionPtr;

/// The error returned when a string cannot be read as the requested type.
///
/// Its message is `cannot parse value: the string is not a valid value of the target type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BadLexicalCast;

impl fmt::Display for BadLexicalCast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cannot parse value: the string is not a valid value of the target type")
    }
}

impl std::error::Error for BadLexicalCast {}

/// Converts a string to a memory size in bytes.
///
/// Suffixes are binary (powers of 1024) even without the `i`, to follow existing usage.
/// Accepted grammar:
///
/// ```text
/// memory_size ::= <digit>+ <suffix>? "i"? "B"?
/// suffix      ::= ("k" | "K" | "M" | "G" | "T")
/// ```
///
/// For instance `"5"` is 5, `"4k"` is `4 << 10`, `"8Mi"` is `8 << 20`, `"7GB"` is
/// `7 << 30` and `"1TiB"` is `1 << 40`.
///
/// # Errors
///
/// Returns [`BadLexicalCast`] (as an [`ExceptionPtr`]) when what remains after removing
/// the suffixes is not a whole `usize` number.
pub fn parse_memory_size(s: &str) -> Result<usize, ExceptionPtr> {
    let mut s = s;
    for unit in ["i", "iB", "B"] {
        if let Some(rest) = s.strip_suffix(unit) {
            s = rest;
            break;
        }
    }
    let mut factor: usize = 1;
    for &Suffix { suffix, power } in SUFFIXES.iter() {
        if let Some(rest) = s.strip_suffix(suffix) {
            factor <<= power;
            s = rest;
            break;
        }
    }
    // The whole remaining string must be a number.
    let v: usize = s.parse().map_err(|_| ExceptionPtr::new(BadLexicalCast))?;
    Ok(v * factor)
}

/// Returns the bytes of `str` followed by a terminating NUL byte.
#[inline]
pub fn string2vector(str: &str) -> Vec<u8> {
    let mut v: Vec<u8> = str.as_bytes().to_vec();
    v.push(b'\0');
    v
}

/// A size suffix and the power of two it multiplies by.
struct Suffix {
    suffix: &'static str,
    power: u32,
}

/// The suffixes accepted by [`parse_memory_size`], tried in order.
static SUFFIXES: [Suffix; 5] = [
    Suffix { suffix: "k", power: 10 },
    Suffix { suffix: "K", power: 10 },
    Suffix { suffix: "M", power: 20 },
    Suffix { suffix: "G", power: 30 },
    Suffix { suffix: "T", power: 40 },
];
