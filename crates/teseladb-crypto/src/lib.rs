//! Cryptographic primitives of TeselaDB, written from their standards (013 FR-001).
//!
//! Every function is pure: no I/O, no clock and no randomness of its own. Secret-dependent code
//! avoids secret-dependent branches and memory indices; `tests/constant_time.rs` checks it. The
//! CPU's AES and carry-less multiplication instructions are used when present; `src/arch/` is the
//! only module with `unsafe`, each block with its `// SAFETY:`.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod aes;
mod arch;
pub mod at_rest;
mod bigint;
pub mod chacha20poly1305;
pub mod ct;
pub mod curve25519;
pub mod des;
pub mod digest;
pub mod ec;
mod field25519;
pub mod hkdf;
pub mod hmac;
pub mod kerberos;
pub mod modes;
pub mod pbkdf2;
pub mod rand;
pub mod rsa;
pub mod sha224;
pub mod sha512;

pub use arch::{accelerated, force_portable};
pub use digest::{Digest, HashAlgorithm, Output};
pub use teseladb_hash::{Sha1, Sha256, md5, sha1, sha256};
