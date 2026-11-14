//! BSON 1.1 (bsonspec.org), with RefDb's limits.
//!
//! - [`RawDoc`]: zero-copy view over a validated document, with access by dotted path (FR-004).
//!   Reading validates lengths, terminators, types, booleans, UTF-8, the inner length of the old
//!   binary and bytes left over, and fails with the offset (FR-002); a stored document has at most
//!   [`MAX_SIZE`] bytes and [`MAX_DEPTH`] levels (FR-003). [`validate_message`] checks a message
//!   the way RefDb does, without the UTF-8 and old-binary rules the BSON specification adds and
//!   with the layout of BSON columns.
//! - [`column`](mod@column): BSON columns (binary subtype 7), their check and their values.
//! - [`Document`] and [`Value`]: owned values to build documents, written in canonical BSON with
//!   [`Document::to_bytes`] (FR-001).
//! - [`compare`]: RefDb's comparison order, numbers by exact value across their four types and
//!   strings by bytes or by a [`Collator`] (FR-005 to FR-007).
//! - [`extjson`]: Extended JSON v2, canonical and relaxed (FR-015).
//! - [`find_nul`]: the 0 byte that ends a name, found with the vector instructions of the CPU
//!   (NEON, AVX2 or SSE2); the only `unsafe` code of the crate lives in its module.
//!
//! ```
//! use teseladb_bson::{Document, RawDoc, RawValue};
//!
//! let bytes = Document::new().with("a", Document::new().with("b", 7)).to_bytes().unwrap();
//! let raw = RawDoc::from_bytes(&bytes).unwrap();
//! assert_eq!(raw.get("a.b"), Some(RawValue::Int32(7)));
//! assert_eq!(Document::from(raw).to_bytes().unwrap(), bytes);
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod arch;
mod bignum;
mod builder;
pub mod column;
mod compare;
mod decimal128;
mod error;
pub mod extjson;
mod number;
mod raw;
mod read;
pub mod shell;
pub mod types;
mod value;
mod write;

pub use arch::{find_nul, nul_search};
pub use compare::{Collator, compare, compare_docs, compare_for_sort, type_class};
pub use decimal128::{
    Decimal, Decimal128, MAX_DIGITS, MAX_EXPONENT, MIN_EXPONENT, ParseDecimalError,
};
pub use error::BsonError;
pub use number::{NumberParts, number_parts};
pub use raw::{RawArray, RawDoc, RawIter, RawValue};
pub use read::{Limits, first_element, read_message, read_message_in, validate_message};
pub use value::{Document, Value};
pub use write::Fragments;

/// Maximum size of a document: 16 MiB.
pub const MAX_SIZE: usize = 16 * 1024 * 1024;

/// Maximum nested levels of a stored document, the top document being the first: 180, as a
/// `refrouter` 9.0.4 stores (measured, 003 T009; RefDb's `maxBSONDepth` of 200 minus the 20 levels
/// it keeps for itself). Documents and arrays count alike.
pub const MAX_DEPTH: usize = 180;

/// Maximum nested levels of a command document, the command being the first: a `refrouter` 9.0.4
/// answers `Overflow` for an object nested deeper than 200 levels below the command (measured,
/// 003 T009), and checks each document of an `OP_MSG` sequence the same way on its own.
pub const MAX_COMMAND_DEPTH: usize = 201;
