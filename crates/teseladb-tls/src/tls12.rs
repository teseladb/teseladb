//! What TLS 1.2 (RFC 5246) adds for the clients' port: the ECDHE suites with
//! AES-GCM (RFC 5289) and ChaCha20-Poly1305 (RFC 7905), the PRF and the key block, the extended
//! master secret (RFC 7627), the messages that differ from TLS 1.3 and the signatures of
//! `ServerKeyExchange` and `CertificateVerify`.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::ec::ecdsa_sig_to_der;
use teseladb_x509::{Certificate, PrivateKey, PublicKey, SignatureAlgorithm};

use crate::codec::{Reader, put_u16, put_vec8, put_vec16, put_vec24};
use crate::keyschedule::Secret;
use crate::msgs::frame;
use crate::suites::{Aead, CipherSuite, NamedGroup, SignatureScheme};
use crate::{Alert, Error};

/// Length of the master secret.
pub const MASTER_LEN: usize = 48;
/// Length of `verify_data` in Finished.
pub const VERIFY_DATA_LEN: usize = 12;
/// Length of the explicit nonce of an AES-GCM record (RFC 5288, 3).
pub const EXPLICIT_NONCE_LEN: usize = 8;
/// The last eight bytes of a TLS 1.3 server's random when it negotiates TLS 1.2 (RFC 8446,
/// 4.1.3).
pub const DOWNGRADE_SENTINEL: [u8; 8] = [0x44, 0x4f, 0x57, 0x4e, 0x47, 0x52, 0x44, 0x01];
/// `TLS_EMPTY_RENEGOTIATION_INFO_SCSV` (RFC 5746, 3.3).
pub const RENEGOTIATION_SCSV: u16 = 0x00ff;
/// `TLS_FALLBACK_SCSV` (RFC 7507).
pub const FALLBACK_SCSV: u16 = 0x5600;
/// `named_curve` in `ServerKeyExchange` (RFC 8422, 5.4).
const NAMED_CURVE: u8 = 3;
/// The uncompressed point format (RFC 8422, 5.1.2).
pub const UNCOMPRESSED: u8 = 0;
/// `rsa_sign` and `ecdsa_sign` in a `CertificateRequest` (RFC 5246, 7.4.4; RFC 8422, 5.5).
pub const CERTIFICATE_TYPES: [u8; 2] = [1, 64];

/// TLS 1.2 handshake message types that TLS 1.3 does not have.
pub mod hs {
    /// `hello_request`.
    pub const HELLO_REQUEST: u8 = 0;
    /// `server_key_exchange`.
    pub const SERVER_KEY_EXCHANGE: u8 = 12;
    /// `server_hello_done`.
    pub const SERVER_HELLO_DONE: u8 = 14;
    /// `client_key_exchange`.
    pub const CLIENT_KEY_EXCHANGE: u8 = 16;
    /// `certificate_status` (RFC 6066, 8).
    pub const CERTIFICATE_STATUS: u8 = 22;
}

/// TLS 1.2 extensions that TLS 1.3 does not use.
pub mod ext {
    /// `ec_point_formats` (RFC 8422).
    pub const EC_POINT_FORMATS: u16 = 11;
    /// `extended_master_secret` (RFC 7627).
    pub const EXTENDED_MASTER_SECRET: u16 = 23;
    /// `session_ticket` (RFC 5077): read, never answered.
    pub const SESSION_TICKET: u16 = 35;
    /// `renegotiation_info` (RFC 5746).
    pub const RENEGOTIATION_INFO: u16 = 0xff01;
}

/// The record AEAD of a suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cipher {
    /// AES-128-GCM.
    Aes128Gcm,
    /// AES-256-GCM.
    Aes256Gcm,
    /// ChaCha20-Poly1305.
    ChaCha20Poly1305,
}

/// The kind of certificate key a suite needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Auth {
    /// ECDSA or Ed25519 (RFC 8422).
    Ecdsa,
    /// RSA.
    Rsa,
}

/// A TLS 1.2 cipher suite: ECDHE with an AEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tls12Suite {
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`.
    EcdheEcdsaAes128GcmSha256,
    /// `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`.
    EcdheRsaAes128GcmSha256,
    /// `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`.
    EcdheEcdsaAes256GcmSha384,
    /// `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`.
    EcdheRsaAes256GcmSha384,
    /// `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`.
    EcdheEcdsaChaCha20Poly1305Sha256,
    /// `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`.
    EcdheRsaChaCha20Poly1305Sha256,
}

impl Tls12Suite {
    /// Every suite, in this crate's order of preference.
    pub const ALL: [Tls12Suite; 6] = [
        Tls12Suite::EcdheEcdsaAes128GcmSha256,
        Tls12Suite::EcdheRsaAes128GcmSha256,
        Tls12Suite::EcdheEcdsaAes256GcmSha384,
        Tls12Suite::EcdheRsaAes256GcmSha384,
        Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256,
        Tls12Suite::EcdheRsaChaCha20Poly1305Sha256,
    ];

    /// The code point.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            Tls12Suite::EcdheEcdsaAes128GcmSha256 => 0xc02b,
            Tls12Suite::EcdheRsaAes128GcmSha256 => 0xc02f,
            Tls12Suite::EcdheEcdsaAes256GcmSha384 => 0xc02c,
            Tls12Suite::EcdheRsaAes256GcmSha384 => 0xc030,
            Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256 => 0xcca9,
            Tls12Suite::EcdheRsaChaCha20Poly1305Sha256 => 0xcca8,
        }
    }

    /// The suite of a code point.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Tls12Suite> {
        Tls12Suite::ALL.into_iter().find(|s| s.code() == code)
    }

    /// The record AEAD.
    #[must_use]
    pub fn cipher(self) -> Cipher {
        match self {
            Tls12Suite::EcdheEcdsaAes128GcmSha256 | Tls12Suite::EcdheRsaAes128GcmSha256 => {
                Cipher::Aes128Gcm
            }
            Tls12Suite::EcdheEcdsaAes256GcmSha384 | Tls12Suite::EcdheRsaAes256GcmSha384 => {
                Cipher::Aes256Gcm
            }
            Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256
            | Tls12Suite::EcdheRsaChaCha20Poly1305Sha256 => Cipher::ChaCha20Poly1305,
        }
    }

    /// The certificate key the suite needs.
    #[must_use]
    pub fn auth(self) -> Auth {
        match self {
            Tls12Suite::EcdheEcdsaAes128GcmSha256
            | Tls12Suite::EcdheEcdsaAes256GcmSha384
            | Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256 => Auth::Ecdsa,
            Tls12Suite::EcdheRsaAes128GcmSha256
            | Tls12Suite::EcdheRsaAes256GcmSha384
            | Tls12Suite::EcdheRsaChaCha20Poly1305Sha256 => Auth::Rsa,
        }
    }

    /// The hash of the PRF and of the handshake.
    #[must_use]
    pub fn hash(self) -> HashAlgorithm {
        match self.cipher() {
            Cipher::Aes256Gcm => HashAlgorithm::Sha384,
            Cipher::Aes128Gcm | Cipher::ChaCha20Poly1305 => HashAlgorithm::Sha256,
        }
    }

    /// Whether FIPS 140-3 approves the suite: the AES-GCM ones.
    #[must_use]
    pub fn is_fips_approved(self) -> bool {
        self.cipher() != Cipher::ChaCha20Poly1305
    }

    /// The TLS 1.3 suite with the same AEAD and key length, which builds the record AEAD.
    fn aead_suite(self) -> CipherSuite {
        match self.cipher() {
            Cipher::Aes128Gcm => CipherSuite::Aes128GcmSha256,
            Cipher::Aes256Gcm => CipherSuite::Aes256GcmSha384,
            Cipher::ChaCha20Poly1305 => CipherSuite::ChaCha20Poly1305Sha256,
        }
    }

    /// The AEAD key length.
    #[must_use]
    pub fn key_len(self) -> usize {
        self.aead_suite().key_len()
    }

    /// The fixed IV length: the 4-byte salt of AES-GCM, or the 12-byte IV of ChaCha20-Poly1305.
    #[must_use]
    pub fn fixed_iv_len(self) -> usize {
        match self.cipher() {
            Cipher::ChaCha20Poly1305 => 12,
            Cipher::Aes128Gcm | Cipher::Aes256Gcm => 4,
        }
    }

    /// The AEAD with `key`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] for a key of the wrong length.
    pub fn aead(self, key: &[u8]) -> Result<Aead, Error> {
        self.aead_suite().aead(key)
    }

    /// Whether `key` can sign for the suite.
    #[must_use]
    pub fn fits(self, key: &PrivateKey) -> bool {
        matches!(
            (self.auth(), key),
            (Auth::Rsa, PrivateKey::Rsa(..))
                | (Auth::Ecdsa, PrivateKey::Ec(_) | PrivateKey::Ed25519(..))
        )
    }
}

/// The PRF of RFC 5246, 5: `P_hash(secret, label ‖ seed)` cut to `len` bytes.
#[must_use]
pub fn prf(hash: HashAlgorithm, secret: &[u8], label: &[u8], seed: &[u8], len: usize) -> Secret {
    let mut out = Vec::with_capacity(len + hash.output_len());
    let mut a = hash.hmac_parts(secret, &[label, seed]).as_ref().to_vec();
    while out.len() < len {
        out.extend_from_slice(hash.hmac_parts(secret, &[&a, label, seed]).as_ref());
        a = hash.hmac(secret, &a).as_ref().to_vec();
    }
    out.truncate(len);
    Secret(out)
}

/// The master secret: with the extended master secret, from the hash of the handshake through
/// `ClientKeyExchange` (RFC 7627, 4); without it, from the two randoms (RFC 5246, 8.1).
#[must_use]
pub fn master_secret(
    hash: HashAlgorithm,
    pre_master: &[u8],
    ems_session_hash: Option<&[u8]>,
    client_random: &[u8; 32],
    server_random: &[u8; 32],
) -> Secret {
    if let Some(h) = ems_session_hash {
        prf(hash, pre_master, b"extended master secret", h, MASTER_LEN)
    } else {
        let seed = [client_random.as_slice(), server_random.as_slice()].concat();
        prf(hash, pre_master, b"master secret", &seed, MASTER_LEN)
    }
}

/// One direction's record keys.
#[derive(Debug, Clone)]
pub struct Keys12 {
    /// The AEAD key.
    pub key: Secret,
    /// The fixed IV.
    pub iv: Vec<u8>,
}

/// The key block (RFC 5246, 6.3), split into the client's and the server's write keys.
#[must_use]
pub fn key_block(
    suite: Tls12Suite,
    master: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
) -> (Keys12, Keys12) {
    let (k, iv) = (suite.key_len(), suite.fixed_iv_len());
    let seed = [server_random.as_slice(), client_random.as_slice()].concat();
    let block = prf(suite.hash(), master, b"key expansion", &seed, 2 * (k + iv));
    let b = &block.0;
    let client = Keys12 { key: Secret(b[..k].to_vec()), iv: b[2 * k..2 * k + iv].to_vec() };
    let server = Keys12 { key: Secret(b[k..2 * k].to_vec()), iv: b[2 * k + iv..].to_vec() };
    (client, server)
}

/// `verify_data` of a Finished (RFC 5246, 7.4.9).
#[must_use]
pub fn verify_data(suite: Tls12Suite, master: &[u8], client: bool, transcript: &[u8]) -> Vec<u8> {
    let label: &[u8] = if client { b"client finished" } else { b"server finished" };
    let h = suite.hash().hash(transcript);
    prf(suite.hash(), master, label, h.as_ref(), VERIFY_DATA_LEN).0.clone()
}

/// The signature schemes this crate takes in TLS 1.2, in order of preference: those of TLS 1.3
/// and RSASSA-PKCS1-v1_5.
pub const SCHEMES: [SignatureScheme; 13] = [
    SignatureScheme::EcdsaP256Sha256,
    SignatureScheme::EcdsaP384Sha384,
    SignatureScheme::EcdsaP521Sha512,
    SignatureScheme::Ed25519,
    SignatureScheme::RsaPssRsaeSha256,
    SignatureScheme::RsaPssRsaeSha384,
    SignatureScheme::RsaPssRsaeSha512,
    SignatureScheme::RsaPssPssSha256,
    SignatureScheme::RsaPssPssSha384,
    SignatureScheme::RsaPssPssSha512,
    SignatureScheme::RsaPkcs1Sha256,
    SignatureScheme::RsaPkcs1Sha384,
    SignatureScheme::RsaPkcs1Sha512,
];

/// The X.509 algorithm of a TLS 1.2 scheme: in TLS 1.2 an ECDSA scheme names the hash and not
/// the curve (RFC 8422, 5.1.3).
fn algorithm(scheme: SignatureScheme) -> SignatureAlgorithm {
    let pss =
        |hash: HashAlgorithm| SignatureAlgorithm::RsaPss { hash, salt_len: hash.output_len() };
    match scheme {
        SignatureScheme::EcdsaP256Sha256 => SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha256),
        SignatureScheme::EcdsaP384Sha384 => SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha384),
        SignatureScheme::EcdsaP521Sha512 => SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha512),
        SignatureScheme::Ed25519 => SignatureAlgorithm::Ed25519,
        SignatureScheme::RsaPssRsaeSha256 | SignatureScheme::RsaPssPssSha256 => {
            pss(HashAlgorithm::Sha256)
        }
        SignatureScheme::RsaPssRsaeSha384 | SignatureScheme::RsaPssPssSha384 => {
            pss(HashAlgorithm::Sha384)
        }
        SignatureScheme::RsaPssRsaeSha512 | SignatureScheme::RsaPssPssSha512 => {
            pss(HashAlgorithm::Sha512)
        }
        SignatureScheme::RsaPkcs1Sha256 => SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha256),
        SignatureScheme::RsaPkcs1Sha384 => SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha384),
        SignatureScheme::RsaPkcs1Sha512 => SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha512),
    }
}

/// Whether `key` signs with `scheme`; this crate signs only with RSA-PSS of the `rsae` kind.
fn signs_with(key: &PrivateKey, scheme: SignatureScheme) -> bool {
    match (key, algorithm(scheme)) {
        (PrivateKey::Ec(_), SignatureAlgorithm::Ecdsa(_))
        | (PrivateKey::Ed25519(..), SignatureAlgorithm::Ed25519)
        | (PrivateKey::Rsa(..), SignatureAlgorithm::RsaPkcs1(_)) => true,
        (PrivateKey::Rsa(..), SignatureAlgorithm::RsaPss { .. }) => matches!(
            scheme,
            SignatureScheme::RsaPssRsaeSha256
                | SignatureScheme::RsaPssRsaeSha384
                | SignatureScheme::RsaPssRsaeSha512
        ),
        _ => false,
    }
}

/// The scheme this side signs with: for an ECDSA key the one of its curve's hash if the peer
/// offers it, otherwise the peer's first one that fits the key.
#[must_use]
pub fn choose_scheme(key: &PrivateKey, peer: &[u16]) -> Option<SignatureScheme> {
    let offered: Vec<SignatureScheme> = peer
        .iter()
        .filter_map(|&c| SignatureScheme::from_code(c))
        .filter(|s| SCHEMES.contains(s) && signs_with(key, *s))
        .collect();
    let own = match key {
        PrivateKey::Ec(k) => offered
            .iter()
            .copied()
            .find(|s| algorithm(*s) == SignatureAlgorithm::Ecdsa(k.curve().hash())),
        _ => None,
    };
    own.or_else(|| offered.first().copied())
}

/// Signs `content` with `scheme`; RSA-PSS takes its salt from `salt`.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::InternalError`] when the key cannot sign with the scheme.
pub fn sign(
    key: &PrivateKey,
    scheme: SignatureScheme,
    content: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, Error> {
    let internal = || Error::Local(Alert::InternalError);
    match (key, algorithm(scheme)) {
        (PrivateKey::Ec(k), SignatureAlgorithm::Ecdsa(hash)) => {
            Ok(ecdsa_sig_to_der(&k.sign_digest(hash, hash.hash(content).as_ref())))
        }
        (PrivateKey::Ed25519(k, _), SignatureAlgorithm::Ed25519) => Ok(k.sign(content).to_vec()),
        (PrivateKey::Rsa(k, _), SignatureAlgorithm::RsaPss { hash, salt_len }) => {
            k.sign_pss(hash, content, &salt[..salt_len]).ok().ok_or(internal())
        }
        (PrivateKey::Rsa(k, _), SignatureAlgorithm::RsaPkcs1(hash)) => {
            k.sign_pkcs1v15(hash, content).ok().ok_or(internal())
        }
        _ => Err(internal()),
    }
}

/// Verifies a signature of the peer's leaf over `content`; the scheme must be one this side
/// offered.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::DecryptError`] when the signature does not verify, and with
/// [`Alert::IllegalParameter`] for a scheme the key does not take.
pub fn verify(
    leaf: &Certificate,
    scheme: u16,
    offered: &[SignatureScheme],
    content: &[u8],
    sig: &[u8],
) -> Result<(), Error> {
    let illegal = Error::Local(Alert::IllegalParameter);
    let s = SignatureScheme::from_code(scheme).filter(|s| offered.contains(s)).ok_or(illegal)?;
    let alg = algorithm(s);
    let fits = matches!(
        (&leaf.public_key, alg),
        (PublicKey::Ec { .. }, SignatureAlgorithm::Ecdsa(_))
            | (PublicKey::Ed25519(_), SignatureAlgorithm::Ed25519)
            | (
                PublicKey::Rsa(_),
                SignatureAlgorithm::RsaPkcs1(_) | SignatureAlgorithm::RsaPss { .. }
            )
    );
    if !fits {
        return Err(illegal);
    }
    leaf.public_key.verify(alg, content, sig).map_err(|_| Error::Local(Alert::DecryptError))
}

/// Certificate in TLS 1.2: the chain, leaf first, without context or extensions.
#[must_use]
pub fn certificate(chain: &[Vec<u8>]) -> Vec<u8> {
    let mut list = Vec::new();
    for der in chain {
        put_vec24(&mut list, der);
    }
    let mut b = Vec::new();
    put_vec24(&mut b, &list);
    frame(crate::msgs::hs::CERTIFICATE, &b)
}

/// Reads a TLS 1.2 Certificate body: the DER certificates, leaf first.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::DecodeError`] for bytes that do not follow the structure of the message,
/// and with [`Alert::IllegalParameter`] or [`Alert::UnsupportedExtension`] for a value or an extension it does not
/// allow.
pub fn parse_certificate(body: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut r = Reader::new(body);
    let mut list = Reader::new(r.vec24()?);
    r.finish()?;
    let mut out = Vec::new();
    while !list.is_empty() {
        let der = list.vec24()?;
        if der.is_empty() {
            return Err(Error::Local(Alert::DecodeError));
        }
        out.push(der.to_vec());
    }
    Ok(out)
}

/// The ECDHE parameters of a `ServerKeyExchange`: curve type, group and point (RFC 8422, 5.4).
#[must_use]
pub fn ecdh_params(group: NamedGroup, public: &[u8]) -> Vec<u8> {
    let mut p = vec![NAMED_CURVE];
    put_u16(&mut p, group.code());
    put_vec8(&mut p, public);
    p
}

/// A `ServerKeyExchange` as read: the group, the point, the parameters as sent (they are signed)
/// and the signature.
pub struct ServerKeyExchange {
    /// The group's code point.
    pub group: u16,
    /// The server's point.
    pub public: Vec<u8>,
    /// The encoded parameters.
    pub params: Vec<u8>,
    /// The signature scheme's code point.
    pub scheme: u16,
    /// The signature.
    pub signature: Vec<u8>,
}

impl ServerKeyExchange {
    /// Encodes the framed message.
    #[must_use]
    pub fn encode(params: &[u8], scheme: SignatureScheme, signature: &[u8]) -> Vec<u8> {
        let mut b = params.to_vec();
        put_u16(&mut b, scheme.code());
        put_vec16(&mut b, signature);
        frame(hs::SERVER_KEY_EXCHANGE, &b)
    }

    /// Parses the body; only named curves.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] for bytes that do not follow the structure of the message,
    /// and with [`Alert::IllegalParameter`] or [`Alert::UnsupportedExtension`] for a value or an extension it does not
    /// allow.
    pub fn parse(body: &[u8]) -> Result<ServerKeyExchange, Error> {
        let mut r = Reader::new(body);
        if r.u8()? != NAMED_CURVE {
            return Err(Error::Local(Alert::IllegalParameter));
        }
        let group = r.u16()?;
        let public = r.vec8()?.to_vec();
        let params_len = body.len() - r.rest().len();
        let scheme = r.u16()?;
        let signature = r.vec16()?.to_vec();
        r.finish()?;
        Ok(ServerKeyExchange {
            group,
            public,
            params: body[..params_len].to_vec(),
            scheme,
            signature,
        })
    }
}

/// `CertificateRequest` in TLS 1.2: certificate types, signature schemes and no authority names
/// (the client may then send any certificate, RFC 5246, 7.4.4).
#[must_use]
pub fn certificate_request(schemes: &[SignatureScheme]) -> Vec<u8> {
    let mut b = Vec::new();
    put_vec8(&mut b, &CERTIFICATE_TYPES);
    let codes: Vec<u16> = schemes.iter().map(|s| s.code()).collect();
    b.extend_from_slice(&crate::msgs::u16_list(&codes));
    put_vec16(&mut b, &[]);
    frame(crate::msgs::hs::CERTIFICATE_REQUEST, &b)
}

/// Reads a TLS 1.2 `CertificateRequest` body: the signature schemes the server takes.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::DecodeError`] for bytes that do not follow the structure of the message,
/// and with [`Alert::IllegalParameter`] or [`Alert::UnsupportedExtension`] for a value or an extension it does not
/// allow.
pub fn parse_certificate_request(body: &[u8]) -> Result<Vec<u16>, Error> {
    let mut r = Reader::new(body);
    if r.vec8()?.is_empty() {
        return Err(Error::Local(Alert::DecodeError));
    }
    let list = r.vec16()?;
    let _authorities = r.vec16()?;
    r.finish()?;
    let mut encoded = Vec::new();
    put_vec16(&mut encoded, list);
    crate::msgs::read_u16_list(&encoded)
}

/// `ClientKeyExchange`: the client's point.
#[must_use]
pub fn client_key_exchange(public: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_vec8(&mut b, public);
    frame(hs::CLIENT_KEY_EXCHANGE, &b)
}

/// Reads a `ClientKeyExchange` body.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::DecodeError`] for bytes that do not follow the structure of the message,
/// and with [`Alert::IllegalParameter`] or [`Alert::UnsupportedExtension`] for a value or an extension it does not
/// allow.
pub fn parse_client_key_exchange(body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut r = Reader::new(body);
    let p = r.vec8()?.to_vec();
    r.finish()?;
    if p.is_empty() {
        return Err(Error::Local(Alert::DecodeError));
    }
    Ok(p)
}

/// The data of an empty extension with a one-byte length (`renegotiation_info`, `ec_point_formats`
/// with the uncompressed format).
#[must_use]
pub fn one_byte_list(values: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    put_vec8(&mut v, values);
    v
}
