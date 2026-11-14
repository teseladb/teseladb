//! AES (FIPS 197) for 128, 192 and 256-bit keys.
//!
//! The S-box is computed, never looked up: the inverse in GF(2⁸) is `x²⁵⁴`, evaluated on the 16
//! bytes of the state at once with byte-lane arithmetic in a `u128`, so no memory access depends on
//! secret data.

const LO7: u128 = 0x7f7f_7f7f_7f7f_7f7f_7f7f_7f7f_7f7f_7f7f;
const ONES: u128 = 0x0101_0101_0101_0101_0101_0101_0101_0101;

/// Doubles each byte in GF(2⁸) with the AES polynomial.
fn xtime(a: u128) -> u128 {
    ((a & LO7) << 1) ^ (((a >> 7) & ONES) * 0x1b)
}

/// Byte-wise product in GF(2⁸).
fn gf_mul(mut a: u128, b: u128) -> u128 {
    let mut r = 0;
    for i in 0..8 {
        let mask = ((b >> i) & ONES) * 0xff;
        r ^= a & mask;
        a = xtime(a);
    }
    r
}

/// Byte-wise inverse (0 maps to 0): `x²⁵⁴ = (x¹²⁷)²`.
fn gf_inv(x: u128) -> u128 {
    let mut a = x;
    for _ in 0..6 {
        a = gf_mul(gf_mul(a, a), x);
    }
    gf_mul(a, a)
}

fn rotl_bytes(b: u128, n: u32) -> u128 {
    let lo = (0xffu128 >> n) * ONES;
    ((b & lo) << n) | ((b >> (8 - n)) & !(lo << n) & (ONES * 0xff))
}

fn sub_bytes(x: u128) -> u128 {
    let b = gf_inv(x);
    b ^ rotl_bytes(b, 1) ^ rotl_bytes(b, 2) ^ rotl_bytes(b, 3) ^ rotl_bytes(b, 4) ^ (ONES * 0x63)
}

fn inv_sub_bytes(x: u128) -> u128 {
    let b = rotl_bytes(x, 1) ^ rotl_bytes(x, 3) ^ rotl_bytes(x, 6) ^ (ONES * 0x05);
    gf_inv(b)
}

fn sub_word(w: u32) -> u32 {
    let column = sub_bytes(u128::from(w)).to_le_bytes();
    u32::from_le_bytes([column[0], column[1], column[2], column[3]])
}

/// Error for a key that is not 16, 24 or 32 bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKeyLength;

/// An expanded AES key.
#[derive(Clone)]
pub struct Aes {
    rounds: usize,
    enc: [[u8; 16]; 15],
    /// `InvMixColumns` of each round key, for the CPU's equivalent inverse cipher.
    imc: [[u8; 16]; 15],
}

impl std::fmt::Debug for Aes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aes({} rounds)", self.rounds)
    }
}

impl Drop for Aes {
    fn drop(&mut self) {
        self.enc = [[0; 16]; 15];
        self.imc = [[0; 16]; 15];
        std::hint::black_box((&self.enc, &self.imc));
    }
}

impl Aes {
    /// Expands a 16, 24 or 32-byte key.
    ///
    /// # Errors
    ///
    /// [`InvalidKeyLength`] for a key of any other length.
    pub fn new(key: &[u8]) -> Result<Aes, InvalidKeyLength> {
        match key.len() {
            16 | 24 | 32 => Ok(Aes::expand(key)),
            _ => Err(InvalidKeyLength),
        }
    }

    /// Expands a 32-byte key (AES-256).
    #[must_use]
    pub fn new_256(key: &[u8; 32]) -> Aes {
        Aes::expand(key)
    }

    /// Expands a key of 16, 24 or 32 bytes.
    fn expand(key: &[u8]) -> Aes {
        let nk = key.len() / 4;
        let rounds = nk + 6;
        let total = 4 * (rounds + 1);
        let mut w = [0u32; 60];
        for (i, c) in key.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let mut rcon = 1u8;
        for i in nk..total {
            let mut t = w[i - 1];
            if i % nk == 0 {
                t = sub_word(t.rotate_right(8)) ^ u32::from(rcon);
                rcon = (rcon << 1) ^ (((rcon >> 7) & 1) * 0x1b);
            } else if nk > 6 && i % nk == 4 {
                t = sub_word(t);
            }
            w[i] = w[i - nk] ^ t;
        }
        let mut enc = [[0u8; 16]; 15];
        for (r, rk) in enc.iter_mut().enumerate().take(rounds + 1) {
            for c in 0..4 {
                rk[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c].to_le_bytes());
            }
        }
        w = [0; 60];
        std::hint::black_box(&w);
        let mut imc = [[0u8; 16]; 15];
        for (m, k) in imc.iter_mut().zip(&enc).take(rounds + 1) {
            *m = inv_mix_columns(u128::from_le_bytes(*k)).to_le_bytes();
        }
        Aes { rounds, enc, imc }
    }

    /// Key length in bytes.
    #[must_use]
    pub fn key_len(&self) -> usize {
        (self.rounds - 6) * 4
    }

    /// Encrypts one block in place.
    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        if crate::arch::aes_encrypt(&self.enc, self.rounds, block) {
            return;
        }
        let mut s = u128::from_le_bytes(*block) ^ u128::from_le_bytes(self.enc[0]);
        for r in 1..=self.rounds {
            s = shift_rows(sub_bytes(s));
            if r != self.rounds {
                s = mix_columns(s);
            }
            s ^= u128::from_le_bytes(self.enc[r]);
        }
        *block = s.to_le_bytes();
    }

    /// Decrypts one block in place.
    pub fn decrypt_block(&self, block: &mut [u8; 16]) {
        if crate::arch::aes_decrypt(&self.enc, &self.imc, self.rounds, block) {
            return;
        }
        let mut s = u128::from_le_bytes(*block) ^ u128::from_le_bytes(self.enc[self.rounds]);
        for r in (0..self.rounds).rev() {
            s = inv_sub_bytes(inv_shift_rows(s)) ^ u128::from_le_bytes(self.enc[r]);
            if r != 0 {
                s = inv_mix_columns(s);
            }
        }
        *block = s.to_le_bytes();
    }
}

/// Byte `r + 4c` of the state is row `r`, column `c`; row `r` moves left by `r` columns.
fn shift_rows(s: u128) -> u128 {
    let b = s.to_le_bytes();
    let mut o = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[r + 4 * c] = b[r + 4 * ((c + r) % 4)];
        }
    }
    u128::from_le_bytes(o)
}

fn inv_shift_rows(s: u128) -> u128 {
    let b = s.to_le_bytes();
    let mut o = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[r + 4 * ((c + r) % 4)] = b[r + 4 * c];
        }
    }
    u128::from_le_bytes(o)
}

/// Rotates each 32-bit column by `n` bytes toward row 0.
fn rot_col(s: u128, n: u32) -> u128 {
    let mut o = 0u128;
    for c in 0..4 {
        let bytes = (s >> (32 * c)).to_le_bytes();
        let col = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).rotate_right(8 * n);
        o |= u128::from(col) << (32 * c);
    }
    o
}

fn mix_columns(s: u128) -> u128 {
    // out_r = 2·a_r + 3·a_{r+1} + a_{r+2} + a_{r+3}
    let a1 = rot_col(s, 1);
    xtime(s ^ a1) ^ a1 ^ rot_col(s, 2) ^ rot_col(s, 3)
}

fn inv_mix_columns(s: u128) -> u128 {
    // Multiplying by {04}x² + {05} first turns InvMixColumns into MixColumns.
    let x4 = xtime(xtime(s));
    let u = s ^ x4 ^ rot_col(x4, 2);
    mix_columns(u)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_computed_sbox_has_the_published_corners() {
        let s = sub_bytes(0x53 | (0x01 << 8)).to_le_bytes();
        assert_eq!((s[0], s[1]), (0xed, 0x7c));
        let i = inv_sub_bytes(0xed | (0x7c << 8)).to_le_bytes();
        assert_eq!((i[0], i[1]), (0x53, 0x01));
        assert_eq!(sub_bytes(0).to_le_bytes()[0], 0x63);
    }
}
