//! AES-NI and PCLMULQDQ.

use core::arch::x86_64::{
    _mm_aesdec_si128, _mm_aesdeclast_si128, _mm_aesenc_si128, _mm_aesenclast_si128,
    _mm_clmulepi64_si128, _mm_loadu_si128, _mm_set_epi64x, _mm_storeu_si128, _mm_xor_si128,
};

/// # Safety
/// The CPU must have AES-NI and SSE2.
#[target_feature(enable = "aes,sse2")]
pub(super) unsafe fn aes_encrypt(rk: &[[u8; 16]; 15], rounds: usize, block: &mut [u8; 16]) {
    // SAFETY: `block` and every round key are 16 bytes; the unaligned load and store accept any
    // address.
    unsafe {
        let key = |i: usize| _mm_loadu_si128(rk[i].as_ptr().cast());
        let mut s = _mm_xor_si128(_mm_loadu_si128(block.as_ptr().cast()), key(0));
        for i in 1..rounds {
            s = _mm_aesenc_si128(s, key(i));
        }
        s = _mm_aesenclast_si128(s, key(rounds));
        _mm_storeu_si128(block.as_mut_ptr().cast(), s);
    }
}

/// # Safety
/// The CPU must have AES-NI and SSE2.
#[target_feature(enable = "aes,sse2")]
pub(super) unsafe fn aes_decrypt(
    rk: &[[u8; 16]; 15],
    imc: &[[u8; 16]; 15],
    rounds: usize,
    block: &mut [u8; 16],
) {
    // SAFETY: as in `aes_encrypt`.
    unsafe {
        let mut s = _mm_xor_si128(
            _mm_loadu_si128(block.as_ptr().cast()),
            _mm_loadu_si128(rk[rounds].as_ptr().cast()),
        );
        for k in imc[1..rounds].iter().rev() {
            s = _mm_aesdec_si128(s, _mm_loadu_si128(k.as_ptr().cast()));
        }
        s = _mm_aesdeclast_si128(s, _mm_loadu_si128(rk[0].as_ptr().cast()));
        _mm_storeu_si128(block.as_mut_ptr().cast(), s);
    }
}

/// # Safety
/// The CPU must have PCLMULQDQ and SSE2.
#[target_feature(enable = "pclmulqdq,sse2")]
pub(super) unsafe fn clmul64(a: u64, b: u64) -> u128 {
    let p = _mm_clmulepi64_si128(
        _mm_set_epi64x(0, a.cast_signed()),
        _mm_set_epi64x(0, b.cast_signed()),
        0x00,
    );
    let mut out = [0u8; 16];
    // SAFETY: `out` has 16 writable bytes; the unaligned store accepts any address.
    unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), p) };
    u128::from_le_bytes(out)
}
