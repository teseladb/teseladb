//! RSA signatures (RFC 8017): RSASSA-PKCS1-v1_5 and RSASSA-PSS with MGF1, both for signing and
//! verification. Signing exponentiates with the private exponent in constant time and checks
//! the result with the public exponent before releasing it.

use crate::bigint::{self, MAX_LIMBS, Mont, ct_lt};
use crate::ct::ct_eq;
use crate::digest::HashAlgorithm;

/// Smallest accepted modulus, in bits.
pub const MIN_MODULUS_BITS: usize = 1024;

/// Error for a malformed key, a message too long for the modulus, or a failed self-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsaError;

/// An RSA public key.
#[derive(Clone, Debug)]
pub struct RsaPublicKey {
    mont: Mont,
    e: Vec<u64>,
    bytes: usize,
    bits: usize,
}

fn to_limbs(be: &[u8]) -> Vec<u64> {
    let start = be.iter().position(|&b| b != 0).unwrap_or(be.len());
    let be = &be[start..];
    let mut l = vec![0u64; be.len().div_ceil(8).max(1)];
    bigint::from_be_bytes(be, &mut l);
    l
}

impl RsaPublicKey {
    /// Builds a key from the big-endian modulus and public exponent.
    pub fn new(n: &[u8], e: &[u8]) -> Result<RsaPublicKey, RsaError> {
        let n = to_limbs(n);
        let e = to_limbs(e);
        let top = n.last().copied().unwrap_or(0);
        let bits = 64 * n.len() - top.leading_zeros() as usize;
        let e_ok = e[0] & 1 == 1 && (e.len() > 1 || e[0] >= 3);
        if bits < MIN_MODULUS_BITS
            || n.len() > MAX_LIMBS
            || n[0] & 1 == 0
            || !e_ok
            || e.len() > n.len()
        {
            return Err(RsaError);
        }
        Ok(RsaPublicKey { mont: Mont::new(&n), e, bytes: bits.div_ceil(8), bits })
    }

    /// Modulus length in bytes.
    pub fn modulus_len(&self) -> usize {
        self.bytes
    }

    /// Modulus length in bits.
    pub fn modulus_bits(&self) -> usize {
        self.bits
    }

    /// The modulus, big-endian.
    pub fn modulus(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.bytes];
        bigint::to_be_bytes(&self.mont.m, &mut out);
        out
    }

    /// The public exponent, big-endian without leading zeros.
    pub fn exponent(&self) -> Vec<u8> {
        let mut out = vec![0u8; 8 * self.e.len()];
        bigint::to_be_bytes(&self.e, &mut out);
        let start = out.iter().position(|&b| b != 0).unwrap_or(out.len() - 1);
        out.split_off(start)
    }

    /// RSAVP1: `s^e mod n` for a signature of exactly the modulus length, below n.
    fn public_op(&self, sig: &[u8]) -> Option<Vec<u8>> {
        if sig.len() != self.bytes {
            return None;
        }
        let k = self.mont.limbs();
        let mut s = vec![0u64; k];
        bigint::from_be_bytes(sig, &mut s);
        if ct_lt(&s, &self.mont.m) == 0 {
            return None;
        }
        let mut sm = vec![0u64; k];
        self.mont.to_mont(&s, &mut sm);
        let mut r = vec![0u64; k];
        self.mont.pow(&sm, &self.e, &mut r);
        let mut plain = vec![0u64; k];
        self.mont.mont_to_plain(&r, &mut plain);
        let mut out = vec![0u8; self.bytes];
        bigint::to_be_bytes(&plain, &mut out);
        Some(out)
    }

    /// Verifies RSASSA-PKCS1-v1_5 over a digest made with `hash`.
    pub fn verify_pkcs1v15_digest(&self, hash: HashAlgorithm, digest: &[u8], sig: &[u8]) -> bool {
        let Some(em) = self.public_op(sig) else {
            return false;
        };
        pkcs1v15_encode(hash, digest, self.bytes).is_some_and(|want| ct_eq(&em, &want))
    }

    /// Hashes `msg` and verifies RSASSA-PKCS1-v1_5.
    pub fn verify_pkcs1v15(&self, hash: HashAlgorithm, msg: &[u8], sig: &[u8]) -> bool {
        self.verify_pkcs1v15_digest(hash, hash.hash(msg).as_ref(), sig)
    }

    /// Verifies RSASSA-PSS over a digest with MGF1 of the same hash and a salt of `salt_len`.
    pub fn verify_pss_digest(
        &self,
        hash: HashAlgorithm,
        digest: &[u8],
        salt_len: usize,
        sig: &[u8],
    ) -> bool {
        let Some(em_full) = self.public_op(sig) else {
            return false;
        };
        let em_bits = self.bits - 1;
        let em_len = em_bits.div_ceil(8);
        // The leading byte is zero when emLen is one byte shorter than the modulus.
        let (lead, em) = em_full.split_at(self.bytes - em_len);
        if lead.iter().any(|&b| b != 0) {
            return false;
        }
        pss_verify(hash, digest, em, em_bits, salt_len)
    }

    /// Hashes `msg` and verifies RSASSA-PSS.
    pub fn verify_pss(&self, hash: HashAlgorithm, msg: &[u8], salt_len: usize, sig: &[u8]) -> bool {
        self.verify_pss_digest(hash, hash.hash(msg).as_ref(), salt_len, sig)
    }
}

/// An RSA private key.
#[derive(Clone)]
pub struct RsaPrivateKey {
    public: RsaPublicKey,
    d: Vec<u64>,
}

impl std::fmt::Debug for RsaPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RsaPrivateKey({} bits)", self.public.bits)
    }
}

impl Drop for RsaPrivateKey {
    fn drop(&mut self) {
        self.d.fill(0);
        std::hint::black_box(&self.d);
    }
}

impl RsaPrivateKey {
    /// Builds a key from the modulus, public exponent and private exponent (big-endian).
    pub fn new(n: &[u8], e: &[u8], d: &[u8]) -> Result<RsaPrivateKey, RsaError> {
        let public = RsaPublicKey::new(n, e)?;
        let mut dl = vec![0u64; public.mont.limbs()];
        if !bigint::from_be_bytes(d, &mut dl) || ct_lt(&dl, &public.mont.m) == 0 {
            return Err(RsaError);
        }
        Ok(RsaPrivateKey { public, d: dl })
    }

    /// The public half.
    pub fn public_key(&self) -> &RsaPublicKey {
        &self.public
    }

    /// RSASP1 with the self-check: `m^d mod n`, verified by `s^e = m`.
    fn private_op(&self, em: &[u8]) -> Result<Vec<u8>, RsaError> {
        let p = &self.public;
        let k = p.mont.limbs();
        let mut m = vec![0u64; k];
        bigint::from_be_bytes(em, &mut m);
        let mut mm = vec![0u64; k];
        p.mont.to_mont(&m, &mut mm);
        let mut r = vec![0u64; k];
        p.mont.pow(&mm, &self.d, &mut r);
        let mut s = vec![0u64; k];
        p.mont.mont_to_plain(&r, &mut s);
        let mut out = vec![0u8; p.bytes];
        bigint::to_be_bytes(&s, &mut out);
        let mut padded = vec![0u8; p.bytes];
        padded[p.bytes - em.len()..].copy_from_slice(em);
        match p.public_op(&out) {
            Some(back) if ct_eq(&back, &padded) => Ok(out),
            _ => Err(RsaError),
        }
    }

    /// Signs a digest with RSASSA-PKCS1-v1_5.
    pub fn sign_pkcs1v15_digest(
        &self,
        hash: HashAlgorithm,
        digest: &[u8],
    ) -> Result<Vec<u8>, RsaError> {
        let em = pkcs1v15_encode(hash, digest, self.public.bytes).ok_or(RsaError)?;
        self.private_op(&em)
    }

    /// Hashes `msg` and signs it with RSASSA-PKCS1-v1_5.
    pub fn sign_pkcs1v15(&self, hash: HashAlgorithm, msg: &[u8]) -> Result<Vec<u8>, RsaError> {
        self.sign_pkcs1v15_digest(hash, hash.hash(msg).as_ref())
    }

    /// Signs a digest with RSASSA-PSS, MGF1 of the same hash and the given salt.
    pub fn sign_pss_digest(
        &self,
        hash: HashAlgorithm,
        digest: &[u8],
        salt: &[u8],
    ) -> Result<Vec<u8>, RsaError> {
        let em_bits = self.public.bits - 1;
        let em = pss_encode(hash, digest, salt, em_bits).ok_or(RsaError)?;
        self.private_op(&em)
    }

    /// Hashes `msg` and signs it with RSASSA-PSS.
    pub fn sign_pss(
        &self,
        hash: HashAlgorithm,
        msg: &[u8],
        salt: &[u8],
    ) -> Result<Vec<u8>, RsaError> {
        self.sign_pss_digest(hash, hash.hash(msg).as_ref(), salt)
    }
}

/// The DER `DigestInfo` prefix of each hash (RFC 8017, section 9.2, note 1).
fn digest_info_prefix(hash: HashAlgorithm) -> &'static [u8] {
    match hash {
        HashAlgorithm::Sha1 => &[
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
            0x14,
        ],
        HashAlgorithm::Sha224 => &[
            0x30, 0x2d, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x04, 0x05, 0x00, 0x04, 0x1c,
        ],
        HashAlgorithm::Sha256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ],
        HashAlgorithm::Sha384 => &[
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x02, 0x05, 0x00, 0x04, 0x30,
        ],
        HashAlgorithm::Sha512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x03, 0x05, 0x00, 0x04, 0x40,
        ],
    }
}

/// EMSA-PKCS1-v1_5: `00 01 FF… 00 ‖ DigestInfo`, at least eight bytes of `FF`.
fn pkcs1v15_encode(hash: HashAlgorithm, digest: &[u8], k: usize) -> Option<Vec<u8>> {
    if digest.len() != hash.output_len() {
        return None;
    }
    let prefix = digest_info_prefix(hash);
    // k ≥ 128 (MIN_MODULUS_BITS) leaves room for the longest DigestInfo (83 bytes) and 11 more.
    let t_len = prefix.len() + digest.len();
    let mut em = vec![0xffu8; k];
    em[0] = 0;
    em[1] = 1;
    em[k - t_len - 1] = 0;
    em[k - t_len..k - digest.len()].copy_from_slice(prefix);
    em[k - digest.len()..].copy_from_slice(digest);
    Some(em)
}

/// MGF1 (RFC 8017, B.2.1) XORed into `out`.
fn mgf1_xor(hash: HashAlgorithm, seed: &[u8], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(hash.output_len()).enumerate() {
        let block = hash.hash_parts(&[seed, &(i as u32).to_be_bytes()]);
        for (o, b) in chunk.iter_mut().zip(block.as_ref()) {
            *o ^= b;
        }
    }
}

fn pss_mprime(hash: HashAlgorithm, digest: &[u8], salt: &[u8]) -> Vec<u8> {
    hash.hash_parts(&[&[0u8; 8], digest, salt]).as_ref().to_vec()
}

/// EMSA-PSS-ENCODE (RFC 8017, 9.1.1).
fn pss_encode(hash: HashAlgorithm, digest: &[u8], salt: &[u8], em_bits: usize) -> Option<Vec<u8>> {
    let h_len = hash.output_len();
    let em_len = em_bits.div_ceil(8);
    if digest.len() != h_len || em_len < h_len + salt.len() + 2 {
        return None;
    }
    let h = pss_mprime(hash, digest, salt);
    let db_len = em_len - h_len - 1;
    let mut db = vec![0u8; db_len];
    db[db_len - salt.len() - 1] = 1;
    db[db_len - salt.len()..].copy_from_slice(salt);
    mgf1_xor(hash, &h, &mut db);
    db[0] &= 0xff >> (8 * em_len - em_bits);
    let mut em = db;
    em.extend_from_slice(&h);
    em.push(0xbc);
    Some(em)
}

/// EMSA-PSS-VERIFY (RFC 8017, 9.1.2).
fn pss_verify(
    hash: HashAlgorithm,
    digest: &[u8],
    em: &[u8],
    em_bits: usize,
    salt_len: usize,
) -> bool {
    let h_len = hash.output_len();
    let em_len = em.len();
    if digest.len() != h_len || em_len < h_len + salt_len + 2 || em[em_len - 1] != 0xbc {
        return false;
    }
    let (masked, rest) = em.split_at(em_len - h_len - 1);
    let h = &rest[..h_len];
    let top_mask = !(0xffu8 >> (8 * em_len - em_bits));
    if masked[0] & top_mask != 0 {
        return false;
    }
    let mut db = masked.to_vec();
    mgf1_xor(hash, h, &mut db);
    db[0] &= !top_mask;
    let ps_len = em_len - h_len - salt_len - 2;
    if db[..ps_len].iter().any(|&b| b != 0) || db[ps_len] != 1 {
        return false;
    }
    let salt = &db[ps_len + 1..];
    ct_eq(&pss_mprime(hash, digest, salt), h)
}
