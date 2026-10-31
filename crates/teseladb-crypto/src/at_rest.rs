//! Encryption at rest (013 FR-015, D-030): a [`KeyRing`] holds the master key and makes one
//! data key per file, which the file keeps wrapped with the master key in its header or footer
//! ([`WRAPPED_KEY_LEN`] bytes). A [`FileKey`] seals each block of the file with a random IV or
//! nonce, which goes with the tag into the [`TRAILER_LEN`] bytes every format reserves per block:
//!
//! ```text
//! AES256-CBC (default)  trailer := iv(16) · HMAC-SHA-256(mac key, aad · iv · ciphertext)[..16]
//! AES256-GCM            trailer := nonce(12) · tag(16) · zeros(4)
//! wrapped key           := mode(u8) · zeros(3) · nonce(12) · AES-256-GCM(master, key(32)) · tag(16)
//! ```
//!
//! The CBC mode encrypts with PKCS #7 padding and authenticates with its own key; both of its keys
//! come from the data key with HKDF-SHA-256. The randomness of the data keys and of the IVs and
//! nonces comes from one HMAC-DRBG per thread and ring, without a lock (D-090): seeded on its
//! first use from the ring's [`SeedSource`] (this crate does no I/O) and reseeded from it when it
//! runs out. A data key seals at most [`MAX_BLOCKS_PER_KEY`] blocks; the next block needs a new
//! file.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Sha256;
use crate::aes::Aes;
use crate::ct::ct_eq;
use crate::digest::HashAlgorithm;
use crate::hkdf;
use crate::modes::{AesGcm, GCM_TAG_LEN, cbc_decrypt, cbc_encrypt};
use crate::rand::{HmacDrbg, RESEED_INTERVAL};

/// Bytes each format reserves per block for the IV or nonce and the tag.
pub const TRAILER_LEN: usize = 32;
/// Bytes of a data key wrapped with the master key.
pub const WRAPPED_KEY_LEN: usize = 64;
/// Blocks a data key seals at most (FR-015: rotated before 2³²).
pub const MAX_BLOCKS_PER_KEY: u64 = 1 << 32;

const KEY_LEN: usize = 32;
const IV_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const MAC_LEN: usize = 16;
const SEED_LEN: usize = 48;
const PERSONALIZATION: &[u8] = b"teseladb at rest";
const WRAP_AAD: &[u8] = b"teseladb data key";
const INFO_ENC: &[u8] = b"teseladb at rest enc";
const INFO_MAC: &[u8] = b"teseladb at rest mac";

/// `--encryptionCipherMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CipherMode {
    /// `AES256-CBC`, RefDb's default.
    Cbc,
    /// `AES256-GCM`.
    Gcm,
}

const MODES: [(CipherMode, &str, u8); 2] =
    [(CipherMode::Cbc, "AES256-CBC", 1), (CipherMode::Gcm, "AES256-GCM", 2)];

impl CipherMode {
    /// The mode of a `--encryptionCipherMode` value.
    pub fn parse(text: &str) -> Option<CipherMode> {
        MODES.iter().find(|(_, s, _)| *s == text).map(|(m, _, _)| *m)
    }

    /// The `--encryptionCipherMode` value.
    pub fn as_str(self) -> &'static str {
        MODES.iter().find(|(m, _, _)| *m == self).map_or("", |(_, s, _)| s)
    }

    fn byte(self) -> u8 {
        MODES.iter().find(|(m, _, _)| *m == self).map_or(0, |(_, _, b)| *b)
    }

    fn from_byte(b: u8) -> Option<CipherMode> {
        MODES.iter().find(|(_, _, x)| *x == b).map(|(m, _, _)| *m)
    }
}

/// Why a block or a key could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtRestError {
    /// The wrapped key is damaged or was wrapped with another master key.
    WrongKey,
    /// The block or its trailer changed, or belongs elsewhere.
    Corrupt,
    /// The key sealed its last block, or the randomness needs a new seed.
    Exhausted,
}

impl fmt::Display for AtRestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AtRestError::WrongKey => "the data key does not open with this master key",
            AtRestError::Corrupt => "an encrypted block does not authenticate",
            AtRestError::Exhausted => "the data key sealed its last block",
        })
    }
}

impl std::error::Error for AtRestError {}

/// Fills a buffer with seed bytes: the operating system's randomness on a node, fixed bytes in a
/// test that wants the same keys again.
pub type SeedSource = fn(&mut [u8]) -> std::io::Result<()>;

/// The rings made so far: each ring's generators are told apart by its number.
static RINGS: AtomicU64 = AtomicU64::new(0);
/// The threads that drew randomness so far: two threads seeded with the same bytes still draw
/// different streams.
static THREADS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's number, drawn on its first use.
    static THREAD: Cell<Option<u64>> = const { Cell::new(None) };
    /// This thread's generator of each ring it drew from.
    static GENERATORS: RefCell<Vec<(u64, HmacDrbg)>> = const { RefCell::new(Vec::new()) };
}

fn thread_number() -> u64 {
    THREAD.with(|t| {
        let n = t.get().unwrap_or_else(|| THREADS.fetch_add(1, Ordering::Relaxed));
        t.set(Some(n));
        n
    })
}

/// Where a ring's random bytes come from.
#[derive(Clone, Copy)]
struct Randomness {
    ring: u64,
    seed: SeedSource,
    reseed_interval: u64,
}

impl Randomness {
    /// Fills `out` from this thread's generator of the ring, seeding or reseeding it from the
    /// seed source when needed; [`AtRestError::Exhausted`] when the source fails.
    fn fill(&self, out: &mut [u8]) -> Result<(), AtRestError> {
        GENERATORS.with(|g| {
            let mut generators = g.borrow_mut();
            let at = match generators.iter().position(|(r, _)| *r == self.ring) {
                Some(at) => at,
                None => {
                    let mut seed = [0u8; SEED_LEN];
                    let drbg = (self.seed)(&mut seed).map(|()| {
                        HmacDrbg::new(&seed, &thread_number().to_be_bytes(), PERSONALIZATION)
                            .with_reseed_interval(self.reseed_interval)
                    });
                    seed.fill(0);
                    generators.push((self.ring, drbg.map_err(|_| AtRestError::Exhausted)?));
                    generators.len() - 1
                }
            };
            let drbg = &mut generators[at].1;
            if drbg.generate(out, b"").is_ok() {
                return Ok(());
            }
            let mut seed = [0u8; SEED_LEN];
            let seeded = (self.seed)(&mut seed);
            if seeded.is_ok() {
                drbg.reseed(&seed, b"");
            }
            seed.fill(0);
            seeded.map_err(|_| AtRestError::Exhausted)?;
            drbg.generate(out, b"").map_err(|_| AtRestError::Exhausted)
        })
    }
}

/// A node's [`KeyRing`] shared by its storage; two compare equal when they share the same ring,
/// so that configurations holding one still compare.
#[derive(Clone, Debug)]
pub struct Encryption(pub std::sync::Arc<KeyRing>);

impl PartialEq for Encryption {
    fn eq(&self, other: &Encryption) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Encryption {}

impl Encryption {
    /// The ring.
    pub fn ring(&self) -> &KeyRing {
        &self.0
    }
}

/// The master key and the randomness of new data keys and of their blocks.
pub struct KeyRing {
    master: AesGcm,
    mode: CipherMode,
    random: Randomness,
}

impl fmt::Debug for KeyRing {
    /// Without the keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRing").field("mode", &self.mode).finish_non_exhaustive()
    }
}

impl KeyRing {
    /// The ring of master key `master` making keys of `mode`, its randomness seeded from
    /// `seed`.
    pub fn new(master: &[u8; KEY_LEN], mode: CipherMode, seed: SeedSource) -> KeyRing {
        let master = AesGcm::new(Aes::new_256(master));
        let ring = RINGS.fetch_add(1, Ordering::Relaxed);
        KeyRing {
            master,
            mode,
            random: Randomness { ring, seed, reseed_interval: RESEED_INTERVAL },
        }
    }

    /// The same ring with its generators reseeded after `requests` requests (tests of the
    /// reseed; at most the generator's own interval).
    pub fn with_reseed_interval(mut self, requests: u64) -> KeyRing {
        self.random.reseed_interval = requests.min(RESEED_INTERVAL);
        self
    }

    /// The mode of the keys it makes.
    pub fn mode(&self) -> CipherMode {
        self.mode
    }

    /// A new data key and its wrapped form, for a new file.
    pub fn new_file_key(&self) -> Result<(FileKey, [u8; WRAPPED_KEY_LEN]), AtRestError> {
        let (mut key, mut nonce) = ([0u8; KEY_LEN], [0u8; NONCE_LEN]);
        self.random.fill(&mut key)?;
        self.random.fill(&mut nonce)?;
        let mut wrapped = [0u8; WRAPPED_KEY_LEN];
        wrapped[0] = self.mode.byte();
        wrapped[4..16].copy_from_slice(&nonce);
        let sealed = self.master.seal(&nonce, &aad(self.mode), &key);
        wrapped[16..].copy_from_slice(&sealed);
        let file = FileKey::new(self.mode, &key, self.random);
        key.fill(0);
        std::hint::black_box(&key);
        Ok((file, wrapped))
    }

    /// The data key of `wrapped`, for reading a file (and for its tests, writing more blocks).
    pub fn open_file_key(&self, wrapped: &[u8]) -> Result<FileKey, AtRestError> {
        if wrapped.len() != WRAPPED_KEY_LEN || wrapped[1..4] != [0; 3] {
            return Err(AtRestError::WrongKey);
        }
        let mode = CipherMode::from_byte(wrapped[0]).ok_or(AtRestError::WrongKey)?;
        let mut key = self
            .master
            .open(&wrapped[4..16], &aad(mode), &wrapped[16..])
            .map_err(|_| AtRestError::WrongKey)?;
        let opened: Result<&[u8; KEY_LEN], _> = key.as_slice().try_into();
        let file = opened.map(|k| FileKey::new(mode, k, self.random));
        key.fill(0);
        file.map_err(|_| AtRestError::WrongKey)
    }
}

fn aad(mode: CipherMode) -> Vec<u8> {
    let mut a = WRAP_AAD.to_vec();
    a.push(mode.byte());
    a
}

/// One file's data key: it seals and opens that file's blocks.
pub struct FileKey {
    mode: CipherMode,
    enc: Aes,
    gcm: AesGcm,
    mac: [u8; KEY_LEN],
    random: Randomness,
    sealed: AtomicU64,
}

impl fmt::Debug for FileKey {
    /// Without the keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileKey").field("mode", &self.mode).finish_non_exhaustive()
    }
}

impl Drop for FileKey {
    fn drop(&mut self) {
        self.mac.fill(0);
        std::hint::black_box(&self.mac);
    }
}

impl FileKey {
    fn new(mode: CipherMode, key: &[u8; KEY_LEN], random: Randomness) -> FileKey {
        let prk = hkdf::extract::<Sha256>(b"", key);
        let mut enc: [u8; KEY_LEN] = hkdf::expand_array::<Sha256, KEY_LEN>(prk.as_ref(), INFO_ENC);
        let mac = hkdf::expand_array::<Sha256, KEY_LEN>(prk.as_ref(), INFO_MAC);
        let file = FileKey {
            mode,
            enc: Aes::new_256(&enc),
            gcm: AesGcm::new(Aes::new_256(key)),
            mac,
            random,
            sealed: AtomicU64::new(0),
        };
        enc.fill(0);
        file
    }

    /// Its mode.
    pub fn mode(&self) -> CipherMode {
        self.mode
    }

    /// Whether it sealed its last block.
    pub fn exhausted(&self) -> bool {
        self.sealed.load(Ordering::Relaxed) >= MAX_BLOCKS_PER_KEY
    }

    /// Counts `n` blocks as sealed (tests of the limit).
    pub fn set_sealed_for_tests(&self, n: u64) {
        self.sealed.store(n, Ordering::Relaxed);
    }

    fn tag(&self, aad: &[u8], iv: &[u8], ciphertext: &[u8]) -> [u8; MAC_LEN] {
        let full = HashAlgorithm::Sha256.hmac_parts(&self.mac, &[aad, iv, ciphertext]);
        let mut tag = [0u8; MAC_LEN];
        tag.copy_from_slice(&full.as_ref()[..MAC_LEN]);
        tag
    }

    /// Seals block `plain` bound to `aad` (where it belongs); the ciphertext and the trailer.
    pub fn seal(
        &self,
        aad: &[u8],
        plain: &[u8],
    ) -> Result<(Vec<u8>, [u8; TRAILER_LEN]), AtRestError> {
        if self.sealed.fetch_add(1, Ordering::Relaxed) >= MAX_BLOCKS_PER_KEY {
            self.sealed.store(MAX_BLOCKS_PER_KEY, Ordering::Relaxed);
            return Err(AtRestError::Exhausted);
        }
        let mut trailer = [0u8; TRAILER_LEN];
        match self.mode {
            CipherMode::Cbc => {
                let mut iv = [0u8; IV_LEN];
                self.random.fill(&mut iv)?;
                let ct = cbc_encrypt(&self.enc, &iv, plain);
                trailer[..IV_LEN].copy_from_slice(&iv);
                trailer[IV_LEN..].copy_from_slice(&self.tag(aad, &iv, &ct));
                Ok((ct, trailer))
            }
            CipherMode::Gcm => {
                let mut nonce = [0u8; NONCE_LEN];
                self.random.fill(&mut nonce)?;
                let mut ct = plain.to_vec();
                let tag = self.gcm.seal_in_place(&nonce, aad, &mut ct);
                trailer[..NONCE_LEN].copy_from_slice(&nonce);
                trailer[NONCE_LEN..NONCE_LEN + GCM_TAG_LEN].copy_from_slice(&tag);
                Ok((ct, trailer))
            }
        }
    }

    /// Opens a block sealed with [`FileKey::seal`] and the same `aad`.
    pub fn open(
        &self,
        aad: &[u8],
        ciphertext: &[u8],
        trailer: &[u8; TRAILER_LEN],
    ) -> Result<Vec<u8>, AtRestError> {
        match self.mode {
            CipherMode::Cbc => {
                let iv: &[u8; IV_LEN] =
                    trailer[..IV_LEN].try_into().map_err(|_| AtRestError::Corrupt)?;
                if !ct_eq(&self.tag(aad, iv, ciphertext), &trailer[IV_LEN..]) {
                    return Err(AtRestError::Corrupt);
                }
                cbc_decrypt(&self.enc, iv, ciphertext).map_err(|_| AtRestError::Corrupt)
            }
            CipherMode::Gcm => {
                if trailer[NONCE_LEN + GCM_TAG_LEN..] != [0; 4] {
                    return Err(AtRestError::Corrupt);
                }
                let mut out = ciphertext.to_vec();
                let tag = &trailer[NONCE_LEN..NONCE_LEN + GCM_TAG_LEN];
                self.gcm
                    .open_in_place(&trailer[..NONCE_LEN], aad, &mut out, tag)
                    .map_err(|_| AtRestError::Corrupt)?;
                Ok(out)
            }
        }
    }
}
