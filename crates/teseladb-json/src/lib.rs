//! JSON conforming to RFC 8259, with no dependencies.
//!
//! - [`Value`] with insertion-ordered objects ([`Object`]) and lossless numbers ([`Number`]:
//!   signed integer, unsigned integer, `f64`, or the exact text of an integer wider than 64 bits).
//! - [`parse`] with configurable depth (128) and size (64 MiB) limits ([`Limits`]) and errors
//!   carrying offset, line and column ([`Error`]). Duplicate keys are rejected.
//! - [`to_string`] writes compact JSON that reads back identically.
//! - [`parse_yaml`] reads the YAML subset of reference server configuration files into a [`Value`].
//!
//! ```
//! use teseladb_json::{Value, parse, to_string};
//!
//! let v = parse(r#"{"a":[1,-2,3.5,"xé"],"b":null}"#.as_bytes()).unwrap();
//! assert_eq!(v.get("a").unwrap().as_array().unwrap()[1].as_i64(), Some(-2));
//! assert_eq!(to_string(&v).unwrap(), r#"{"a":[1,-2,3.5,"xé"],"b":null}"#);
//! assert!(parse(b"[1,]").is_err());
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod error;
pub mod go;
mod parse;
mod value;
mod write;
mod yaml;

pub use error::{Error, ErrorKind};
pub use parse::{Limits, parse, parse_with};
pub use value::{Number, Object, Value};
pub use write::{WriteError, to_string, write_into};
pub use yaml::{MAX_YAML_DEPTH, MAX_YAML_SIZE, YamlError, YamlErrorKind, parse_yaml};
