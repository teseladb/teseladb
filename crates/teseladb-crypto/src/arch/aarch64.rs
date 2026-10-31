//! AES and PMULL with the ARMv8 cryptographic extension.

use core::arch::aarch64::*;

/// # Safety
/// The CPU must have the `aes` extension.
#[target_feature(enable = "aes")]
pub(super) unsafe fn aes_encrypt(rk: &[[u8; 16]; 15], rounds: usize, block: &mut [u8; 16]) {
    // SAFETY: `block` and every round key are 16 readable bytes.
    unsafe {
        let mut s = vld1q_u8(block.as_ptr());
        for k in &rk[..rounds - 1] {
            s = vaesmcq_u8(vaeseq_u8(s, vld1q_u8(k.as_ptr())));
        }
        s = veorq_u8(
            vaeseq_u8(s, vld1q_u8(rk[rounds - 1].as_ptr())),
            vld1q_u8(rk[rounds].as_ptr()),
        );
        vst1q_u8(block.as_mut_ptr(), s);
    }
}

/// # Safety
/// The CPU must have the `aes` extension.
#[target_feature(enable = "aes")]
pub(super) unsafe fn aes_decrypt(
    rk: &[[u8; 16]; 15],
    imc: &[[u8; 16]; 15],
    rounds: usize,
    block: &mut [u8; 16],
) {
    // SAFETY: `block` and every round key are 16 readable bytes.
    unsafe {
        let mut s = vaesdq_u8(vld1q_u8(block.as_ptr()), vld1q_u8(rk[rounds].as_ptr()));
        for k in imc[1..rounds].iter().rev() {
            s = vaesdq_u8(vaesimcq_u8(s), vld1q_u8(k.as_ptr()));
        }
        s = veorq_u8(s, vld1q_u8(rk[0].as_ptr()));
        vst1q_u8(block.as_mut_ptr(), s);
    }
}

/// # Safety
/// The CPU must have the `aes` extension (PMULL is part of it).
#[target_feature(enable = "aes")]
pub(super) unsafe fn clmul64(a: u64, b: u64) -> u128 {
    vmull_p64(a, b)
}
