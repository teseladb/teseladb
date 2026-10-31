//! PBKDF2 (RFC 8018) with HMAC.

use crate::digest::Digest;
use crate::hmac::Hmac;

/// Fills `out` with PBKDF2-HMAC-`D` of `password` and `salt` with `iterations` rounds (at least 1).
pub fn pbkdf2<D: Digest>(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let keyed = Hmac::<D>::new(password);
    for (i, chunk) in out.chunks_mut(D::OUTPUT_LEN).enumerate() {
        let mut m = keyed.clone();
        m.update(salt);
        m.update(&(i as u32 + 1).to_be_bytes());
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
