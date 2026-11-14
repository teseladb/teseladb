//! Node enrollment (013 FR-005, contract `pki-and-enrollment.md`): the token a running node
//! issues, the node that enrolls with it and the node that accepts it.
//!
//! A token is `tdbe1_` and the Base64url of the SHA-256 fingerprint of the cluster's CA, the
//! token's id and its secret (32 + 16 + 32 bytes). Group 0 keeps the id, the SHA-256 of the
//! secret and the expiry. The new node dials a member over TLS trusting only the CA with that
//! fingerprint, sends the token, its name and a fresh public key, and gets back its certificate,
//! the CA's, and the master key and the CA's key sealed for its key: AES-256-GCM under a key that
//! HKDF-SHA-256 draws from an ephemeral P-256 agreement with the node's key.

use std::fmt;

use teseladb_crypto::ec::{EcPrivateKey, NamedCurve, P256_RANDOM_LEN};
use teseladb_crypto::hkdf::{expand_array, extract};
use teseladb_crypto::modes::AesGcm;
use teseladb_crypto::{Sha256, aes::Aes, sha256};
use teseladb_io::peer::{EnrollAccepted, EnrollRequest};
use teseladb_meta::enroll_tokens::{EnrollToken, SECRET_HASH_LEN, TOKEN_ID_LEN};
use teseladb_x509::pem::{base64_decode_strict as base64_decode, base64_encode};
use teseladb_x509::{Certificate, PrivateKey, PublicKey};

use crate::pki::{self, Pki};

/// The start of a token.
pub const TOKEN_PREFIX: &str = "tdbe1_";
/// Bytes of a CA fingerprint.
pub const FINGERPRINT_LEN: usize = 32;
/// Bytes of a token's secret.
pub const SECRET_LEN: usize = 32;
const TOKEN_LEN: usize = FINGERPRINT_LEN + TOKEN_ID_LEN + SECRET_LEN;
/// Bytes of the master key.
pub const MASTER_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const SEALING_INFO: &[u8] = b"teseladb node enrollment";
/// Random bytes a P-256 key is derived from.
pub const KEY_RANDOM_LEN: usize = P256_RANDOM_LEN;

/// Why an enrollment failed, by the step of the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnrollError {
    /// The token is not one.
    BadToken,
    /// The answer does not open with the node's key, or does not hold what it should.
    BadAnswer,
    /// A key could not be made.
    Key,
}

impl fmt::Display for EnrollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EnrollError::BadToken => "the enrollment token is not valid",
            EnrollError::BadAnswer => "the enrollment answer does not open with this node's key",
            EnrollError::Key => "no key could be made",
        })
    }
}

/// A token.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// SHA-256 of the CA's DER.
    pub fingerprint: [u8; FINGERPRINT_LEN],
    /// Its id.
    pub id: [u8; TOKEN_ID_LEN],
    /// Its secret.
    pub secret: [u8; SECRET_LEN],
}

impl fmt::Debug for Token {
    /// Without the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token")
    }
}

impl Token {
    /// The text a node is started with.
    pub fn to_text(&self) -> String {
        let mut raw = Vec::with_capacity(TOKEN_LEN);
        raw.extend_from_slice(&self.fingerprint);
        raw.extend_from_slice(&self.id);
        raw.extend_from_slice(&self.secret);
        let b64 = base64_encode(&raw).replace('+', "-").replace('/', "_").replace('=', "");
        format!("{TOKEN_PREFIX}{b64}")
    }

    /// Reads a token's text.
    pub fn parse(text: &str) -> Result<Token, EnrollError> {
        let body = text.trim().strip_prefix(TOKEN_PREFIX).ok_or(EnrollError::BadToken)?;
        let mut b64 = body.replace('-', "+").replace('_', "/");
        while b64.len() % 4 != 0 {
            b64.push('=');
        }
        let raw = base64_decode(b64.as_bytes()).map_err(|_| EnrollError::BadToken)?;
        if raw.len() != TOKEN_LEN {
            return Err(EnrollError::BadToken);
        }
        let mut t = Token {
            fingerprint: [0; FINGERPRINT_LEN],
            id: [0; TOKEN_ID_LEN],
            secret: [0; SECRET_LEN],
        };
        t.fingerprint.copy_from_slice(&raw[..FINGERPRINT_LEN]);
        t.id.copy_from_slice(&raw[FINGERPRINT_LEN..FINGERPRINT_LEN + TOKEN_ID_LEN]);
        t.secret.copy_from_slice(&raw[FINGERPRINT_LEN + TOKEN_ID_LEN..]);
        Ok(t)
    }

    /// What Group 0 keeps of it: valid until cluster time `expires_ms`.
    pub fn record(&self, expires_ms: u64) -> EnrollToken {
        EnrollToken { id: self.id, secret_hash: secret_hash(&self.secret), expires_ms, used: false }
    }
}

/// SHA-256 of a token's secret.
pub fn secret_hash(secret: &[u8; SECRET_LEN]) -> [u8; SECRET_HASH_LEN] {
    sha256(secret)
}

/// SHA-256 of a certificate's DER: what a token carries of the CA.
pub fn fingerprint(ca: &Certificate) -> [u8; FINGERPRINT_LEN] {
    sha256(ca.der())
}

fn gcm(secret: &[u8], salt: &[u8]) -> AesGcm {
    let prk = extract::<Sha256>(salt, secret);
    let mut key: [u8; 32] = expand_array::<Sha256, 32>(prk.as_ref(), SEALING_INFO);
    let gcm = AesGcm::new(Aes::new_256(&key));
    key = [0; 32];
    std::hint::black_box(&key);
    gcm
}

/// The master key and the CA's key sealed for the node's EC public key `node`: the ephemeral
/// key's point (length-prefixed), the nonce and the sealed `master ‖ PKCS #8 of the CA key`.
pub fn seal_for(
    node: &PublicKey,
    master: &[u8; MASTER_LEN],
    ca_key: &PrivateKey,
    random: &[u8; KEY_RANDOM_LEN + NONCE_LEN],
) -> Result<Vec<u8>, EnrollError> {
    let PublicKey::Ec { curve: NamedCurve::P256, point } = node else {
        return Err(EnrollError::Key);
    };
    let mut key_random = [0u8; P256_RANDOM_LEN];
    key_random.copy_from_slice(&random[..KEY_RANDOM_LEN]);
    let eph = EcPrivateKey::p256_from_random(&key_random);
    key_random.fill(0);
    std::hint::black_box(&key_random);
    let shared = eph.agree(point).map_err(|_| EnrollError::Key)?;
    let eph_point = eph.public_key();
    let aead = gcm(&shared, &eph_point);
    let nonce = &random[KEY_RANDOM_LEN..];
    let mut plain = master.to_vec();
    plain.extend_from_slice(&ca_key.to_pkcs8());
    let sealed = aead.seal(nonce, &eph_point, &plain);
    plain.fill(0);
    // An uncompressed P-256 point is 65 bytes: its length fits the prefix byte.
    let mut out = vec![eph_point.len() as u8];
    out.extend_from_slice(&eph_point);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Opens what [`seal_for`] sealed for `node_key`: the master key and the CA's key.
pub fn open_sealed(
    node_key: &PrivateKey,
    sealed: &[u8],
) -> Result<([u8; MASTER_LEN], PrivateKey), EnrollError> {
    let PrivateKey::Ec(key) = node_key else { return Err(EnrollError::Key) };
    let (&n, rest) = sealed.split_first().ok_or(EnrollError::BadAnswer)?;
    let n = usize::from(n);
    if rest.len() < n + NONCE_LEN {
        return Err(EnrollError::BadAnswer);
    }
    let (eph_point, rest) = rest.split_at(n);
    let (nonce, text) = rest.split_at(NONCE_LEN);
    let shared = key.agree(eph_point).map_err(|_| EnrollError::BadAnswer)?;
    let aead = gcm(&shared, eph_point);
    let mut plain = aead.open(nonce, eph_point, text).map_err(|_| EnrollError::BadAnswer)?;
    let result = (plain.len() > MASTER_LEN)
        .then(|| {
            let mut master = [0u8; MASTER_LEN];
            master.copy_from_slice(&plain[..MASTER_LEN]);
            PrivateKey::from_pkcs8(&plain[MASTER_LEN..]).ok().map(|k| (master, k))
        })
        .flatten()
        .ok_or(EnrollError::BadAnswer);
    plain.fill(0);
    result
}

/// What a member needs to accept enrollments: the CA, its key and the master key.
#[derive(Clone)]
pub struct Enroller {
    ca: Certificate,
    ca_key: PrivateKey,
    master: [u8; MASTER_LEN],
}

impl fmt::Debug for Enroller {
    /// Without the keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Enroller")
    }
}

impl Drop for Enroller {
    fn drop(&mut self) {
        self.master = [0; MASTER_LEN];
        std::hint::black_box(&self.master);
    }
}

impl Enroller {
    /// The enroller of a node with `pki`.
    pub fn new(pki: &Pki) -> Enroller {
        Enroller { ca: pki.ca.clone(), ca_key: pki.ca_key.clone(), master: pki.master_key.0 }
    }

    /// SHA-256 of the CA's DER.
    pub fn fingerprint(&self) -> [u8; FINGERPRINT_LEN] {
        fingerprint(&self.ca)
    }

    /// The public key of a request, if it is a P-256 key (checked before the token is used).
    pub fn public_key(req: &EnrollRequest) -> Result<PublicKey, String> {
        match PublicKey::from_spki(&req.public_key) {
            Ok(k @ PublicKey::Ec { curve: NamedCurve::P256, .. }) => Ok(k),
            _ => Err("the enrollment request does not carry a P-256 public key".to_owned()),
        }
    }

    /// The answer to `req`, whose token Group 0 already took, at `now` (seconds) with `random`
    /// bytes: the node's certificate for its public key and names, the CA's certificate, and the
    /// keys sealed for the node.
    pub fn accept(
        &self,
        req: &EnrollRequest,
        now: i64,
        random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
    ) -> Result<EnrollAccepted, String> {
        let public = Enroller::public_key(req)?;
        let names = pki::node_names(&req.name);
        let cert = pki::issue_node(&self.ca, &self.ca_key, &public, names, now, random)
            .map_err(crate::error_text)?;
        let mut bytes = [0u8; KEY_RANDOM_LEN + NONCE_LEN];
        random(&mut bytes).map_err(crate::error_text)?;
        let sealed =
            seal_for(&public, &self.master, &self.ca_key, &bytes).map_err(crate::error_text)?;
        Ok(EnrollAccepted { certificate: cert.der().to_vec(), ca: self.ca.der().to_vec(), sealed })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ec(seed: u8) -> PrivateKey {
        PrivateKey::generate_ec(NamedCurve::P256, &[seed; KEY_RANDOM_LEN]).unwrap()
    }

    #[test]
    fn tokens_read_back_and_hide_their_secret() {
        let t = Token { fingerprint: [1; 32], id: [2; 16], secret: [3; 32] };
        let text = t.to_text();
        assert_eq!(Token::parse(&format!(" {text}\n")), Ok(t.clone()));
        assert_eq!(format!("{t:?}"), "Token");
        assert_eq!(t.record(9).secret_hash, sha256(&[3; 32]));
        for bad in ["", "tdbe1_", "nope_AAAA", "tdbe1_!!!!", "tdbe1_AAAA"] {
            assert_eq!(Token::parse(bad), Err(EnrollError::BadToken), "{bad}");
        }
        // A space inside the token is a different token, not the same one with a gap.
        let gap = format!("{} {}", &text[..20], &text[20..]);
        assert_eq!(Token::parse(&gap), Err(EnrollError::BadToken));
        for e in [EnrollError::BadToken, EnrollError::BadAnswer, EnrollError::Key] {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn sealed_keys_open_only_with_the_node_key() {
        let node = ec(1);
        let ca_key = ec(2);
        let random = [7u8; KEY_RANDOM_LEN + NONCE_LEN];
        let sealed = seal_for(&node.public_key(), &[5; MASTER_LEN], &ca_key, &random).unwrap();
        let (master, key) = open_sealed(&node, &sealed).unwrap();
        assert_eq!(master, [5; MASTER_LEN]);
        assert_eq!(key.public_key(), ca_key.public_key());
        // Another key, a changed byte, a cut text, a key of another type.
        assert_eq!(open_sealed(&ec(3), &sealed).err(), Some(EnrollError::BadAnswer));
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(open_sealed(&node, &flipped).err(), Some(EnrollError::BadAnswer));
        assert_eq!(open_sealed(&node, &sealed[..10]).err(), Some(EnrollError::BadAnswer));
        assert_eq!(open_sealed(&node, &[]).err(), Some(EnrollError::BadAnswer));
        let mut bad_point = sealed.clone();
        bad_point[2] ^= 0xff;
        assert_eq!(open_sealed(&node, &bad_point).err(), Some(EnrollError::BadAnswer));
        let ed = PrivateKey::ed25519(&[1; 32]);
        assert_eq!(open_sealed(&ed, &sealed).err(), Some(EnrollError::Key));
        let wrong = seal_for(&ed.public_key(), &[5; MASTER_LEN], &ca_key, &random);
        assert_eq!(wrong, Err(EnrollError::Key));
    }

    #[test]
    fn a_request_needs_a_p256_key() {
        let req = |public_key| EnrollRequest {
            token_id: [0; 16],
            secret: [0; 32],
            name: "a".into(),
            public_key,
        };
        assert!(Enroller::public_key(&req(ec(1).public_key().to_spki())).is_ok());
        let ed = PrivateKey::ed25519(&[1; 32]).public_key().to_spki();
        assert!(Enroller::public_key(&req(ed)).is_err());
        assert!(Enroller::public_key(&req(vec![1, 2])).is_err());
    }
    /// Fuzzing of what an enrolling node reads (013 T010, SC-003): token texts and sealed answers,
    /// mutated, give a value or an error and never panic; a changed sealed answer never opens.
    /// `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with changing seeds.
    #[test]
    fn tokens_and_sealed_answers_survive_mutations() {
        let node = ec(1);
        let random = [7u8; KEY_RANDOM_LEN + NONCE_LEN];
        let sealed = seal_for(&node.public_key(), &[5; MASTER_LEN], &ec(2), &random).unwrap();
        let text = Token { fingerprint: [1; 32], id: [2; 16], secret: [3; 32] }.to_text();
        let secs: Option<u64> =
            std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
        let start = std::time::Instant::now();
        let mut rng = teseladb_types::Rng::new(0x005e_ede1);
        let mut rounds = 0u64;
        while rounds < 2_000
            || secs.is_some_and(|s| start.elapsed() < std::time::Duration::from_secs(s))
        {
            let mut answer = sealed.clone();
            let i = rng.range(0, answer.len() as u64) as usize;
            answer[i] ^= 1 << rng.range(0, 8);
            answer.resize(rng.range(0, sealed.len() as u64 + 2) as usize, 0);
            assert!(open_sealed(&node, &answer).is_err() || answer == sealed);
            let mut t = text.clone().into_bytes();
            let j = rng.range(0, t.len() as u64) as usize;
            t[j] = rng.range(0, 128) as u8;
            let _ = Token::parse(&String::from_utf8_lossy(&t));
            rounds += 1;
        }
    }
}
