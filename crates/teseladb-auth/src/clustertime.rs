//! Signing and checking `$clusterTime` (013 FR-014), as RefDb 9.0.4 does it (measured on a
//! `refrouter` with `--keyFile`, 30-10-2026, `scripts/authz-probe/ct_probe.py`): the signature is
//! HMAC-SHA-1 with a 20-byte key over the cluster time as a little-endian 64-bit number
//! (`seconds << 32 | increment`) with its low 16 bits set, so that one signature covers 65 536
//! increments. A key is named by the cluster time at which it was made and signs the times before
//! its `expiresAt`; there are always two, the current one and the next, each valid 90 days after
//! the one before. A time a client sends that is not past the node's own is taken as is; a later
//! one needs a valid signature.
//!
//! The keys live in Group 0 sealed with the cluster's master key (D-030): AES-256-GCM under a
//! key that HKDF-SHA-256 draws from the master key for this use alone, with the key's id and
//! expiry as associated data, so that a sealed key cannot pass for another.

use teseladb_crypto::aes::Aes;
use teseladb_crypto::ct::ct_eq;
use teseladb_crypto::hkdf::{expand, extract};
use teseladb_crypto::hmac::hmac;
use teseladb_crypto::modes::{AesGcm, GCM_TAG_LEN};
use teseladb_crypto::{Sha1, Sha256};

/// Bytes of a key and of a signature.
pub const KEY_LEN: usize = 20;
/// The low bits of the time set before signing.
pub const RANGE_MASK: u64 = 0xFFFF;
/// Seconds a key signs for: 90 days.
pub const KEY_VALID_SECS: u32 = 90 * 24 * 3600;
/// Keys kept ahead: the current one and the next.
pub const KEYS_AHEAD: usize = 2;
/// Bytes of the nonce of a sealed key.
pub const NONCE_LEN: usize = 12;
/// Bytes of a sealed key: nonce, encrypted key and tag.
pub const SEALED_LEN: usize = NONCE_LEN + KEY_LEN + GCM_TAG_LEN;
/// HKDF `info` of the sealing key.
const SEALING_INFO: &[u8] = b"teseladb $clusterTime signing keys";

/// A cluster time: seconds and increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    /// Seconds.
    pub secs: u32,
    /// Increment.
    pub inc: u32,
}

impl Time {
    /// The time as RefDb's 64-bit number.
    pub fn as_u64(self) -> u64 {
        (u64::from(self.secs) << 32) | u64::from(self.inc)
    }
}

/// A signing key.
#[derive(Clone, PartialEq, Eq)]
pub struct HmacKey {
    /// Its id: the cluster time at which it was made, as a 64-bit number.
    pub id: i64,
    /// The key.
    pub key: [u8; KEY_LEN],
    /// It signs times before this second.
    pub expires_at: u32,
}

impl std::fmt::Debug for HmacKey {
    /// Without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacKey")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The signature of `t` with `key`.
pub fn sign(key: &[u8; KEY_LEN], t: Time) -> [u8; KEY_LEN] {
    let signed = (t.as_u64() | RANGE_MASK).to_le_bytes();
    let out = hmac::<Sha1>(key, &signed);
    let mut sig = [0u8; KEY_LEN];
    sig.copy_from_slice(&out.as_ref()[..KEY_LEN]);
    sig
}

/// The 20-byte hash of a signature a client sent; another length is `UnsupportedFormat`, which
/// RefDb answers when it reads the signature, before it compares the times.
pub fn hash(bytes: &[u8]) -> Result<[u8; KEY_LEN], ClusterTimeError> {
    bytes.try_into().map_err(|_| ClusterTimeError::HashLength(bytes.len()))
}

/// Why a `$clusterTime` a client sent was refused; the wire layer gives each its code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterTimeError {
    /// `UnsupportedFormat` (12): a signature that is not 20 bytes.
    HashLength(usize),
    /// `KeyNotFound` (211): no key with that id signs that time.
    KeyNotFound(Time, i64),
    /// `TimeProofMismatch` (204).
    Mismatch,
}

impl ClusterTimeError {
    /// RefDb's message.
    pub fn message(&self) -> String {
        match self {
            ClusterTimeError::HashLength(n) => format!("Unsupported SHA1Block hash length: {n}"),
            ClusterTimeError::KeyNotFound(t, id) => format!(
                "No keys found for HMAC that is valid for time: {{ ts: Timestamp({}, {}) }} with id: {id}",
                t.secs, t.inc
            ),
            ClusterTimeError::Mismatch => "Proof does not match the cluster time".to_owned(),
        }
    }
}

/// The keys of the cluster, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyRing {
    /// The keys.
    pub keys: Vec<HmacKey>,
}

impl KeyRing {
    /// The latest `expiresAt` of the keys (0 without keys): a new key is valid 90 days past it.
    pub fn last_expiry(&self) -> u32 {
        self.keys.iter().map(|k| k.expires_at).max().unwrap_or(0)
    }

    /// The key that signs `t`: the first that has not expired at its second.
    pub fn signing_key(&self, t: Time) -> Option<&HmacKey> {
        self.keys.iter().filter(|k| t.secs < k.expires_at).min_by_key(|k| k.expires_at)
    }

    /// `t` signed: the hash and the key id, or none without a key for it (a node without keys
    /// signs with zeros and id 0, as RefDb without authentication).
    pub fn signature(&self, t: Time) -> Option<([u8; KEY_LEN], i64)> {
        self.signing_key(t).map(|k| (sign(&k.key, t), k.id))
    }

    /// Checks a time a client sent against the node's `current` one: a time not past it is taken
    /// as is, a later one needs `signature` (hash, read with [`hash`], and key id; `None` when the
    /// client sent none).
    pub fn check(
        &self,
        given: Time,
        current: Time,
        signature: Option<(&[u8; KEY_LEN], i64)>,
    ) -> Result<(), ClusterTimeError> {
        if given <= current {
            return Ok(());
        }
        let (hash, id) = signature.unwrap_or((&[0u8; KEY_LEN], 0));
        let key = self
            .keys
            .iter()
            .find(|k| k.id == id && given.secs < k.expires_at)
            .ok_or(ClusterTimeError::KeyNotFound(given, id))?;
        if ct_eq(&sign(&key.key, given), hash) { Ok(()) } else { Err(ClusterTimeError::Mismatch) }
    }

    /// The keys to make at cluster time `now` so that two are ahead: each named by `now` with its
    /// increment raised by one per key, the first valid 90 days from `now`, each next 90 days
    /// after the one before. `random` fills each key.
    pub fn due(&self, now: Time, random: &mut dyn FnMut(&mut [u8; KEY_LEN])) -> Vec<HmacKey> {
        let live: Vec<&HmacKey> = self.keys.iter().filter(|k| now.secs < k.expires_at).collect();
        let mut last = live.iter().map(|k| k.expires_at).max().unwrap_or(now.secs);
        let mut out = Vec::new();
        for i in live.len()..KEYS_AHEAD {
            let mut key = [0u8; KEY_LEN];
            random(&mut key);
            let inc = now.inc.saturating_add(u32::try_from(i - live.len()).unwrap_or(u32::MAX));
            let id = i64::try_from(Time { secs: now.secs, inc }.as_u64()).unwrap_or(i64::MAX);
            last = last.saturating_add(KEY_VALID_SECS);
            out.push(HmacKey { id, key, expires_at: last });
        }
        out
    }
}

/// A sealed key that does not open: another master key, or a changed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealError;

/// Seals and opens the signing keys with the cluster's master key.
#[derive(Clone)]
pub struct KeySealer {
    gcm: AesGcm,
}

impl std::fmt::Debug for KeySealer {
    /// Without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeySealer")
    }
}

/// What a sealed key is bound to: its id and its expiry.
fn associated(id: i64, expires_at: u32) -> [u8; 12] {
    let mut aad = [0u8; 12];
    aad[..8].copy_from_slice(&id.to_le_bytes());
    aad[8..].copy_from_slice(&expires_at.to_le_bytes());
    aad
}

impl KeySealer {
    /// The sealer of `master`, the cluster's master key (`None` cannot happen: HKDF gives the 32
    /// bytes of an AES-256 key).
    pub fn new(master: &[u8]) -> Option<KeySealer> {
        let prk = extract::<Sha256>(&[], master);
        let mut key = [0u8; 32];
        let gcm = expand::<Sha256>(prk.as_ref(), SEALING_INFO, &mut key)
            .ok()
            .and_then(|()| Aes::new(&key).ok())
            .map(AesGcm::new);
        key = [0; 32];
        std::hint::black_box(&key);
        gcm.map(|gcm| KeySealer { gcm })
    }

    /// `key` sealed with `nonce`, which never repeats under one master key (12 random bytes).
    pub fn seal(&self, key: &HmacKey, nonce: [u8; NONCE_LEN]) -> Vec<u8> {
        let aad = associated(key.id, key.expires_at);
        let mut out = nonce.to_vec();
        out.extend(self.gcm.seal(&nonce, &aad, &key.key));
        out
    }

    /// The key of id `id` and expiry `expires_at` from its sealed text.
    pub fn open(&self, id: i64, expires_at: u32, sealed: &[u8]) -> Result<HmacKey, SealError> {
        if sealed.len() != SEALED_LEN {
            return Err(SealError);
        }
        let (nonce, text) = sealed.split_at(NONCE_LEN);
        let plain =
            self.gcm.open(nonce, &associated(id, expires_at), text).map_err(|_| SealError)?;
        let key: [u8; KEY_LEN] = plain.as_slice().try_into().map_err(|_| SealError)?;
        Ok(HmacKey { id, key, expires_at })
    }
}
