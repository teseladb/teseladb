//! AES modes: CTR, CBC with PKCS#7 padding, CBC with ciphertext stealing (RFC 3962) and GCM
//! (NIST SP 800-38D).

use crate::aes::Aes;
use crate::ct::ct_eq;

/// Error of an authenticated or padded decryption: the input is not what the key produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecryptError;

/// AES-CTR with a 16-byte initial counter block incremented as a 128-bit big-endian number.
pub fn ctr_xor(aes: &Aes, iv: &[u8; 16], data: &mut [u8]) {
    let mut counter = u128::from_be_bytes(*iv);
    for chunk in data.chunks_mut(16) {
        let mut ks = counter.to_be_bytes();
        aes.encrypt_block(&mut ks);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
        counter = counter.wrapping_add(1);
    }
}

fn xor_into(dst: &mut [u8; 16], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= s;
    }
}

/// CBC over whole blocks in place; returns the last ciphertext block.
fn cbc_raw(aes: &Aes, iv: &[u8; 16], data: &mut [u8]) -> [u8; 16] {
    let mut prev = *iv;
    for chunk in data.as_chunks_mut::<16>().0 {
        xor_into(&mut prev, chunk);
        aes.encrypt_block(&mut prev);
        chunk.copy_from_slice(&prev);
    }
    prev
}

/// AES-CBC encryption with PKCS#7 padding.
pub fn cbc_encrypt(aes: &Aes, iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let pad = 16 - plaintext.len() % 16;
    let mut out = Vec::with_capacity(plaintext.len() + pad);
    out.extend_from_slice(plaintext);
    out.resize(plaintext.len() + pad, pad as u8);
    cbc_raw(aes, iv, &mut out);
    out
}

/// AES-CBC decryption with PKCS#7 padding. The padding check does not branch on its bytes.
pub fn cbc_decrypt(aes: &Aes, iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, DecryptError> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(DecryptError);
    }
    let mut out = Vec::with_capacity(ciphertext.len());
    let mut prev = *iv;
    for chunk in ciphertext.as_chunks::<16>().0 {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        aes.decrypt_block(&mut block);
        xor_into(&mut block, &prev);
        out.extend_from_slice(&block);
        prev.copy_from_slice(chunk);
    }
    let last = &out[out.len() - 16..];
    let pad = last[15];
    let mut bad = u32::from(pad.wrapping_sub(1) >= 16);
    for (i, &b) in last.iter().rev().enumerate() {
        let inside = u32::from((i as u8) < pad);
        bad |= inside & u32::from(b != pad);
    }
    if std::hint::black_box(bad) != 0 {
        return Err(DecryptError);
    }
    let keep = out.len() - usize::from(pad);
    out.resize(keep, 0);
    Ok(out)
}

/// Splits a CTS message of `n > 16` bytes: whole blocks before the last two, and the length of the
/// final partial (or full) block.
fn cts_split(n: usize) -> (usize, usize) {
    let tail = if n.is_multiple_of(16) { 16 } else { n % 16 };
    (n - tail - 16, tail)
}

/// AES-CBC with ciphertext stealing as Kerberos uses it (RFC 3962, section 5): the last two
/// blocks swap places and the output is as long as the input (at least one block).
pub fn cts_encrypt(aes: &Aes, iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, DecryptError> {
    let n = data.len();
    if n < 16 {
        return Err(DecryptError);
    }
    let mut out = data.to_vec();
    if n == 16 {
        cbc_raw(aes, iv, &mut out);
        return Ok(out);
    }
    let (head, tail) = cts_split(n);
    let mut prev = cbc_raw(aes, iv, &mut out[..head]);
    xor_into(&mut prev, &data[head..head + 16]);
    aes.encrypt_block(&mut prev);
    let mut last = [0u8; 16];
    last[..tail].copy_from_slice(&data[head + 16..]);
    xor_into(&mut last, &prev);
    aes.encrypt_block(&mut last);
    out[head..head + 16].copy_from_slice(&last);
    out[head + 16..].copy_from_slice(&prev[..tail]);
    Ok(out)
}

/// Inverse of [`cts_encrypt`].
pub fn cts_decrypt(aes: &Aes, iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, DecryptError> {
    let n = data.len();
    if n < 16 {
        return Err(DecryptError);
    }
    let (head, tail) = if n == 16 { (16, 0) } else { cts_split(n) };
    let mut out = Vec::with_capacity(n);
    let mut prev = *iv;
    for chunk in data[..head].as_chunks::<16>().0 {
        let mut b = [0u8; 16];
        b.copy_from_slice(chunk);
        aes.decrypt_block(&mut b);
        xor_into(&mut b, &prev);
        out.extend_from_slice(&b);
        prev.copy_from_slice(chunk);
    }
    if n == 16 {
        return Ok(out);
    }
    // The first stored block encrypts the last plaintext block padded with the stolen bytes.
    let mut d = [0u8; 16];
    d.copy_from_slice(&data[head..head + 16]);
    aes.decrypt_block(&mut d);
    let mut cn = d;
    cn[..tail].copy_from_slice(&data[head + 16..]);
    let mut pn = d;
    xor_into(&mut pn, &cn);
    let mut p = cn;
    aes.decrypt_block(&mut p);
    xor_into(&mut p, &prev);
    out.extend_from_slice(&p);
    out.extend_from_slice(&pn[..tail]);
    Ok(out)
}

/// Multiplies in GF(2¹²⁸) with GCM's bit order: with the CPU's carry-less multiplication when
/// there is one, otherwise bit by bit without branching on the operands.
fn gf128_mul(x: u128, h: u128) -> u128 {
    // Reversing the bits turns GCM's order into the usual one (bit i is the coefficient of xⁱ).
    let (a, b) = (x.reverse_bits(), h.reverse_bits());
    let (a0, a1, b0, b1) = (a as u64, (a >> 64) as u64, b as u64, (b >> 64) as u64);
    if let (Some(lo), Some(hi), Some(m0), Some(m1)) = (
        crate::arch::clmul64(a0, b0),
        crate::arch::clmul64(a1, b1),
        crate::arch::clmul64(a0, b1),
        crate::arch::clmul64(a1, b0),
    ) {
        let mid = m0 ^ m1;
        return reduce_gf128(lo ^ (mid << 64), hi ^ (mid >> 64)).reverse_bits();
    }
    gf128_mul_portable(x, h)
}

/// `lo + hi·x¹²⁸` modulo x¹²⁸ + x⁷ + x² + x + 1, in the usual bit order.
fn reduce_gf128(lo: u128, hi: u128) -> u128 {
    let over = (hi >> 127) ^ (hi >> 126) ^ (hi >> 121);
    let folded = hi ^ (hi << 1) ^ (hi << 2) ^ (hi << 7);
    lo ^ folded ^ over ^ (over << 1) ^ (over << 2) ^ (over << 7)
}

fn gf128_mul_portable(x: u128, h: u128) -> u128 {
    const R: u128 = 0xe1 << 120;
    let mut z = 0u128;
    let mut v = h;
    for i in (0..128).rev() {
        let bit = (x >> i) & 1;
        z ^= v & bit.wrapping_neg();
        let lsb = v & 1;
        v = (v >> 1) ^ (R & lsb.wrapping_neg());
    }
    z
}

fn ghash_padded(h: u128, mut acc: u128, data: &[u8]) -> u128 {
    for chunk in data.chunks(16) {
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        acc = gf128_mul(acc ^ u128::from_be_bytes(b), h);
    }
    acc
}

/// Length of the GCM tag.
pub const GCM_TAG_LEN: usize = 16;

/// AES-GCM with a 16-byte tag.
#[derive(Clone, Debug)]
pub struct AesGcm {
    aes: Aes,
    h: u128,
}

impl AesGcm {
    /// Builds GCM over an expanded key.
    pub fn new(aes: Aes) -> AesGcm {
        let mut h = [0u8; 16];
        aes.encrypt_block(&mut h);
        AesGcm { aes, h: u128::from_be_bytes(h) }
    }

    fn j0(&self, nonce: &[u8]) -> u128 {
        if nonce.len() == 12 {
            let mut b = [0u8; 16];
            b[..12].copy_from_slice(nonce);
            b[15] = 1;
            return u128::from_be_bytes(b);
        }
        let acc = ghash_padded(self.h, 0, nonce);
        gf128_mul(acc ^ ((nonce.len() as u128) * 8), self.h)
    }

    fn gctr(&self, j: u128, data: &mut [u8]) {
        let mut ctr = j;
        for chunk in data.chunks_mut(16) {
            ctr = (ctr & !0xffff_ffffu128) | u128::from((ctr as u32).wrapping_add(1));
            let mut ks = ctr.to_be_bytes();
            self.aes.encrypt_block(&mut ks);
            for (d, k) in chunk.iter_mut().zip(ks) {
                *d ^= k;
            }
        }
    }

    fn tag(&self, j0: u128, aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let acc = ghash_padded(self.h, 0, aad);
        let acc = ghash_padded(self.h, acc, ciphertext);
        let lens = ((aad.len() as u128 * 8) << 64) | (ciphertext.len() as u128 * 8);
        let s = gf128_mul(acc ^ lens, self.h);
        let mut ek = j0.to_be_bytes();
        self.aes.encrypt_block(&mut ek);
        (s ^ u128::from_be_bytes(ek)).to_be_bytes()
    }

    /// Encrypts `data` in place and returns the tag. `nonce` must be non-empty and never repeat
    /// under one key.
    pub fn seal_in_place(&self, nonce: &[u8], aad: &[u8], data: &mut [u8]) -> [u8; 16] {
        let j0 = self.j0(nonce);
        self.gctr(j0, data);
        self.tag(j0, aad, data)
    }

    /// Checks the 16-byte tag and decrypts in place. The data is left untouched when the tag
    /// does not match.
    pub fn open_in_place(
        &self,
        nonce: &[u8],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8],
    ) -> Result<(), DecryptError> {
        if nonce.is_empty() {
            return Err(DecryptError);
        }
        let j0 = self.j0(nonce);
        let expected = self.tag(j0, aad, data);
        if !ct_eq(&expected, tag) {
            return Err(DecryptError);
        }
        self.gctr(j0, data);
        Ok(())
    }

    /// Encrypts and appends the tag.
    pub fn seal(&self, nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut out = plaintext.to_vec();
        let tag = self.seal_in_place(nonce, aad, &mut out);
        out.extend_from_slice(&tag);
        out
    }

    /// Splits off the tag, checks it and decrypts.
    pub fn open(&self, nonce: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, DecryptError> {
        if sealed.len() < GCM_TAG_LEN {
            return Err(DecryptError);
        }
        let (ct, tag) = sealed.split_at(sealed.len() - GCM_TAG_LEN);
        let mut out = ct.to_vec();
        self.open_in_place(nonce, aad, &mut out, tag)?;
        Ok(out)
    }
}
