//! TeselaDB's RefDb wire protocol, without I/O (specification 006): the server hands each
//! connection the bytes it reads and writes back the bytes it gets.
//!
//! - [`msg`], [`query`] and [`compressed`]: `OP_MSG`, the `OP_QUERY` handshake with its
//!   `OP_REPLY`, and `OP_COMPRESSED` with snappy, zlib and zstd (FR-001, FR-002).
//! - [`connection`]: one client connection, from bytes to [`connection::Request`] and from a
//!   reply document to bytes.
//! - [`dispatch`] and [`hello`]: the commands a node answers by itself, the handshake as a
//!   `refrouter` among them (FR-003, FR-004).
//! - [`error`]: RefDb's error codes and the translation of the range's errors (FR-021).
//! - [`api_version`]: the parameters of the Stable API and what `apiStrict` refuses (FR-035).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

use std::fmt;

pub mod api_version;
mod bson_len;
pub mod cluster_time;
pub mod commands;
pub mod compressed;
pub mod concerns;
pub mod connection;
pub mod dispatch;
pub mod error;
pub mod failpoint;
pub mod generic;
pub mod hello;
pub mod ingress;
pub mod internal_fields;
pub mod msg;
pub mod ops;
pub mod query;
pub mod redact;
pub mod types;

/// Largest message, header included (`maxMessageSizeBytes`).
pub const MAX_MESSAGE_SIZE: usize = 48_000_000;
/// Largest stored document (`maxBsonObjectSize`).
pub const MAX_BSON_OBJECT_SIZE: i32 = 16 * 1024 * 1024;
/// Most writes in one batch (`maxWriteBatchSize`).
pub const MAX_WRITE_BATCH_SIZE: i32 = 100_000;
/// Newest wire version: RefDb 9.0's (D-024).
pub const MAX_WIRE_VERSION: i32 = 29;
/// Oldest wire version a client may speak.
pub const MIN_WIRE_VERSION: i32 = 0;
/// Minutes a logical session lives without use (`logicalSessionTimeoutMinutes`).
pub const LOGICAL_SESSION_TIMEOUT_MINUTES: i32 = 30;

/// A fault that closes the connection, as RefDb closes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// `messageLength` below the header or above [`MAX_MESSAGE_SIZE`].
    MessageLength(i32),
    /// `messageLength` above [`connection::UNAUTHENTICATED_MAX_MESSAGE`] on a connection that
    /// has not authenticated yet, with authentication on (013).
    UnauthenticatedLength(i32),
    /// An opcode the server does not take (the legacy ones included).
    UnsupportedOpcode(i32),
    /// Required `OP_MSG` flag bits the server does not know.
    IllegalFlags(u32),
    /// The `OP_MSG` checksum does not match.
    Checksum,
    /// A compressor id that is not enabled (`noop` included).
    UnknownCompressor(u8),
    /// Compressed data that does not decode.
    Compression(teseladb_compress::Error),
    /// A document that is not valid BSON.
    Bson(teseladb_bson::BsonError),
    /// Lengths or strings that do not add up.
    Malformed(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::MessageLength(n) => {
                write!(f, "message length {n} is invalid. Min 16 Max: {MAX_MESSAGE_SIZE}")
            }
            WireError::UnauthenticatedLength(n) => write!(
                f,
                "message length {n} is too long for a connection that has not authenticated. Max: {}",
                connection::UNAUTHENTICATED_MAX_MESSAGE
            ),
            WireError::UnsupportedOpcode(op) => write!(f, "Message type {op} is not supported."),
            WireError::IllegalFlags(bits) => {
                write!(f, "Message contains illegal flags value: Ob{bits:032b}")
            }
            WireError::Checksum => f.write_str("OP_MSG checksum does not match contents"),
            WireError::UnknownCompressor(id) => write!(f, "unknown compressor id {id}"),
            WireError::Compression(e) => write!(f, "{e}"),
            WireError::Bson(e) => write!(f, "invalid BSON: {e}"),
            WireError::Malformed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for WireError {}
