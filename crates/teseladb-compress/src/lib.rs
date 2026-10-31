//! TeselaDB's own compressors for the wire protocol's `OP_COMPRESSED` (006 FR-002, D-025):
//! [`snappy`] (raw format, compressor id 1) and [`zstd`] (RFC 8878, compressor id 3). zlib
//! (id 2) is `teseladb-deflate`'s.
//!
//! Both decoders take the most bytes they may produce and never allocate more, so a hostile
//! message cannot make the server reserve memory beyond what `maxMessageSizeBytes` allows.
//!
//! ```
//! use teseladb_compress::{snappy, zstd};
//!
//! let data = b"hello hello hello hello, world".repeat(10);
//! assert_eq!(snappy::decompress(&snappy::compress(&data), 1 << 20).unwrap(), data);
//! assert_eq!(zstd::decompress(&zstd::compress(&data), 1 << 20).unwrap(), data);
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

use std::fmt;

pub mod snappy;
pub mod zstd;

/// Why a compressed stream could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input ends in the middle of a stream.
    Truncated,
    /// The stream breaks a rule of its format; the text names it.
    Corrupt(&'static str),
    /// The output would exceed the limit the caller gave.
    TooLarge,
    /// The content checksum does not match.
    Checksum,
    /// A valid feature this decoder does not take (a zstd dictionary).
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("truncated compressed data"),
            Error::Corrupt(why) => write!(f, "corrupt compressed data: {why}"),
            Error::TooLarge => f.write_str("decompressed data exceeds the limit"),
            Error::Checksum => f.write_str("checksum mismatch in compressed data"),
            Error::Unsupported(what) => write!(f, "unsupported compressed data: {what}"),
        }
    }
}

impl std::error::Error for Error {}
