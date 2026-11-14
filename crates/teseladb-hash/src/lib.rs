//! Hash functions and checksums.
//!
//! Every algorithm has a portable implementation and, when the CPU allows it, an accelerated one
//! using `std::arch`, chosen at run time. The `unsafe` code lives only in `src/arch/` and every
//! block documents its invariant with `// SAFETY:`.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod adler32;
mod arch;
mod crc32;
mod crc32c;
mod md5;
mod sha1;
mod sha256;

pub use adler32::{Adler32, adler32};
pub use crc32::{Crc32, crc32, crc32_append};
pub use crc32c::{Crc32c, crc32c, crc32c_append, crc32c_portable};
pub use md5::md5;
pub use sha1::{Sha1, sha1};
pub use sha256::{Sha256, sha256};

/// Portable implementations, exposed to compare against the accelerated ones in tests and benchmarks.
pub mod portable {
    pub use crate::adler32::adler32_portable as adler32;
    pub use crate::crc32c::crc32c_portable as crc32c;
    pub use crate::sha1::compress_portable as sha1_compress;
    pub use crate::sha256::compress_portable as sha256_compress;
}

pub use arch::force_portable;

/// Which hardware accelerations this CPU has.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each field is an independent CPU capability, not a state machine"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accel {
    /// Has the instructions for CRC32C.
    pub crc32c: bool,
    /// Has the instructions for SHA-1.
    pub sha1: bool,
    /// Has the instructions for SHA-256.
    pub sha256: bool,
    /// Has the vector instructions used by Adler-32 (NEON, SSSE3).
    pub adler32: bool,
}

/// Detects the available accelerations (computed once and cached).
#[must_use]
pub fn accel() -> Accel {
    arch::detect()
}
