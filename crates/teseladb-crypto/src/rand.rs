//! HMAC_DRBG with SHA-256 (NIST SP 800-90A Rev. 1, section 10.1.2).
//!
//! The crate does no I/O (principle II): the operating system's entropy comes from
//! `teseladb-sys` and is passed in as the seed; the simulator passes its seeded bytes instead.

use teseladb_hash::Sha256;

/// Default number of requests between two reseeds (the limit of SP 800-90A is 2⁴⁸).
pub const RESEED_INTERVAL: u64 = 1 << 32;

/// Error when the generator has served its reseed interval of requests without a reseed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReseedRequired;

/// A deterministic random bit generator.
#[derive(Clone)]
pub struct HmacDrbg {
    k: [u8; 32],
    v: [u8; 32],
    counter: u64,
    reseed_interval: u64,
}

impl std::fmt::Debug for HmacDrbg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HmacDrbg(requests {})", self.counter)
    }
}

impl Drop for HmacDrbg {
    fn drop(&mut self) {
        self.k = [0; 32];
        self.v = [0; 32];
        std::hint::black_box((&self.k, &self.v));
    }
}

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = crate::hmac::Hmac::<Sha256>::new(key);
    for p in parts {
        m.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(m.finish().as_ref());
    out
}

impl HmacDrbg {
    /// Instantiates from entropy, a nonce and an optional personalization string.
    pub fn new(entropy: &[u8], nonce: &[u8], personalization: &[u8]) -> HmacDrbg {
        let mut d =
            HmacDrbg { k: [0; 32], v: [1; 32], counter: 1, reseed_interval: RESEED_INTERVAL };
        d.update(&[entropy, nonce, personalization]);
        d
    }

    /// Lowers the number of requests allowed between reseeds (at most [`RESEED_INTERVAL`]).
    pub fn with_reseed_interval(mut self, requests: u64) -> HmacDrbg {
        self.reseed_interval = requests.min(RESEED_INTERVAL);
        self
    }

    fn update(&mut self, provided: &[&[u8]]) {
        let mut parts: Vec<&[u8]> = vec![&self.v, &[0]];
        parts.extend_from_slice(provided);
        self.k = mac(&self.k, &parts);
        self.v = mac(&self.k, &[&self.v]);
        if provided.iter().all(|p| p.is_empty()) {
            return;
        }
        let mut parts: Vec<&[u8]> = vec![&self.v, &[1]];
        parts.extend_from_slice(provided);
        self.k = mac(&self.k, &parts);
        self.v = mac(&self.k, &[&self.v]);
    }

    /// Mixes in fresh entropy and restarts the request counter.
    pub fn reseed(&mut self, entropy: &[u8], additional: &[u8]) {
        self.update(&[entropy, additional]);
        self.counter = 1;
    }

    /// Fills `out`, with optional additional input.
    pub fn generate(&mut self, out: &mut [u8], additional: &[u8]) -> Result<(), ReseedRequired> {
        if self.counter > self.reseed_interval {
            return Err(ReseedRequired);
        }
        if !additional.is_empty() {
            self.update(&[additional]);
        }
        for chunk in out.chunks_mut(32) {
            self.v = mac(&self.k, &[&self.v]);
            chunk.copy_from_slice(&self.v[..chunk.len()]);
        }
        self.update(&[additional]);
        self.counter += 1;
        Ok(())
    }
}
