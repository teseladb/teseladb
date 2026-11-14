//! A common interface for the SHA family, used by HMAC, HKDF, PBKDF2, signatures and TLS.

use teseladb_hash::{Sha1, Sha256};

use crate::sha224::Sha224;
use crate::sha512::{Sha384, Sha512};

/// The largest digest (SHA-512).
pub const MAX_OUTPUT_LEN: usize = 64;

/// A digest value of up to 64 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Output {
    bytes: [u8; MAX_OUTPUT_LEN],
    len: usize,
}

impl Output {
    /// Builds an output from its bytes (at most 64).
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Output {
        let mut out = Output { bytes: [0; MAX_OUTPUT_LEN], len: bytes.len().min(MAX_OUTPUT_LEN) };
        out.bytes[..out.len].copy_from_slice(&bytes[..out.len]);
        out
    }
}

impl AsRef<[u8]> for Output {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Output({} bytes)", self.len)
    }
}

/// A hash function of the SHA family.
pub trait Digest: Clone {
    /// Input block length in bytes.
    const BLOCK_LEN: usize;
    /// Output length in bytes.
    const OUTPUT_LEN: usize;
    /// The algorithm.
    const ALGORITHM: HashAlgorithm;
    /// Initial state.
    fn new() -> Self;
    /// Adds data.
    fn update(&mut self, data: &[u8]);
    /// Final digest.
    fn finish(self) -> Output;
    /// Digest of `data` in one call.
    #[must_use]
    fn digest(data: &[u8]) -> Output {
        let mut h = Self::new();
        h.update(data);
        h.finish()
    }
}

impl Digest for Sha1 {
    const BLOCK_LEN: usize = 64;
    const OUTPUT_LEN: usize = 20;
    const ALGORITHM: HashAlgorithm = HashAlgorithm::Sha1;
    fn new() -> Self {
        Sha1::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha1::update(self, data);
    }
    fn finish(self) -> Output {
        Output::from_slice(&self.finalize())
    }
}

impl Digest for Sha256 {
    const BLOCK_LEN: usize = 64;
    const OUTPUT_LEN: usize = 32;
    const ALGORITHM: HashAlgorithm = HashAlgorithm::Sha256;
    fn new() -> Self {
        Sha256::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha256::update(self, data);
    }
    fn finish(self) -> Output {
        Output::from_slice(&self.finalize())
    }
}

impl Digest for Sha224 {
    const BLOCK_LEN: usize = 64;
    const OUTPUT_LEN: usize = 28;
    const ALGORITHM: HashAlgorithm = HashAlgorithm::Sha224;
    fn new() -> Self {
        Sha224::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha224::update(self, data);
    }
    fn finish(self) -> Output {
        Output::from_slice(&self.finalize())
    }
}

impl Digest for Sha384 {
    const BLOCK_LEN: usize = 128;
    const OUTPUT_LEN: usize = 48;
    const ALGORITHM: HashAlgorithm = HashAlgorithm::Sha384;
    fn new() -> Self {
        Sha384::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha384::update(self, data);
    }
    fn finish(self) -> Output {
        Output::from_slice(&self.finalize())
    }
}

impl Digest for Sha512 {
    const BLOCK_LEN: usize = 128;
    const OUTPUT_LEN: usize = 64;
    const ALGORITHM: HashAlgorithm = HashAlgorithm::Sha512;
    fn new() -> Self {
        Sha512::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha512::update(self, data);
    }
    fn finish(self) -> Output {
        Output::from_slice(&self.finalize())
    }
}

/// A hash algorithm chosen at run time (TLS cipher suites, certificate signatures).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HashAlgorithm {
    /// SHA-1 (only for SCRAM-SHA-1, HMAC-SHA1 of `$clusterTime` and Kerberos).
    Sha1,
    /// SHA-224 (only for verifying RSA and ECDSA signatures that use it).
    Sha224,
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
}

impl HashAlgorithm {
    /// Output length in bytes.
    #[must_use]
    pub fn output_len(self) -> usize {
        match self {
            HashAlgorithm::Sha1 => Sha1::OUTPUT_LEN,
            HashAlgorithm::Sha224 => Sha224::OUTPUT_LEN,
            HashAlgorithm::Sha256 => Sha256::OUTPUT_LEN,
            HashAlgorithm::Sha384 => Sha384::OUTPUT_LEN,
            HashAlgorithm::Sha512 => Sha512::OUTPUT_LEN,
        }
    }

    /// Digest of `data`.
    #[must_use]
    pub fn hash(self, data: &[u8]) -> Output {
        self.hash_parts(&[data])
    }

    /// Digest of the concatenation of `parts`.
    #[must_use]
    pub fn hash_parts(self, parts: &[&[u8]]) -> Output {
        fn run<D: Digest>(parts: &[&[u8]]) -> Output {
            let mut h = D::new();
            for p in parts {
                h.update(p);
            }
            h.finish()
        }
        match self {
            HashAlgorithm::Sha1 => run::<Sha1>(parts),
            HashAlgorithm::Sha224 => run::<Sha224>(parts),
            HashAlgorithm::Sha256 => run::<Sha256>(parts),
            HashAlgorithm::Sha384 => run::<Sha384>(parts),
            HashAlgorithm::Sha512 => run::<Sha512>(parts),
        }
    }

    /// HMAC of `data` with `key`.
    #[must_use]
    pub fn hmac(self, key: &[u8], data: &[u8]) -> Output {
        self.hmac_parts(key, &[data])
    }

    /// HMAC of the concatenation of `parts` with `key`.
    #[must_use]
    pub fn hmac_parts(self, key: &[u8], parts: &[&[u8]]) -> Output {
        fn run<D: Digest>(key: &[u8], parts: &[&[u8]]) -> Output {
            let mut m = crate::hmac::Hmac::<D>::new(key);
            for p in parts {
                m.update(p);
            }
            m.finish()
        }
        match self {
            HashAlgorithm::Sha1 => run::<Sha1>(key, parts),
            HashAlgorithm::Sha224 => run::<Sha224>(key, parts),
            HashAlgorithm::Sha256 => run::<Sha256>(key, parts),
            HashAlgorithm::Sha384 => run::<Sha384>(key, parts),
            HashAlgorithm::Sha512 => run::<Sha512>(key, parts),
        }
    }
}
