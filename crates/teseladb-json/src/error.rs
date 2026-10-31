//! Parse errors with their position.

use std::fmt;

/// What failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// End of text before a value finished.
    Eof,
    /// Unexpected character.
    Unexpected,
    /// Malformed number.
    Number,
    /// Invalid escape or lone UTF-16 surrogate.
    Escape,
    /// Unescaped control character inside a string.
    Control,
    /// The text is not valid UTF-8.
    Utf8,
    /// Repeated key in an object.
    DuplicateKey,
    /// Nesting deeper than the limit.
    Depth,
    /// Text larger than the limit.
    Size,
    /// Data after the value.
    Trailing,
}

/// Error with its position (byte offset, line and column from 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// What failed.
    pub kind: ErrorKind,
    /// Byte offset.
    pub offset: usize,
    /// Line (from 1).
    pub line: usize,
    /// Column in bytes (from 1).
    pub column: usize,
}

impl Error {
    pub(crate) fn at(kind: ErrorKind, input: &[u8], offset: usize) -> Error {
        let offset = offset.min(input.len());
        let before = &input[..offset];
        let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
        let column = 1 + before.iter().rev().take_while(|&&b| b != b'\n').count();
        Error { kind, offset, line, column }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Eof => "unexpected end of input",
            ErrorKind::Unexpected => "unexpected character",
            ErrorKind::Number => "malformed number",
            ErrorKind::Escape => "invalid escape",
            ErrorKind::Control => "unescaped control character",
            ErrorKind::Utf8 => "invalid UTF-8",
            ErrorKind::DuplicateKey => "duplicate key",
            ErrorKind::Depth => "nesting too deep",
            ErrorKind::Size => "text too large",
            ErrorKind::Trailing => "data after the value",
        };
        write!(f, "{what} at line {}, column {} (byte {})", self.line, self.column, self.offset)
    }
}

impl std::error::Error for Error {}
