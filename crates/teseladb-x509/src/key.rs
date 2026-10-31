//! Keys and signature algorithms: SubjectPublicKeyInfo (RFC 5280, 5480, 8410), private keys in
//! PKCS #8 (RFC 5958), SEC 1 and PKCS #1 (RFC 8017), PKCS #8 encrypted with PBES2 (RFC 8018), and
//! the `AlgorithmIdentifier`s of RSA PKCS #1 v1.5, RSASSA-PSS (RFC 4055), ECDSA (RFC 5758) and
//! Ed25519 (RFC 8410).

use teseladb_crypto::aes::Aes;
use teseladb_crypto::curve25519::{Ed25519KeyPair, ed25519_verify};
use teseladb_crypto::ec::{self, EcPrivateKey, NamedCurve};
use teseladb_crypto::modes;
use teseladb_crypto::pbkdf2::pbkdf2;
use teseladb_crypto::rsa::{RsaPrivateKey, RsaPublicKey};
use teseladb_crypto::sha512::{Sha384, Sha512};
use teseladb_crypto::{HashAlgorithm, Sha1, Sha256};

use crate::Error;
use crate::der::{self, tag};
use crate::oid;
use crate::pem;

/// Default salt length of RSASSA-PSS parameters (RFC 4055).
const PSS_DEFAULT_SALT: u64 = 20;
/// Largest PBKDF2 iteration count accepted when decrypting a key (bounds the work an attacker
/// controlled file can ask for).
pub const MAX_PBKDF2_ITERATIONS: u64 = 10_000_000;

/// A signature algorithm of certificates, CRLs and CSRs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureAlgorithm {
    /// RSASSA-PKCS1-v1_5 with the hash.
    RsaPkcs1(HashAlgorithm),
    /// RSASSA-PSS with the hash, MGF1 of the same hash and the salt length.
    RsaPss {
        /// The message hash and the MGF1 hash.
        hash: HashAlgorithm,
        /// The salt length in bytes.
        salt_len: usize,
    },
    /// ECDSA with the hash.
    Ecdsa(HashAlgorithm),
    /// Ed25519.
    Ed25519,
}

fn hash_from_oid(o: &[u8]) -> Result<HashAlgorithm, Error> {
    Ok(match o {
        oid::SHA1 => HashAlgorithm::Sha1,
        oid::SHA224 => HashAlgorithm::Sha224,
        oid::SHA256 => HashAlgorithm::Sha256,
        oid::SHA384 => HashAlgorithm::Sha384,
        oid::SHA512 => HashAlgorithm::Sha512,
        _ => return Err(Error::UnsupportedAlgorithm),
    })
}

fn hash_oid(h: HashAlgorithm) -> &'static [u8] {
    match h {
        HashAlgorithm::Sha1 => oid::SHA1,
        HashAlgorithm::Sha224 => oid::SHA224,
        HashAlgorithm::Sha256 => oid::SHA256,
        HashAlgorithm::Sha384 => oid::SHA384,
        HashAlgorithm::Sha512 => oid::SHA512,
    }
}

/// Reads a hash `AlgorithmIdentifier` whose parameters are absent or NULL.
fn read_hash_alg(data: &[u8]) -> Result<HashAlgorithm, Error> {
    let mut r = der::Reader::new(data);
    let h = hash_from_oid(der::oid(r.read(tag::OID)?)?)?;
    if r.peek_tag() == Some(tag::NULL) {
        der::read_single(r.read_any()?.raw, tag::NULL)
            .and_then(|t| if t.value.is_empty() { Ok(()) } else { Err(Error::Der) })?;
    }
    r.finish()?;
    Ok(h)
}

/// Absent or NULL parameters, as RFC 4055 and 8017 allow for the PKCS #1 algorithms.
fn null_or_absent(r: &mut der::Reader<'_>) -> Result<(), Error> {
    if let Some(v) = r.read_optional(tag::NULL)?
        && !v.is_empty()
    {
        return Err(Error::Der);
    }
    r.finish()
}

fn parse_pss_params(data: &[u8]) -> Result<SignatureAlgorithm, Error> {
    let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
    let mut hash = HashAlgorithm::Sha1;
    if let Some(v) = r.read_optional(tag::context(0))? {
        hash = read_hash_alg(der::read_single(v, tag::SEQUENCE)?.value)?;
    }
    let mut mgf_hash = HashAlgorithm::Sha1;
    if let Some(v) = r.read_optional(tag::context(1))? {
        let mut m = der::Reader::new(der::read_single(v, tag::SEQUENCE)?.value);
        if der::oid(m.read(tag::OID)?)? != oid::MGF1 {
            return Err(Error::UnsupportedAlgorithm);
        }
        mgf_hash = read_hash_alg(m.read(tag::SEQUENCE)?)?;
        m.finish()?;
    }
    let mut salt = PSS_DEFAULT_SALT;
    if let Some(v) = r.read_optional(tag::context(2))? {
        salt = der::small_unsigned(der::read_single(v, tag::INTEGER)?.value)?;
    }
    if let Some(v) = r.read_optional(tag::context(3))?
        && der::small_unsigned(der::read_single(v, tag::INTEGER)?.value)? != 1
    {
        return Err(Error::UnsupportedAlgorithm);
    }
    r.finish()?;
    if mgf_hash != hash {
        return Err(Error::UnsupportedAlgorithm);
    }
    let salt_len = usize::try_from(salt).map_err(|_| Error::Der)?;
    Ok(SignatureAlgorithm::RsaPss { hash, salt_len })
}

impl SignatureAlgorithm {
    /// Parses the contents of an `AlgorithmIdentifier` SEQUENCE.
    pub fn from_der(value: &[u8]) -> Result<SignatureAlgorithm, Error> {
        let mut r = der::Reader::new(value);
        let o = der::oid(r.read(tag::OID)?)?;
        let rsa = |h| -> Result<SignatureAlgorithm, Error> {
            null_or_absent(&mut r.clone())?;
            Ok(SignatureAlgorithm::RsaPkcs1(h))
        };
        let bare = |alg| -> Result<SignatureAlgorithm, Error> {
            r.finish()?;
            Ok(alg)
        };
        match o {
            oid::SHA1_WITH_RSA => rsa(HashAlgorithm::Sha1),
            oid::SHA224_WITH_RSA => rsa(HashAlgorithm::Sha224),
            oid::SHA256_WITH_RSA => rsa(HashAlgorithm::Sha256),
            oid::SHA384_WITH_RSA => rsa(HashAlgorithm::Sha384),
            oid::SHA512_WITH_RSA => rsa(HashAlgorithm::Sha512),
            oid::RSASSA_PSS => {
                let params = r.read_any()?;
                r.finish()?;
                parse_pss_params(params.raw)
            }
            oid::ECDSA_SHA1 => bare(SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha1)),
            oid::ECDSA_SHA224 => bare(SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha224)),
            oid::ECDSA_SHA256 => bare(SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha256)),
            oid::ECDSA_SHA384 => bare(SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha384)),
            oid::ECDSA_SHA512 => bare(SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha512)),
            oid::ED25519 => bare(SignatureAlgorithm::Ed25519),
            _ => Err(Error::UnsupportedAlgorithm),
        }
    }

    /// The `AlgorithmIdentifier` SEQUENCE.
    pub fn to_der(self) -> Vec<u8> {
        match self {
            SignatureAlgorithm::RsaPkcs1(h) => {
                let o = match h {
                    HashAlgorithm::Sha1 => oid::SHA1_WITH_RSA,
                    HashAlgorithm::Sha224 => oid::SHA224_WITH_RSA,
                    HashAlgorithm::Sha256 => oid::SHA256_WITH_RSA,
                    HashAlgorithm::Sha384 => oid::SHA384_WITH_RSA,
                    HashAlgorithm::Sha512 => oid::SHA512_WITH_RSA,
                };
                der::sequence(&[&der::object_id(o), &der::null()])
            }
            SignatureAlgorithm::RsaPss { hash, salt_len } => {
                let h = der::sequence(&[&der::object_id(hash_oid(hash))]);
                let mgf = der::sequence(&[&der::object_id(oid::MGF1), &h]);
                let params = der::sequence(&[
                    &der::tlv(tag::context(0), &h),
                    &der::tlv(tag::context(1), &mgf),
                    &der::tlv(tag::context(2), &der::small_integer(salt_len as u64)),
                ]);
                der::sequence(&[&der::object_id(oid::RSASSA_PSS), &params])
            }
            SignatureAlgorithm::Ecdsa(h) => {
                let o = match h {
                    HashAlgorithm::Sha1 => oid::ECDSA_SHA1,
                    HashAlgorithm::Sha224 => oid::ECDSA_SHA224,
                    HashAlgorithm::Sha256 => oid::ECDSA_SHA256,
                    HashAlgorithm::Sha384 => oid::ECDSA_SHA384,
                    HashAlgorithm::Sha512 => oid::ECDSA_SHA512,
                };
                der::sequence(&[&der::object_id(o)])
            }
            SignatureAlgorithm::Ed25519 => der::sequence(&[&der::object_id(oid::ED25519)]),
        }
    }
}

/// A public key of a certificate or CSR.
#[derive(Clone, Debug)]
pub enum PublicKey {
    /// RSA.
    Rsa(RsaPublicKey),
    /// ECDSA or ECDH on a NIST curve, as an uncompressed point.
    Ec {
        /// The curve.
        curve: NamedCurve,
        /// The point `04 ‖ x ‖ y`.
        point: Vec<u8>,
    },
    /// Ed25519.
    Ed25519([u8; 32]),
}

fn curve_from_oid(o: &[u8]) -> Result<NamedCurve, Error> {
    match o {
        oid::P256 => Ok(NamedCurve::P256),
        oid::P384 => Ok(NamedCurve::P384),
        oid::P521 => Ok(NamedCurve::P521),
        _ => Err(Error::UnsupportedAlgorithm),
    }
}

fn curve_oid(c: NamedCurve) -> &'static [u8] {
    match c {
        NamedCurve::P256 => oid::P256,
        NamedCurve::P384 => oid::P384,
        NamedCurve::P521 => oid::P521,
    }
}

fn parse_rsa_public(data: &[u8]) -> Result<RsaPublicKey, Error> {
    let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
    let n = der::unsigned(r.read(tag::INTEGER)?)?;
    let e = der::unsigned(r.read(tag::INTEGER)?)?;
    r.finish()?;
    RsaPublicKey::new(n, e).map_err(|_| Error::InvalidKey)
}

impl PublicKey {
    /// Parses a DER SubjectPublicKeyInfo.
    pub fn from_spki(data: &[u8]) -> Result<PublicKey, Error> {
        let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
        let alg = r.read(tag::SEQUENCE)?;
        let key = der::octet_aligned_bits(r.read(tag::BIT_STRING)?)?;
        r.finish()?;
        let mut a = der::Reader::new(alg);
        match der::oid(a.read(tag::OID)?)? {
            oid::RSA_ENCRYPTION => {
                null_or_absent(&mut a)?;
                Ok(PublicKey::Rsa(parse_rsa_public(key)?))
            }
            // An RSA key restricted to PSS: the parameters only narrow its use.
            oid::RSASSA_PSS => Ok(PublicKey::Rsa(parse_rsa_public(key)?)),
            oid::EC_PUBLIC_KEY => {
                let curve = curve_from_oid(der::oid(a.read(tag::OID)?)?)?;
                a.finish()?;
                let len = 1 + 2 * curve.scalar_len();
                if key.len() != len || key[0] != 0x04 {
                    return Err(Error::InvalidKey);
                }
                Ok(PublicKey::Ec { curve, point: key.to_vec() })
            }
            oid::ED25519 => {
                a.finish()?;
                let k: [u8; 32] = key.try_into().map_err(|_| Error::InvalidKey)?;
                Ok(PublicKey::Ed25519(k))
            }
            _ => Err(Error::UnsupportedAlgorithm),
        }
    }

    /// The DER SubjectPublicKeyInfo.
    pub fn to_spki(&self) -> Vec<u8> {
        let (alg, key) = match self {
            PublicKey::Rsa(k) => (
                der::sequence(&[&der::object_id(oid::RSA_ENCRYPTION), &der::null()]),
                der::sequence(&[
                    &der::unsigned_integer(&k.modulus()),
                    &der::unsigned_integer(&k.exponent()),
                ]),
            ),
            PublicKey::Ec { curve, point } => (
                der::sequence(&[
                    &der::object_id(oid::EC_PUBLIC_KEY),
                    &der::object_id(curve_oid(*curve)),
                ]),
                point.clone(),
            ),
            PublicKey::Ed25519(k) => (der::sequence(&[&der::object_id(oid::ED25519)]), k.to_vec()),
        };
        der::sequence(&[&alg, &der::bits(&key)])
    }

    /// The bytes of the `subjectPublicKey` BIT STRING (what key identifiers hash).
    pub fn key_bytes(&self) -> Vec<u8> {
        match self {
            PublicKey::Rsa(k) => der::sequence(&[
                &der::unsigned_integer(&k.modulus()),
                &der::unsigned_integer(&k.exponent()),
            ]),
            PublicKey::Ec { point, .. } => point.clone(),
            PublicKey::Ed25519(k) => k.to_vec(),
        }
    }

    /// Verifies `sig` over `msg`. ECDSA signatures are DER `Ecdsa-Sig-Value`s, as in X.509.
    pub fn verify(&self, alg: SignatureAlgorithm, msg: &[u8], sig: &[u8]) -> Result<(), Error> {
        let ok = match (self, alg) {
            (PublicKey::Rsa(k), SignatureAlgorithm::RsaPkcs1(h)) => k.verify_pkcs1v15(h, msg, sig),
            (PublicKey::Rsa(k), SignatureAlgorithm::RsaPss { hash, salt_len }) => {
                k.verify_pss(hash, msg, salt_len, sig)
            }
            (PublicKey::Ec { curve, point }, SignatureAlgorithm::Ecdsa(h)) => {
                ec::ecdsa_sig_from_der(*curve, sig)
                    .is_some_and(|raw| ec::ecdsa_verify(*curve, point, h, msg, &raw))
            }
            (PublicKey::Ed25519(k), SignatureAlgorithm::Ed25519) => ed25519_verify(k, msg, sig),
            _ => return Err(Error::InvalidKey),
        };
        if ok { Ok(()) } else { Err(Error::BadSignature) }
    }
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &PublicKey) -> bool {
        self.to_spki() == other.to_spki()
    }
}

impl Eq for PublicKey {}

/// Secret bytes wiped when dropped.
#[derive(Clone)]
struct Secret(Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.0);
    }
}

/// A private key.
#[derive(Clone)]
pub enum PrivateKey {
    /// RSA, with its PKCS #1 `RSAPrivateKey` encoding kept to write it back.
    Rsa(RsaPrivateKey, SecretDer),
    /// ECDSA on a NIST curve.
    Ec(EcPrivateKey),
    /// Ed25519, with its seed.
    Ed25519(Ed25519KeyPair, SecretDer),
}

/// Private key material that is wiped when dropped.
#[derive(Clone)]
pub struct SecretDer(Secret);

impl std::fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrivateKey::Rsa(..) => f.write_str("PrivateKey(RSA)"),
            PrivateKey::Ec(k) => write!(f, "PrivateKey({:?})", k.curve()),
            PrivateKey::Ed25519(..) => f.write_str("PrivateKey(Ed25519)"),
        }
    }
}

fn parse_pkcs1(data: &[u8]) -> Result<PrivateKey, Error> {
    let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
    if der::small_unsigned(r.read(tag::INTEGER)?)? != 0 {
        return Err(Error::UnsupportedAlgorithm);
    }
    let n = der::unsigned(r.read(tag::INTEGER)?)?;
    let e = der::unsigned(r.read(tag::INTEGER)?)?;
    let d = der::unsigned(r.read(tag::INTEGER)?)?;
    for _ in 0..5 {
        der::unsigned(r.read(tag::INTEGER)?)?;
    }
    r.finish()?;
    let key = RsaPrivateKey::new(n, e, d).map_err(|_| Error::InvalidKey)?;
    Ok(PrivateKey::Rsa(key, SecretDer(Secret(data.to_vec()))))
}

fn parse_sec1(data: &[u8], curve: Option<NamedCurve>) -> Result<PrivateKey, Error> {
    let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
    if der::small_unsigned(r.read(tag::INTEGER)?)? != 1 {
        return Err(Error::Der);
    }
    let d = r.read(tag::OCTET_STRING)?;
    let inner = match r.read_optional(tag::context(0))? {
        Some(v) => Some(curve_from_oid(der::oid(der::read_single(v, tag::OID)?.value)?)?),
        None => None,
    };
    let public = r.read_optional(tag::context(1))?;
    r.finish()?;
    let curve = match (curve, inner) {
        (Some(a), Some(b)) if a != b => return Err(Error::InvalidKey),
        (Some(c), _) | (None, Some(c)) => c,
        (None, None) => return Err(Error::Der),
    };
    let key = EcPrivateKey::from_bytes(curve, d).map_err(|_| Error::InvalidKey)?;
    if let Some(p) = public {
        let bits = der::octet_aligned_bits(der::read_single(p, tag::BIT_STRING)?.value)?;
        if bits != key.public_key().as_slice() {
            return Err(Error::InvalidKey);
        }
    }
    Ok(PrivateKey::Ec(key))
}

fn ed25519_from_seed(seed: &[u8]) -> Result<PrivateKey, Error> {
    let seed: [u8; 32] = seed.try_into().map_err(|_| Error::InvalidKey)?;
    Ok(PrivateKey::Ed25519(Ed25519KeyPair::from_seed(&seed), SecretDer(Secret(seed.to_vec()))))
}

/// PBES2 parameters of an encrypted PKCS #8 key.
#[derive(Clone, Copy, Debug)]
pub struct Pbes2Params<'a> {
    /// PBKDF2 salt.
    pub salt: &'a [u8],
    /// PBKDF2 iterations.
    pub iterations: u32,
    /// AES-CBC initialization vector.
    pub iv: [u8; 16],
}

fn derive_pbkdf2(
    prf: HashAlgorithm,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) {
    match prf {
        HashAlgorithm::Sha1 => pbkdf2::<Sha1>(password, salt, iterations, out),
        HashAlgorithm::Sha384 => pbkdf2::<Sha384>(password, salt, iterations, out),
        HashAlgorithm::Sha512 => pbkdf2::<Sha512>(password, salt, iterations, out),
        HashAlgorithm::Sha224 | HashAlgorithm::Sha256 => {
            pbkdf2::<Sha256>(password, salt, iterations, out)
        }
    }
}

fn decrypt_pbes2(alg: &[u8], data: &[u8], password: &[u8]) -> Result<Vec<u8>, Error> {
    let mut a = der::Reader::new(alg);
    if der::oid(a.read(tag::OID)?)? != oid::PBES2 {
        return Err(Error::UnsupportedAlgorithm);
    }
    let params = a.read(tag::SEQUENCE)?;
    a.finish()?;
    let mut p = der::Reader::new(params);
    let kdf = p.read(tag::SEQUENCE)?;
    let enc = p.read(tag::SEQUENCE)?;
    p.finish()?;
    let mut k = der::Reader::new(kdf);
    if der::oid(k.read(tag::OID)?)? != oid::PBKDF2 {
        return Err(Error::UnsupportedAlgorithm);
    }
    let mut kp = der::Reader::new(k.read(tag::SEQUENCE)?);
    k.finish()?;
    let salt = kp.read(tag::OCTET_STRING)?;
    let iterations = der::small_unsigned(kp.read(tag::INTEGER)?)?;
    let key_len = kp.read_optional(tag::INTEGER)?.map(der::small_unsigned).transpose()?;
    let prf = match kp.read_optional(tag::SEQUENCE)? {
        None => HashAlgorithm::Sha1,
        Some(v) => {
            let mut pr = der::Reader::new(v);
            let h = match der::oid(pr.read(tag::OID)?)? {
                oid::HMAC_SHA1 => HashAlgorithm::Sha1,
                oid::HMAC_SHA256 => HashAlgorithm::Sha256,
                oid::HMAC_SHA384 => HashAlgorithm::Sha384,
                oid::HMAC_SHA512 => HashAlgorithm::Sha512,
                _ => return Err(Error::UnsupportedAlgorithm),
            };
            null_or_absent(&mut pr)?;
            h
        }
    };
    kp.finish()?;
    let mut e = der::Reader::new(enc);
    let aes_len = match der::oid(e.read(tag::OID)?)? {
        oid::AES128_CBC => 16,
        oid::AES192_CBC => 24,
        oid::AES256_CBC => 32,
        _ => return Err(Error::UnsupportedAlgorithm),
    };
    let iv: [u8; 16] = e.read(tag::OCTET_STRING)?.try_into().map_err(|_| Error::Der)?;
    e.finish()?;
    if iterations == 0
        || iterations > MAX_PBKDF2_ITERATIONS
        || key_len.is_some_and(|l| l != aes_len as u64)
    {
        return Err(Error::UnsupportedAlgorithm);
    }
    let mut key = Secret(vec![0u8; aes_len]);
    derive_pbkdf2(prf, password, salt, iterations as u32, &mut key.0);
    let aes = Aes::new(&key.0).map_err(|_| Error::Decrypt)?;
    modes::cbc_decrypt(&aes, &iv, data).map_err(|_| Error::Decrypt)
}

impl PrivateKey {
    /// Generates an ECDSA key from at least `scalar_len + 8` random bytes.
    pub fn generate_ec(curve: NamedCurve, random: &[u8]) -> Result<PrivateKey, Error> {
        EcPrivateKey::from_random(curve, random).map(PrivateKey::Ec).map_err(|_| Error::InvalidKey)
    }

    /// An Ed25519 key from its 32-byte seed.
    pub fn ed25519(seed: &[u8; 32]) -> PrivateKey {
        PrivateKey::Ed25519(Ed25519KeyPair::from_seed(seed), SecretDer(Secret(seed.to_vec())))
    }

    /// Parses a DER PKCS #8 `PrivateKeyInfo` or `OneAsymmetricKey`.
    pub fn from_pkcs8(data: &[u8]) -> Result<PrivateKey, Error> {
        let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
        if der::small_unsigned(r.read(tag::INTEGER)?)? > 1 {
            return Err(Error::Der);
        }
        let alg = r.read(tag::SEQUENCE)?;
        let key = r.read(tag::OCTET_STRING)?;
        r.read_optional(tag::context(0))?;
        r.read_optional(tag::context_primitive(1))?;
        r.finish()?;
        let mut a = der::Reader::new(alg);
        match der::oid(a.read(tag::OID)?)? {
            oid::RSA_ENCRYPTION => {
                null_or_absent(&mut a)?;
                parse_pkcs1(key)
            }
            oid::EC_PUBLIC_KEY => {
                let curve = curve_from_oid(der::oid(a.read(tag::OID)?)?)?;
                a.finish()?;
                parse_sec1(key, Some(curve))
            }
            oid::ED25519 => {
                a.finish()?;
                ed25519_from_seed(der::read_single(key, tag::OCTET_STRING)?.value)
            }
            _ => Err(Error::UnsupportedAlgorithm),
        }
    }

    /// Parses a DER SEC 1 `ECPrivateKey` that names its curve.
    pub fn from_sec1(data: &[u8]) -> Result<PrivateKey, Error> {
        parse_sec1(data, None)
    }

    /// Parses a DER PKCS #1 `RSAPrivateKey`.
    pub fn from_pkcs1(data: &[u8]) -> Result<PrivateKey, Error> {
        parse_pkcs1(data)
    }

    /// Decrypts a DER PKCS #8 `EncryptedPrivateKeyInfo` (PBES2 with PBKDF2 and AES-CBC).
    pub fn from_encrypted_pkcs8(data: &[u8], password: &[u8]) -> Result<PrivateKey, Error> {
        let mut r = der::Reader::new(der::read_single(data, tag::SEQUENCE)?.value);
        let alg = r.read(tag::SEQUENCE)?;
        let enc = r.read(tag::OCTET_STRING)?;
        r.finish()?;
        let plain = Secret(decrypt_pbes2(alg, enc, password)?);
        PrivateKey::from_pkcs8(&plain.0).map_err(|_| Error::Decrypt)
    }

    /// The first private key of a PEM file: `PRIVATE KEY`, `EC PRIVATE KEY`, `RSA PRIVATE KEY`
    /// or, with a password, `ENCRYPTED PRIVATE KEY`.
    pub fn from_pem(text: &str, password: Option<&[u8]>) -> Result<PrivateKey, Error> {
        for block in pem::parse_all(text)? {
            if !block.headers.is_empty() {
                return legacy::decrypt(&block, password);
            }
            match (block.label.as_str(), password) {
                (pem::label::PRIVATE_KEY, _) => return PrivateKey::from_pkcs8(&block.der),
                (pem::label::EC_PRIVATE_KEY, _) => return PrivateKey::from_sec1(&block.der),
                (pem::label::RSA_PRIVATE_KEY, _) => return PrivateKey::from_pkcs1(&block.der),
                (pem::label::ENCRYPTED_PRIVATE_KEY, Some(p)) => {
                    return PrivateKey::from_encrypted_pkcs8(&block.der, p);
                }
                (pem::label::ENCRYPTED_PRIVATE_KEY, None) => return Err(Error::Decrypt),
                _ => {}
            }
        }
        Err(Error::Pem)
    }

    /// The DER PKCS #8 `PrivateKeyInfo`.
    pub fn to_pkcs8(&self) -> Vec<u8> {
        let (alg, key) = match self {
            PrivateKey::Rsa(_, der_key) => (
                der::sequence(&[&der::object_id(oid::RSA_ENCRYPTION), &der::null()]),
                der_key.0.0.clone(),
            ),
            PrivateKey::Ec(k) => (
                der::sequence(&[
                    &der::object_id(oid::EC_PUBLIC_KEY),
                    &der::object_id(curve_oid(k.curve())),
                ]),
                der::sequence(&[
                    &der::small_integer(1),
                    &der::octets(&k.to_bytes()),
                    &der::tlv(tag::context(1), &der::bits(&k.public_key())),
                ]),
            ),
            PrivateKey::Ed25519(_, seed) => {
                (der::sequence(&[&der::object_id(oid::ED25519)]), der::octets(&seed.0.0))
            }
        };
        let key = Secret(key);
        der::sequence(&[&der::small_integer(0), &alg, &der::octets(&key.0)])
    }

    /// The DER `EncryptedPrivateKeyInfo` with PBES2, PBKDF2 with HMAC-SHA-256 and AES-256-CBC.
    pub fn to_encrypted_pkcs8(
        &self,
        password: &[u8],
        params: Pbes2Params<'_>,
    ) -> Result<Vec<u8>, Error> {
        if params.iterations == 0 || u64::from(params.iterations) > MAX_PBKDF2_ITERATIONS {
            return Err(Error::UnsupportedAlgorithm);
        }
        let mut key = Secret(vec![0u8; 32]);
        pbkdf2::<Sha256>(password, params.salt, params.iterations, &mut key.0);
        let aes = Aes::new(&key.0).map_err(|_| Error::InvalidKey)?;
        let plain = Secret(self.to_pkcs8());
        let enc = modes::cbc_encrypt(&aes, &params.iv, &plain.0);
        let kdf = der::sequence(&[
            &der::object_id(oid::PBKDF2),
            &der::sequence(&[
                &der::octets(params.salt),
                &der::small_integer(u64::from(params.iterations)),
                &der::sequence(&[&der::object_id(oid::HMAC_SHA256), &der::null()]),
            ]),
        ]);
        let scheme = der::sequence(&[&der::object_id(oid::AES256_CBC), &der::octets(&params.iv)]);
        let alg = der::sequence(&[&der::object_id(oid::PBES2), &der::sequence(&[&kdf, &scheme])]);
        Ok(der::sequence(&[&alg, &der::octets(&enc)]))
    }

    /// The public key.
    pub fn public_key(&self) -> PublicKey {
        match self {
            PrivateKey::Rsa(k, _) => PublicKey::Rsa(k.public_key().clone()),
            PrivateKey::Ec(k) => PublicKey::Ec { curve: k.curve(), point: k.public_key() },
            PrivateKey::Ed25519(k, _) => PublicKey::Ed25519(k.public_key()),
        }
    }

    /// The algorithm `sign` uses: PKCS #1 v1.5 with SHA-256 for RSA, ECDSA with the curve's hash,
    /// Ed25519.
    pub fn default_algorithm(&self) -> SignatureAlgorithm {
        match self {
            PrivateKey::Rsa(..) => SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha256),
            PrivateKey::Ec(k) => SignatureAlgorithm::Ecdsa(k.curve().hash()),
            PrivateKey::Ed25519(..) => SignatureAlgorithm::Ed25519,
        }
    }

    /// Signs `msg` with the default algorithm; ECDSA signatures come out as DER.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, Error> {
        match self {
            PrivateKey::Rsa(k, _) => {
                k.sign_pkcs1v15(HashAlgorithm::Sha256, msg).map_err(|_| Error::InvalidKey)
            }
            PrivateKey::Ec(k) => Ok(ec::ecdsa_sig_to_der(&k.sign(msg))),
            PrivateKey::Ed25519(k, _) => Ok(k.sign(msg).to_vec()),
        }
    }
}

/// OpenSSL's legacy PEM encryption (D-065): `Proc-Type: 4,ENCRYPTED` and `DEK-Info:
/// <cipher>,<IV>`, the key from `EVP_BytesToKey` with MD5, one iteration and the first eight
/// bytes of the IV as salt.
mod legacy {
    use teseladb_crypto::aes::Aes;
    use teseladb_crypto::{des, md5, modes};

    use super::{PrivateKey, Secret};
    use crate::Error;
    use crate::pem::{self, Block};

    const PROC_TYPE_ENCRYPTED: &str = "4,ENCRYPTED";
    const SALT_LEN: usize = 8;

    /// `EVP_BytesToKey(MD5, count 1)`: D1 = MD5(password ‖ salt), Di = MD5(Di−1 ‖ password ‖ salt).
    fn bytes_to_key(password: &[u8], salt: &[u8], len: usize) -> Secret {
        let mut out = Vec::with_capacity(len + 16);
        let mut prev: Vec<u8> = Vec::new();
        while out.len() < len {
            let d = md5(&[prev.as_slice(), password, salt].concat());
            out.extend_from_slice(&d);
            prev = d.to_vec();
        }
        out.truncate(len);
        Secret(out)
    }

    fn hex(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    }

    pub(super) fn decrypt(block: &Block, password: Option<&[u8]>) -> Result<PrivateKey, Error> {
        if block.header("Proc-Type") != Some(PROC_TYPE_ENCRYPTED) {
            return Err(Error::Pem);
        }
        let (cipher, iv_hex) =
            block.header("DEK-Info").and_then(|v| v.split_once(',')).ok_or(Error::Pem)?;
        let iv = hex(iv_hex.trim()).ok_or(Error::Pem)?;
        let password = password.ok_or(Error::Decrypt)?;
        let (key_len, iv_len) = match cipher.trim() {
            "AES-128-CBC" => (16, 16),
            "AES-192-CBC" => (24, 16),
            "AES-256-CBC" => (32, 16),
            "DES-EDE3-CBC" => (24, des::BLOCK_LEN),
            _ => return Err(Error::UnsupportedAlgorithm),
        };
        if iv.len() != iv_len {
            return Err(Error::Pem);
        }
        let key = bytes_to_key(password, &iv[..SALT_LEN], key_len);
        let plain = if iv_len == des::BLOCK_LEN {
            let k = des::TripleDes::new(&key.0).ok().ok_or(Error::Decrypt)?;
            let mut iv8 = [0u8; des::BLOCK_LEN];
            iv8.copy_from_slice(&iv);
            des::cbc_decrypt(&k, &iv8, &block.der)
        } else {
            let aes = Aes::new(&key.0).ok().ok_or(Error::Decrypt)?;
            let mut iv16 = [0u8; 16];
            iv16.copy_from_slice(&iv);
            modes::cbc_decrypt(&aes, &iv16, &block.der)
        };
        let plain = Secret(plain.ok().ok_or(Error::Decrypt)?);
        // A wrong password that happens to leave valid padding still fails to parse.
        let key = match block.label.as_str() {
            pem::label::RSA_PRIVATE_KEY => PrivateKey::from_pkcs1(&plain.0),
            pem::label::EC_PRIVATE_KEY => PrivateKey::from_sec1(&plain.0),
            pem::label::PRIVATE_KEY => PrivateKey::from_pkcs8(&plain.0),
            _ => return Err(Error::Pem),
        };
        key.ok().ok_or(Error::Decrypt)
    }

    #[cfg(test)]
    #[cfg_attr(coverage, coverage(off))]
    mod tests {
        use super::*;

        fn block(headers: &[(&str, &str)], label: &str) -> Block {
            Block {
                label: label.into(),
                der: vec![0; 32],
                headers: headers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            }
        }

        #[test]
        fn malformed_headers() {
            let iv16 = "00112233445566778899AABBCCDDEEFF";
            let pw = Some(&b"pw"[..]);
            let case = |h: &[(&str, &str)], label: &str| decrypt(&block(h, label), pw).err();
            assert_eq!(case(&[("Proc-Type", "4,MIC-ONLY")], "RSA PRIVATE KEY"), Some(Error::Pem));
            assert_eq!(case(&[("Proc-Type", "4,ENCRYPTED")], "RSA PRIVATE KEY"), Some(Error::Pem));
            let dek = |v: &'static str| [("Proc-Type", "4,ENCRYPTED"), ("DEK-Info", v)];
            assert_eq!(case(&dek("AES-128-CBC,0G"), "RSA PRIVATE KEY"), Some(Error::Pem));
            assert_eq!(case(&dek("AES-128-CBC,001"), "RSA PRIVATE KEY"), Some(Error::Pem));
            assert_eq!(case(&dek("AES-128-CBC,0011"), "RSA PRIVATE KEY"), Some(Error::Pem));
            assert_eq!(
                case(&dek("DES-CBC,0011223344556677"), "RSA PRIVATE KEY"),
                Some(Error::UnsupportedAlgorithm)
            );
            let aes = format!("AES-128-CBC,{iv16}");
            let h = [("Proc-Type", "4,ENCRYPTED"), ("DEK-Info", aes.as_str())];
            assert_eq!(decrypt(&block(&h, "RSA PRIVATE KEY"), None).err(), Some(Error::Decrypt));
            // Garbage decrypts to bad padding or to something that is not a key, whatever the label.
            for label in ["RSA PRIVATE KEY", "EC PRIVATE KEY", "PRIVATE KEY"] {
                assert_eq!(case(&h, label), Some(Error::Decrypt));
            }
            // Padding that happens to be valid under an unknown label.
            let key = bytes_to_key(b"pw", &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77], 16);
            let aes_c = Aes::new(&key.0).unwrap();
            let iv: [u8; 16] = hex(iv16).unwrap().try_into().unwrap();
            let mut b = block(&h, "OTHER KEY");
            b.der = modes::cbc_encrypt(&aes_c, &iv, b"x");
            assert_eq!(decrypt(&b, pw).err(), Some(Error::Pem));
            assert_eq!(bytes_to_key(b"", b"", 40).0.len(), 40);
        }
    }
}
