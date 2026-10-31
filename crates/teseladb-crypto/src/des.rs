//! DES (FIPS 46-3) and TDEA (NIST SP 800-67), only to read private keys that OpenSSL wrote in
//! its legacy encrypted PEM format (D-065). The S-boxes are read by scanning every entry and
//! keeping the wanted one with a mask, so no branch or memory index depends on the key or data.

use crate::modes::DecryptError;

/// Block length in bytes.
pub const BLOCK_LEN: usize = 8;

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
/// The eight S-boxes, each as four rows of sixteen.
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Moves bits: bit `table[i]` (1 is the most significant of `width`) becomes output bit `i + 1`.
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0u64, |acc, &t| (acc << 1) | ((input >> (width - u32::from(t))) & 1))
}

/// The S-box value at `index`, read without a secret-dependent index or branch.
fn sbox(b: &[u8; 64], six: u64) -> u64 {
    // Row from the outer bits, column from the inner four (FIPS 46-3).
    let index = ((six & 0x20) | ((six & 1) << 4) | ((six >> 1) & 0xf)) as u8;
    b.iter().enumerate().fold(0u64, |acc, (j, &v)| {
        let eq = u64::from(j as u8 ^ index);
        let mask = (eq.wrapping_sub(1) >> 63).wrapping_neg();
        acc | (u64::from(v) & mask)
    })
}

fn feistel(r: u64, k: u64) -> u64 {
    let x = permute(r, 32, &E) ^ k;
    let out = S
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, b)| (acc << 4) | sbox(b, (x >> (42 - 6 * i)) & 0x3f));
    permute(out, 32, &P)
}

/// One DES key's sixteen round keys.
#[derive(Clone)]
struct Des {
    keys: [u64; 16],
}

impl Des {
    fn new(key: [u8; 8]) -> Des {
        let cd = permute(u64::from_be_bytes(key), 64, &PC1);
        let (mut c, mut d) = ((cd >> 28) & 0x0fff_ffff, cd & 0x0fff_ffff);
        let mut keys = [0u64; 16];
        for (k, s) in keys.iter_mut().zip(SHIFTS) {
            c = ((c << s) | (c >> (28 - s))) & 0x0fff_ffff;
            d = ((d << s) | (d >> (28 - s))) & 0x0fff_ffff;
            *k = permute((c << 28) | d, 56, &PC2);
        }
        Des { keys }
    }

    fn crypt(&self, block: u64, decrypt: bool) -> u64 {
        let ip = permute(block, 64, &IP);
        let (mut l, mut r) = (ip >> 32, ip & 0xffff_ffff);
        for i in 0..16 {
            let k = if decrypt { self.keys[15 - i] } else { self.keys[i] };
            (l, r) = (r, l ^ feistel(r, k));
        }
        permute((r << 32) | l, 64, &FP)
    }
}

impl Drop for Des {
    fn drop(&mut self) {
        self.keys = [0; 16];
        std::hint::black_box(&self.keys);
    }
}

/// Error for a key that is not 8, 16 or 24 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKeyLength;

/// TDEA with one, two or three keys (keying options 3, 2 and 1 of SP 800-67).
#[derive(Clone)]
pub struct TripleDes {
    k1: Des,
    k2: Des,
    k3: Des,
}

impl std::fmt::Debug for TripleDes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TripleDes")
    }
}

impl TripleDes {
    /// A cipher from 24 bytes (K1 ‖ K2 ‖ K3), 16 (K1 ‖ K2, K3 = K1) or 8 (single DES). Parity
    /// bits are ignored.
    pub fn new(key: &[u8]) -> Result<TripleDes, InvalidKeyLength> {
        let part = |i: usize| -> [u8; 8] {
            let mut k = [0u8; 8];
            k.copy_from_slice(&key[i * 8..i * 8 + 8]);
            k
        };
        let (a, b, c) = match key.len() {
            24 => (part(0), part(1), part(2)),
            16 => (part(0), part(1), part(0)),
            8 => (part(0), part(0), part(0)),
            _ => return Err(InvalidKeyLength),
        };
        Ok(TripleDes { k1: Des::new(a), k2: Des::new(b), k3: Des::new(c) })
    }

    /// Encrypts one block: E(K3, D(K2, E(K1, x))).
    pub fn encrypt_block(&self, block: &mut [u8; BLOCK_LEN]) {
        let x = u64::from_be_bytes(*block);
        *block = self.k3.crypt(self.k2.crypt(self.k1.crypt(x, false), true), false).to_be_bytes();
    }

    /// Decrypts one block: D(K1, E(K2, D(K3, x))).
    pub fn decrypt_block(&self, block: &mut [u8; BLOCK_LEN]) {
        let x = u64::from_be_bytes(*block);
        *block = self.k1.crypt(self.k2.crypt(self.k3.crypt(x, true), false), true).to_be_bytes();
    }
}

fn blocks(data: &[u8]) -> impl Iterator<Item = [u8; BLOCK_LEN]> + '_ {
    data.as_chunks::<BLOCK_LEN>().0.iter().copied()
}

/// CBC encryption of whole blocks, without padding; a trailing partial block is ignored.
pub fn cbc_encrypt_blocks(k: &TripleDes, iv: &[u8; BLOCK_LEN], data: &[u8]) -> Vec<u8> {
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for mut b in blocks(data) {
        b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
        k.encrypt_block(&mut b);
        out.extend_from_slice(&b);
        prev = b;
    }
    out
}

/// CBC decryption of whole blocks, without padding; a trailing partial block is ignored.
pub fn cbc_decrypt_blocks(k: &TripleDes, iv: &[u8; BLOCK_LEN], data: &[u8]) -> Vec<u8> {
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for c in blocks(data) {
        let mut b = c;
        k.decrypt_block(&mut b);
        b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
        out.extend_from_slice(&b);
        prev = c;
    }
    out
}

/// CBC decryption with PKCS #5 padding, as the legacy PEM encryption uses. The padding is
/// checked without branching on its bytes.
pub fn cbc_decrypt(
    k: &TripleDes,
    iv: &[u8; BLOCK_LEN],
    data: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK_LEN) {
        return Err(DecryptError);
    }
    let mut out = cbc_decrypt_blocks(k, iv, data);
    let pad = out[out.len() - 1];
    let n = out.len();
    // Every byte of the last block that falls inside the padding must equal `pad`.
    let mut bad = u8::from(pad == 0) | u8::from(usize::from(pad) > BLOCK_LEN);
    for (i, &b) in out[n - BLOCK_LEN..].iter().enumerate() {
        let inside = u8::from(BLOCK_LEN - i <= usize::from(pad));
        bad |= inside & u8::from(b != pad);
    }
    if bad != 0 {
        return Err(DecryptError);
    }
    out.truncate(n - usize::from(pad));
    Ok(out)
}

/// CBC encryption with PKCS #5 padding.
pub fn cbc_encrypt(k: &TripleDes, iv: &[u8; BLOCK_LEN], data: &[u8]) -> Vec<u8> {
    let pad = BLOCK_LEN - data.len() % BLOCK_LEN;
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat_n(pad as u8, pad));
    cbc_encrypt_blocks(k, iv, &padded)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn keys_and_padding() {
        assert!(TripleDes::new(&[0; 7]).is_err());
        assert!(TripleDes::new(&[1; 16]).is_ok());
        assert_eq!(format!("{:?}", TripleDes::new(&[1; 8]).unwrap()), "TripleDes");
        let k = TripleDes::new(&(0..24).collect::<Vec<u8>>()).unwrap();
        let iv = [9; 8];
        for len in [0, 1, 7, 8, 9, 31] {
            let msg: Vec<u8> = (0..len as u8).collect();
            assert_eq!(cbc_decrypt(&k, &iv, &cbc_encrypt(&k, &iv, &msg)).unwrap(), msg);
        }
        assert_eq!(cbc_decrypt(&k, &iv, &[]), Err(DecryptError));
        assert_eq!(cbc_decrypt(&k, &iv, &[0; 9]), Err(DecryptError));
        // Last bytes 0 (pad 0), 9 (too long) and a wrong byte inside the padding.
        for last in [[0u8; 8], [9; 8], [1, 1, 1, 1, 1, 1, 7, 2]] {
            let ct = cbc_encrypt_blocks(&k, &iv, &last);
            assert_eq!(cbc_decrypt(&k, &iv, &ct), Err(DecryptError));
        }
    }
}
