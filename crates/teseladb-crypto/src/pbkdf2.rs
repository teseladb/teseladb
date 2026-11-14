//! PBKDF2 (RFC 8018) with HMAC.

use crate::digest::Digest;
use crate::hmac::Hmac;

/// Fills `out` with PBKDF2-HMAC-`D` of `password` and `salt` with `iterations` rounds (at least 1).
///
/// # Panics
///
/// When `out` is longer than 2³² − 1 blocks of the digest (RFC 8018, section 5.2).
pub fn pbkdf2<D: Digest>(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    assert!(
        u32::try_from(out.len().div_ceil(D::OUTPUT_LEN)).is_ok(),
        "PBKDF2 derives at most 2^32 - 1 blocks"
    );
    let keyed = Hmac::<D>::new(password);
    for (chunk, index) in out.chunks_mut(D::OUTPUT_LEN).zip(1..=u32::MAX) {
        let mut m = keyed.clone();
        m.update(salt);
        m.update(&index.to_be_bytes());
        let mut u = m.finish();
        let mut t = [0u8; 64];
        t[..D::OUTPUT_LEN].copy_from_slice(u.as_ref());
        for _ in 1..iterations.max(1) {
            let mut m = keyed.clone();
            m.update(u.as_ref());
            u = m.finish();
            for (a, b) in t.iter_mut().zip(u.as_ref()) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}
