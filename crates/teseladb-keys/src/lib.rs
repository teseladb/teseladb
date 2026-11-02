//! TeselaDB's keys: values encoded so that their bytes keep RefDb's order (FR-008 to FR-010),
//! with the types the key does not keep in [`TypeBits`] (FR-009), and [`decode`] back to the
//! exact values; the key spaces, buckets and version suffixes of every key (FR-011 to FR-013); the key intervals of a filter's bounds
//! (FR-014).
//!
//! ```
//! use teseladb_bson::{Document, RawDoc};
//! use teseladb_keys::{Direction, encode};
//!
//! let a = Document::new().with("v", 2).to_bytes().unwrap();
//! let b = Document::new().with("v", 10.5).to_bytes().unwrap();
//! let (a, b) = (RawDoc::from_bytes(&a).unwrap(), RawDoc::from_bytes(&b).unwrap());
//! let (ka, _) = encode(&[(a.get("v").unwrap(), Direction::Ascending)]);
//! let (kb, _) = encode(&[(b.get("v").unwrap(), Direction::Ascending)]);
//! assert!(ka < kb);
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod bounds;
mod bucket;
mod decode;
mod encode;
pub mod space;
mod typebits;
mod version;

pub use bounds::{FieldBounds, Interval, KeyRange, bounds, next_prefix};
pub use bucket::{DOCUMENT_BUCKETS, bucket};
pub use decode::{DecodeError, decode, decode_untyped, values_len};
pub use encode::{Direction, class_byte, encode, encode_value, encode_value_with};
pub use space::{DOCUMENTS, FieldSpec, IndexKeySpec, Key, KeyBuilder, KeyParts, PrimaryKey};
pub use typebits::TypeBits;
pub use version::{Hlc, KeyError, VERSION_LEN, VersionedKey};
