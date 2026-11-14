//! HKDF (RFC 5869).

use crate::digest::{Digest, Output};
use crate::hmac::{Hmac, hmac};

/// Error of HKDF-Expand: more than 255 blocks of output asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTooLong;

/// HKDF-Extract: the pseudorandom key from `salt` and the input keying material.
#[must_use]
pub fn extract<D: Digest>(salt: &[u8], ikm: &[u8]) -> Output {
    hmac::<D>(salt, ikm)
}

/// HKDF-Expand: fills `out` from the pseudorandom key and `info`.
///
/// # Errors
///
/// [`OutputTooLong`] when `out` is longer than 255 blocks of the digest.
pub fn expand<D: Digest>(prk: &[u8], info: &[u8], out: &mut [u8]) -> Result<(), OutputTooLong> {
    if out.len() > 255 * D::OUTPUT_LEN {
        return Err(OutputTooLong);
    }
    fill::<D>(prk, info, out);
    Ok(())
}

/// HKDF-Expand into an array, whose length the compiler checks against the 255 blocks.
#[must_use]
pub fn expand_array<D: Digest, const N: usize>(prk: &[u8], info: &[u8]) -> [u8; N] {
    const {
        assert!(N <= 255 * D::OUTPUT_LEN, "HKDF-Expand gives at most 255 blocks");
    };
    let mut out = [0u8; N];
    fill::<D>(prk, info, &mut out);
    out
}

/// HKDF-Expand of at most 255 blocks.
fn fill<D: Digest>(prk: &[u8], info: &[u8], out: &mut [u8]) {
    let mut previous: Option<Output> = None;
    for (chunk, counter) in out.chunks_mut(D::OUTPUT_LEN).zip(1u8..=255) {
        let mut m = Hmac::<D>::new(prk);
        if let Some(p) = &previous {
            m.update(p.as_ref());
        }
        m.update(info);
        // At most 255 chunks, checked by the callers.
        m.update(&[counter]);
        let t = m.finish();
        chunk.copy_from_slice(&t.as_ref()[..chunk.len()]);
        previous = Some(t);
    }
}
