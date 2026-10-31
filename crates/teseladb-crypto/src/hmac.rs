//! HMAC (RFC 2104, FIPS 198-1).

use crate::ct::ct_eq;
use crate::digest::{Digest, Output};

/// Incremental HMAC over the digest `D`.
#[derive(Clone)]
pub struct Hmac<D: Digest> {
    inner: D,
    outer: D,
}

impl<D: Digest> Hmac<D> {
    /// Keys a new HMAC. Keys longer than the block are hashed first.
    pub fn new(key: &[u8]) -> Hmac<D> {
        let mut block = [0u8; 128];
        if key.len() > D::BLOCK_LEN {
            let k = D::digest(key);
            block[..k.as_ref().len()].copy_from_slice(k.as_ref());
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0u8; 128];
        let mut opad = [0u8; 128];
        for i in 0..D::BLOCK_LEN {
            ipad[i] = block[i] ^ 0x36;
            opad[i] = block[i] ^ 0x5c;
        }
        let mut inner = D::new();
        inner.update(&ipad[..D::BLOCK_LEN]);
        let mut outer = D::new();
        outer.update(&opad[..D::BLOCK_LEN]);
        Hmac { inner, outer }
    }

    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Final tag.
    pub fn finish(self) -> Output {
        let inner = self.inner.finish();
        let mut outer = self.outer;
        outer.update(inner.as_ref());
        outer.finish()
    }

    /// Compares the final tag (or a prefix of it, at least 10 bytes) with `tag` in constant time.
    pub fn verify(self, tag: &[u8]) -> bool {
        let out = self.finish();
        let full = out.as_ref();
        tag.len() >= 10 && tag.len() <= full.len() && ct_eq(&full[..tag.len()], tag)
    }
}

/// HMAC of `data` with `key` in one call.
pub fn hmac<D: Digest>(key: &[u8], data: &[u8]) -> Output {
    let mut m = Hmac::<D>::new(key);
    m.update(data);
    m.finish()
}
