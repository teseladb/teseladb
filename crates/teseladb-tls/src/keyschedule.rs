//! The TLS 1.3 key schedule (RFC 8446, 7.1 to 7.5): HKDF-Expand-Label, Derive-Secret, the
//! early, handshake and master secrets, traffic keys, Finished, resumption and key update.

use teseladb_crypto::sha224::Sha224;
use teseladb_crypto::sha512::{Sha384, Sha512};
use teseladb_crypto::{HashAlgorithm, Sha1, Sha256, hkdf};

use crate::suites::{CipherSuite, NONCE_LEN};

/// Bytes wiped when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.0);
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

impl AsRef<[u8]> for Secret {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// HKDF-Extract with the hash.
pub fn extract(hash: HashAlgorithm, salt: &[u8], ikm: &[u8]) -> Secret {
    let out = match hash {
        HashAlgorithm::Sha1 => hkdf::extract::<Sha1>(salt, ikm),
        HashAlgorithm::Sha384 => hkdf::extract::<Sha384>(salt, ikm),
        HashAlgorithm::Sha512 => hkdf::extract::<Sha512>(salt, ikm),
        HashAlgorithm::Sha224 => hkdf::extract::<Sha224>(salt, ikm),
        HashAlgorithm::Sha256 => hkdf::extract::<Sha256>(salt, ikm),
    };
    Secret(out.as_ref().to_vec())
}

/// HKDF-Expand with the hash; `len` never exceeds 255 hash lengths here.
pub fn expand(hash: HashAlgorithm, prk: &[u8], info: &[u8], len: usize) -> Secret {
    let mut out = vec![0u8; len];
    let r = match hash {
        HashAlgorithm::Sha1 => hkdf::expand::<Sha1>(prk, info, &mut out),
        HashAlgorithm::Sha384 => hkdf::expand::<Sha384>(prk, info, &mut out),
        HashAlgorithm::Sha512 => hkdf::expand::<Sha512>(prk, info, &mut out),
        HashAlgorithm::Sha224 => hkdf::expand::<Sha224>(prk, info, &mut out),
        HashAlgorithm::Sha256 => hkdf::expand::<Sha256>(prk, info, &mut out),
    };
    debug_assert!(r.is_ok(), "TLS asks HKDF for at most a few hash lengths");
    Secret(out)
}

/// The `HkdfLabel` structure: length, `"tls13 " ‖ label` and context.
pub fn label_info(label: &[u8], context: &[u8], len: usize) -> Vec<u8> {
    let mut info = Vec::with_capacity(4 + 6 + label.len() + context.len());
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push((6 + label.len()) as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label);
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    info
}

/// HKDF-Expand-Label.
pub fn expand_label(
    hash: HashAlgorithm,
    secret: &[u8],
    label: &[u8],
    context: &[u8],
    len: usize,
) -> Secret {
    expand(hash, secret, &label_info(label, context, len), len)
}

/// Derive-Secret over a transcript hash.
pub fn derive_secret(
    hash: HashAlgorithm,
    secret: &[u8],
    label: &[u8],
    transcript_hash: &[u8],
) -> Secret {
    expand_label(hash, secret, label, transcript_hash, hash.output_len())
}

/// The write key and IV of a traffic secret.
#[derive(Debug, Clone)]
pub struct TrafficKeys {
    /// The AEAD key.
    pub key: Secret,
    /// The per-connection IV, XORed with the sequence number for each record.
    pub iv: [u8; NONCE_LEN],
}

impl TrafficKeys {
    /// RFC 8446, 7.3.
    pub fn derive(suite: CipherSuite, secret: &[u8]) -> TrafficKeys {
        let h = suite.hash();
        let key = expand_label(h, secret, b"key", &[], suite.key_len());
        let iv_bytes = expand_label(h, secret, b"iv", &[], NONCE_LEN);
        let mut iv = [0u8; NONCE_LEN];
        iv.copy_from_slice(&iv_bytes.0);
        TrafficKeys { key, iv }
    }
}

/// `verify_data` of a Finished message (RFC 8446, 4.4.4).
pub fn finished(hash: HashAlgorithm, base_key: &[u8], transcript_hash: &[u8]) -> Vec<u8> {
    let key = expand_label(hash, base_key, b"finished", &[], hash.output_len());
    hash.hmac(&key.0, transcript_hash).as_ref().to_vec()
}

/// The next application traffic secret (RFC 8446, 7.2).
pub fn next_traffic_secret(hash: HashAlgorithm, secret: &[u8]) -> Secret {
    expand_label(hash, secret, b"traffic upd", &[], hash.output_len())
}

/// The PSK of a ticket (RFC 8446, 4.6.1).
pub fn resumption_psk(hash: HashAlgorithm, resumption_master: &[u8], nonce: &[u8]) -> Secret {
    expand_label(hash, resumption_master, b"resumption", nonce, hash.output_len())
}

/// The secrets of one handshake, from the early secret to the master secret.
#[derive(Debug)]
pub struct KeySchedule {
    hash: HashAlgorithm,
    current: Secret,
}

impl KeySchedule {
    /// Starts with the early secret, from the PSK or from zeros.
    pub fn new(hash: HashAlgorithm, psk: Option<&[u8]>) -> KeySchedule {
        let zeros = vec![0u8; hash.output_len()];
        KeySchedule { hash, current: extract(hash, &zeros, psk.unwrap_or(&zeros)) }
    }

    /// The hash.
    pub fn hash(&self) -> HashAlgorithm {
        self.hash
    }

    /// Derive-Secret with the current stage's secret.
    pub fn derive(&self, label: &[u8], transcript_hash: &[u8]) -> Secret {
        derive_secret(self.hash, &self.current.0, label, transcript_hash)
    }

    /// The binder key of the early secret: `res binder` for tickets.
    pub fn resumption_binder_key(&self) -> Secret {
        self.derive(b"res binder", self.hash.hash(&[]).as_ref())
    }

    /// Moves to the next stage: `Derive-Secret(current, "derived", "")` as salt, `ikm` as input
    /// (the (EC)DHE shared secret, then zeros).
    pub fn advance(&mut self, ikm: Option<&[u8]>) {
        let salt = self.derive(b"derived", self.hash.hash(&[]).as_ref());
        let zeros = vec![0u8; self.hash.output_len()];
        self.current = extract(self.hash, &salt.0, ikm.unwrap_or(&zeros));
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn secrets_hide_their_bytes() {
        let s = Secret(vec![1, 2, 3]);
        assert_eq!(format!("{s:?}"), "Secret(3 bytes)");
        assert_eq!(s.as_ref(), &[1, 2, 3]);
        for h in [HashAlgorithm::Sha1, HashAlgorithm::Sha224, HashAlgorithm::Sha512] {
            assert_eq!(extract(h, b"s", b"k").0.len(), h.output_len());
            assert_eq!(expand(h, &[1; 20], b"i", 10).0.len(), 10);
        }
        let ks = KeySchedule::new(HashAlgorithm::Sha256, None);
        assert_eq!(ks.hash(), HashAlgorithm::Sha256);
        assert_eq!(ks.resumption_binder_key().0.len(), 32);
        assert_eq!(next_traffic_secret(HashAlgorithm::Sha256, &[0; 32]).0.len(), 32);
        assert_eq!(resumption_psk(HashAlgorithm::Sha384, &[0; 48], &[0]).0.len(), 48);
    }
}
