//! TeselaDB's own DEFLATE (RFC 1951), zlib (RFC 1950) and gzip (RFC 1952).
//!
//! Everything streams: [`Inflater`] and [`Deflater`] take input in pieces of any size, and when a
//! stream ends the reader reports exactly how many bytes belonged to it (in a Git pack, the next
//! entry starts right there).
//!
//! ```
//! use teseladb_deflate::zlib;
//!
//! let data = b"hello hello hello hello, world".repeat(10);
//! let z = zlib::compress(&data, 6);
//! assert!(z.len() < data.len());
//! let (back, used) = zlib::decompress(&[&z[..], b"next"].concat()).unwrap();
//! assert_eq!((back, used), (data.to_vec(), z.len()));
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod deflate;
pub mod gzip;
mod huffman;
mod inflate;
pub mod zlib;

pub use deflate::{Deflater, deflate};
pub use inflate::{InflateError, Inflater, Status, WINDOW, inflate};
