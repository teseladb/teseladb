//! The server side of RefDb's Queryable Encryption and Client-Side Field Level Encryption
//! (017, decision D-033).
//!
//! Drivers encrypt and decrypt; the server never sees a key or a value in clear. It keeps the
//! encrypted documents with their `__safeContent__` tags, the metadata collections (the encrypted
//! state collection, ESC, and the compaction log, ECOC), turns the search payloads of a query into
//! tag lookups and compacts the metadata. The formats follow the public specification of the
//! drivers and librefcrypt's description of its payloads, measured against a `refnode` 9.0.4.
//!
//! Everything here is sans-I/O: reading the ESC and drawing random bytes come from the caller.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod cipher;
pub mod error;
pub mod idl;
pub mod iev;
pub mod payloads;
pub mod subtype;
pub mod tokens;

pub use error::FleError;
