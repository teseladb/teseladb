//! Run-time selection between the portable and the accelerated implementations.
//!
//! It is the only module of the crate with `unsafe`: the accelerated functions use
//! `#[target_feature]` and are only called after checking that the CPU has those extensions.
#![allow(unsafe_code)]

use std::sync::OnceLock;

use crate::Accel;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

static ACCEL: OnceLock<Accel> = OnceLock::new();

/// Forces the portable implementations (for tests and to compare performance).
static FORCE_PORTABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn detect() -> Accel {
    *ACCEL.get_or_init(|| {
        #[cfg(target_arch = "aarch64")]
        {
            Accel {
                crc32c: std::arch::is_aarch64_feature_detected!("crc"),
                sha1: std::arch::is_aarch64_feature_detected!("sha2"),
                sha256: std::arch::is_aarch64_feature_detected!("sha2"),
                adler32: std::arch::is_aarch64_feature_detected!("neon"),
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            Accel {
                crc32c: std::arch::is_x86_feature_detected!("sse4.2"),
                sha1: sha_ni(),
                sha256: sha_ni(),
                adler32: std::arch::is_x86_feature_detected!("ssse3"),
            }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            Accel { crc32c: false, sha1: false, sha256: false, adler32: false }
        }
    })
}

/// The SHA extensions with the SSSE3 and SSE4.1 shuffles their functions use.
#[cfg(target_arch = "x86_64")]
fn sha_ni() -> bool {
    std::arch::is_x86_feature_detected!("sha")
        && std::arch::is_x86_feature_detected!("ssse3")
        && std::arch::is_x86_feature_detected!("sse4.1")
}

fn active() -> Accel {
    if FORCE_PORTABLE.load(std::sync::atomic::Ordering::Relaxed) {
        Accel { crc32c: false, sha1: false, sha256: false, adler32: false }
    } else {
        detect()
    }
}

/// Turns forced portable mode on or off. Only for tests and benchmarks.
#[doc(hidden)]
pub fn force_portable(on: bool) {
    FORCE_PORTABLE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    if active().crc32c {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: `detect` checked the ARMv8 `crc` extension.
        return unsafe { aarch64::crc32c_update(crc, data) };
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `detect` checked SSE4.2.
        return unsafe { x86_64::crc32c_update(crc, data) };
    }
    crate::crc32c::update_portable(crc, data)
}

pub(crate) fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    if active().adler32 {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: `detect` checked NEON.
        return unsafe { aarch64::adler32_update(adler, data) };
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `detect` checked SSSE3.
        return unsafe { x86_64::adler32_update(adler, data) };
    }
    crate::adler32::adler32_portable(adler, data)
}

pub(crate) fn sha1_compress(state: &mut [u32; 5], blocks: &[[u8; 64]]) {
    if blocks.is_empty() {
        return;
    }
    if active().sha1 {
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: `detect` checked the ARMv8 SHA extensions (`sha2` includes SHA-1).
            unsafe { aarch64::sha1_compress(state, blocks) };
            return;
        }
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `detect` checked the SHA extensions, SSSE3 and SSE4.1.
            unsafe { x86_64::sha1_compress(state, blocks) };
            return;
        }
    }
    crate::sha1::compress_portable(state, blocks);
}

pub(crate) fn sha256_compress(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    if blocks.is_empty() {
        return;
    }
    if active().sha256 {
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: `detect` checked the ARMv8 `sha2` extension.
            unsafe { aarch64::sha256_compress(state, blocks) };
            return;
        }
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `detect` checked the SHA extensions, SSSE3 and SSE4.1.
            unsafe { x86_64::sha256_compress(state, blocks) };
            return;
        }
    }
    crate::sha256::compress_portable(state, blocks);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn forced_portable_disables_every_accel() {
        force_portable(true);
        assert_eq!(active(), Accel { crc32c: false, sha1: false, sha256: false, adler32: false });
        let portable = crc32c_update(!0, b"123456789");
        force_portable(false);
        assert_eq!(active(), detect());
        assert_eq!(crc32c_update(!0, b"123456789"), portable);
        let mut st = [1u32; 5];
        sha1_compress(&mut st, &[]);
        assert_eq!(st, [1; 5]);
        let mut st = [1u32; 8];
        sha256_compress(&mut st, &[]);
        assert_eq!(st, [1; 8]);
    }
}
