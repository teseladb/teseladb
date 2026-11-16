//! [`ExpectedException`], the error tests throw on purpose and then expect to see.

use std::fmt;

/// A runtime error whose message is `expected`.
#[derive(Debug, Clone, Default)]
pub struct ExpectedException;

impl ExpectedException {
    /// The error, with message `expected`.
    pub fn new() -> Self {
        ExpectedException
    }
}

impl fmt::Display for ExpectedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected")
    }
}

impl std::error::Error for ExpectedException {}
