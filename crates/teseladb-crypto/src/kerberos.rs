//! Kerberos 5 AES key derivation and encryption for GSSAPI: the `aes128-cts-hmac-sha1-96` and
//! `aes256-cts-hmac-sha1-96` types (RFC 3961 and RFC 3962) and `aes128-cts-hmac-sha256-128` and
//! `aes256-cts-hmac-sha384-192` (RFC 8009).

use crate::aes::{Aes, InvalidKeyLength};
use crate::ct::ct_eq;
use crate::digest::HashAlgorithm;
use crate::modes::{DecryptError, cts_decrypt, cts_encrypt};
use crate::pbkdf2::pbkdf2;
use crate::sha512::Sha384;
use crate::{Sha1, Sha256};

/// A Kerberos AES encryption type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EncType {
    /// `aes128-cts-hmac-sha1-96` (etype 17).
    Aes128CtsHmacSha1,
    /// `aes256-cts-hmac-sha1-96` (etype 18).
    Aes256CtsHmacSha1,
    /// `aes128-cts-hmac-sha256-128` (etype 19).
    Aes128CtsHmacSha256,
    /// `aes256-cts-hmac-sha384-192` (etype 20).
    Aes256CtsHmacSha384,
}

/// Default PBKDF2 iterations of RFC 3962 (4096) and RFC 8009 (32768).
const RFC3962_ITERATIONS: u32 = 4096;
const RFC8009_ITERATIONS: u32 = 32_768;
const KERBEROS: &[u8] = b"kerberos";

/// The three keys of a key usage (RFC 3961, section 5.3): checksum, encryption and integrity.
const USAGE_KC: u8 = 0x99;
const USAGE_KE: u8 = 0xaa;
const USAGE_KI: u8 = 0x55;

impl EncType {
    /// The registered etype number.
    pub fn number(self) -> i32 {
        match self {
            EncType::Aes128CtsHmacSha1 => 17,
            EncType::Aes256CtsHmacSha1 => 18,
            EncType::Aes128CtsHmacSha256 => 19,
            EncType::Aes256CtsHmacSha384 => 20,
        }
    }

    /// Key length in bytes.
    pub fn key_len(self) -> usize {
        match self {
            EncType::Aes128CtsHmacSha1 | EncType::Aes128CtsHmacSha256 => 16,
            EncType::Aes256CtsHmacSha1 | EncType::Aes256CtsHmacSha384 => 32,
        }
    }

    /// Length of the integrity tag appended to a ciphertext.
    pub fn mac_len(self) -> usize {
        match self {
            EncType::Aes128CtsHmacSha1 | EncType::Aes256CtsHmacSha1 => 12,
            EncType::Aes128CtsHmacSha256 => 16,
            EncType::Aes256CtsHmacSha384 => 24,
        }
    }

    fn is_rfc8009(self) -> bool {
        matches!(self, EncType::Aes128CtsHmacSha256 | EncType::Aes256CtsHmacSha384)
    }

    fn name(self) -> &'static [u8] {
        match self {
            EncType::Aes128CtsHmacSha1 => b"aes128-cts-hmac-sha1-96",
            EncType::Aes256CtsHmacSha1 => b"aes256-cts-hmac-sha1-96",
            EncType::Aes128CtsHmacSha256 => b"aes128-cts-hmac-sha256-128",
            EncType::Aes256CtsHmacSha384 => b"aes256-cts-hmac-sha384-192",
        }
    }

    fn hash(self) -> HashAlgorithm {
        match self {
            EncType::Aes128CtsHmacSha1 | EncType::Aes256CtsHmacSha1 => HashAlgorithm::Sha1,
            EncType::Aes128CtsHmacSha256 => HashAlgorithm::Sha256,
            EncType::Aes256CtsHmacSha384 => HashAlgorithm::Sha384,
        }
    }

    /// The default PBKDF2 iteration count of the type.
    pub fn default_iterations(self) -> u32 {
        if self.is_rfc8009() { RFC8009_ITERATIONS } else { RFC3962_ITERATIONS }
    }

    /// string-to-key (RFC 3962, section 4; RFC 8009, section 4).
    pub fn string_to_key(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut tkey = vec![0u8; self.key_len()];
        if self.is_rfc8009() {
            let mut saltp = self.name().to_vec();
            saltp.push(0);
            saltp.extend_from_slice(salt);
            match self.hash() {
                HashAlgorithm::Sha384 => pbkdf2::<Sha384>(password, &saltp, iterations, &mut tkey),
                _ => pbkdf2::<Sha256>(password, &saltp, iterations, &mut tkey),
            }
            kdf_hmac_sha2(self.hash(), &tkey, KERBEROS, self.key_len())
        } else {
            pbkdf2::<Sha1>(password, salt, iterations, &mut tkey);
            // tkey has the key length of the type, which AES accepts.
            derive_rfc3961(&tkey, KERBEROS, self.key_len()).unwrap_or_default()
        }
    }

    /// The key of `usage` and kind (`0x99` checksum, `0xaa` encryption, `0x55` integrity).
    fn usage_key(self, base: &[u8], usage: u32, kind: u8) -> Result<Vec<u8>, InvalidKeyLength> {
        if base.len() != self.key_len() {
            return Err(InvalidKeyLength);
        }
        let mut label = usage.to_be_bytes().to_vec();
        label.push(kind);
        if self.is_rfc8009() {
            let len = match (self, kind) {
                (EncType::Aes256CtsHmacSha384, USAGE_KE) => 32,
                (EncType::Aes256CtsHmacSha384, _) => 24,
                _ => 16,
            };
            Ok(kdf_hmac_sha2(self.hash(), base, &label, len))
        } else {
            derive_rfc3961(base, &label, self.key_len())
        }
    }

    /// Checksum key Kc of a key usage.
    pub fn checksum_key(self, base: &[u8], usage: u32) -> Result<Vec<u8>, InvalidKeyLength> {
        self.usage_key(base, usage, USAGE_KC)
    }

    /// Encryption key Ke of a key usage.
    pub fn encryption_key(self, base: &[u8], usage: u32) -> Result<Vec<u8>, InvalidKeyLength> {
        self.usage_key(base, usage, USAGE_KE)
    }

    /// Integrity key Ki of a key usage.
    pub fn integrity_key(self, base: &[u8], usage: u32) -> Result<Vec<u8>, InvalidKeyLength> {
        self.usage_key(base, usage, USAGE_KI)
    }

    /// Encrypts with a 16-byte random confounder: AES-CTS under Ke, then HMAC under Ki over the
    /// plaintext (RFC 3962) or over the zero IV and the ciphertext (RFC 8009).
    pub fn encrypt(
        self,
        base: &[u8],
        usage: u32,
        confounder: &[u8; 16],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, DecryptError> {
        let ke = self
            .encryption_key(base, usage)
            .and_then(|k| Aes::new(&k))
            .map_err(|_| DecryptError)?;
        let ki = self.integrity_key(base, usage).map_err(|_| DecryptError)?;
        let mut p = confounder.to_vec();
        p.extend_from_slice(plaintext);
        let mut c = cts_encrypt(&ke, &[0; 16], &p)?;
        let mac = self.mac(&ki, &p, &c);
        c.extend_from_slice(&mac);
        Ok(c)
    }

    /// Checks the tag and decrypts; returns the plaintext without the confounder.
    pub fn decrypt(
        self,
        base: &[u8],
        usage: u32,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, DecryptError> {
        let mac_len = self.mac_len();
        if ciphertext.len() < 16 + mac_len {
            return Err(DecryptError);
        }
        let ke = self
            .encryption_key(base, usage)
            .and_then(|k| Aes::new(&k))
            .map_err(|_| DecryptError)?;
        let ki = self.integrity_key(base, usage).map_err(|_| DecryptError)?;
        let (c, tag) = ciphertext.split_at(ciphertext.len() - mac_len);
        if self.is_rfc8009() && !ct_eq(&self.mac(&ki, &[], c), tag) {
            return Err(DecryptError);
        }
        let p = cts_decrypt(&ke, &[0; 16], c)?;
        if !self.is_rfc8009() && !ct_eq(&self.mac(&ki, &p, c), tag) {
            return Err(DecryptError);
        }
        Ok(p[16..].to_vec())
    }

    fn mac(self, ki: &[u8], plaintext: &[u8], ciphertext: &[u8]) -> Vec<u8> {
        let full = if self.is_rfc8009() {
            self.hash().hmac_parts(ki, &[&[0u8; 16], ciphertext])
        } else {
            self.hash().hmac(ki, plaintext)
        };
        full.as_ref()[..self.mac_len()].to_vec()
    }
}

/// n-fold (RFC 3961, section 5.1): stretches or folds `input` to `out_len` bytes.
pub fn n_fold(input: &[u8], out_len: usize) -> Vec<u8> {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let in_len = input.len();
    let lcm = in_len * out_len / gcd(in_len, out_len);
    let in_bits = 8 * in_len;
    let mut acc = vec![0u32; out_len];
    for i in 0..lcm {
        // Byte i of the repeated string: copy k = i / in_len is the input rotated right by 13·k bits.
        let k = i / in_len;
        let rot = (13 * k) % in_bits;
        let mut byte = 0u8;
        for bit in 0..8 {
            let pos = (8 * (i % in_len) + bit + in_bits - rot) % in_bits;
            let b = (input[pos / 8] >> (7 - pos % 8)) & 1;
            byte |= b << (7 - bit);
        }
        acc[i % out_len] += u32::from(byte);
    }
    // Ones' complement addition: propagate carries from the right, wrapping around.
    loop {
        let mut carry = 0u32;
        for x in acc.iter_mut().rev() {
            let v = *x + carry;
            *x = v & 0xff;
            carry = v >> 8;
        }
        if carry == 0 {
            break;
        }
        acc[out_len - 1] += carry;
    }
    acc.into_iter().map(|x| x as u8).collect()
}

/// DK of RFC 3961 for AES: AES-ECB chained over n-fold(constant), truncated to `len` bytes
/// (random-to-key is the identity for AES).
fn derive_rfc3961(key: &[u8], constant: &[u8], len: usize) -> Result<Vec<u8>, InvalidKeyLength> {
    let aes = Aes::new(key)?;
    let mut block = [0u8; 16];
    block.copy_from_slice(&n_fold(constant, 16));
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        aes.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out.truncate(len);
    Ok(out)
}

/// KDF-HMAC-SHA2 of RFC 8009, section 3 (SP 800-108 counter mode, one block).
fn kdf_hmac_sha2(hash: HashAlgorithm, key: &[u8], label: &[u8], len: usize) -> Vec<u8> {
    let bits = (8 * len as u32).to_be_bytes();
    let k = hash.hmac_parts(key, &[&1u32.to_be_bytes(), label, &[0], &bits]);
    k.as_ref()[..len].to_vec()
}
