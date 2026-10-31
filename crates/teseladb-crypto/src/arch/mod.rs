//! Run-time selection between the portable code and the CPU's AES and carry-less multiplication
//! instructions (ARMv8 `aes` with PMULL, x86-64 AES-NI with PCLMULQDQ).
//!
//! It is the only module of the crate with `unsafe` (constitution, technical restrictions): the
//! accelerated functions use `#[target_feature]` and are only called after checking that the CPU
//! has those extensions.
#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

static DETECTED: OnceLock<bool> = OnceLock::new();
static FORCE_PORTABLE: AtomicBool = AtomicBool::new(false);

fn detect() -> bool {
    *DETECTED.get_or_init(|| {
        #[cfg(target_arch = "aarch64")]
        {
            std::arch::is_aarch64_feature_detected!("aes")
        }
        #[cfg(target_arch = "x86_64")]
        {
            std::arch::is_x86_feature_detected!("aes")
                && std::arch::is_x86_feature_detected!("pclmulqdq")
                && std::arch::is_x86_feature_detected!("sse2")
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            false
        }
    })
}

/// Whether AES and GHASH use the CPU's instructions.
pub fn accelerated() -> bool {
    !FORCE_PORTABLE.load(Ordering::Relaxed) && detect()
}

/// Turns forced portable mode on or off. Only for tests and benchmarks.
#[doc(hidden)]
pub fn force_portable(on: bool) {
    FORCE_PORTABLE.store(on, Ordering::Relaxed);
}

/// Encrypts a block with the expanded round keys; `None` when the CPU path is not available.
pub(crate) fn aes_encrypt(rk: &[[u8; 16]; 15], rounds: usize, block: &mut [u8; 16]) -> bool {
    if !accelerated() {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `detect` checked the ARMv8 `aes` extension.
    unsafe {
        aarch64::aes_encrypt(rk, rounds, block);
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `detect` checked AES-NI and SSE2.
    unsafe {
        x86_64::aes_encrypt(rk, rounds, block);
    }
    true
}

/// Decrypts a block with the encryption round keys `rk` and the InvMixColumns of each, `imc`.
pub(crate) fn aes_decrypt(
    rk: &[[u8; 16]; 15],
    imc: &[[u8; 16]; 15],
    rounds: usize,
    block: &mut [u8; 16],
) -> bool {
    if !accelerated() {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `detect` checked the ARMv8 `aes` extension.
    unsafe {
        aarch64::aes_decrypt(rk, imc, rounds, block);
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `detect` checked AES-NI and SSE2.
    unsafe {
        x86_64::aes_decrypt(rk, imc, rounds, block);
    }
    true
}

/// The 128-bit carry-less product of two 64-bit polynomials, when the CPU has the instruction.
pub(crate) fn clmul64(a: u64, b: u64) -> Option<u128> {
    if !accelerated() {
        return None;
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `detect` checked the ARMv8 `aes` extension, which includes PMULL.
    return Some(unsafe { aarch64::clmul64(a, b) });
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `detect` checked PCLMULQDQ and SSE2.
    return Some(unsafe { x86_64::clmul64(a, b) });
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    None
}
