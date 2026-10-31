//! ChaCha20 and Poly1305 as an AEAD (RFC 8439).

use crate::ct::ct_eq;
use crate::modes::DecryptError;

fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// One 64-byte ChaCha20 block for `key`, block `counter` and a 12-byte `nonce`.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        init[4 + i] = le32(&key[4 * i..]);
    }
    init[12] = counter;
    for i in 0..3 {
        init[13 + i] = le32(&nonce[4 * i..]);
    }
    let mut s = init;
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// XORs `data` with the ChaCha20 key stream starting at block `counter`.
pub fn chacha20_xor(key: &[u8; 32], counter: u32, nonce: &[u8; 12], data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(64).enumerate() {
        let ks = chacha20_block(key, counter.wrapping_add(i as u32), nonce);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
    }
}

/// Poly1305 one-time authenticator with 26-bit limbs.
pub struct Poly1305 {
    r: [u32; 5],
    s: [u32; 4],
    h: [u32; 5],
    buf: [u8; 16],
    buf_len: usize,
}

impl Poly1305 {
    /// Keys the authenticator with a one-time 32-byte key.
    pub fn new(key: &[u8; 32]) -> Poly1305 {
        let r = [
            le32(&key[0..]) & 0x03ff_ffff,
            (le32(&key[3..]) >> 2) & 0x03ff_ff03,
            (le32(&key[6..]) >> 4) & 0x03ff_c0ff,
            (le32(&key[9..]) >> 6) & 0x03f0_3fff,
            (le32(&key[12..]) >> 8) & 0x000f_ffff,
        ];
        let s = [le32(&key[16..]), le32(&key[20..]), le32(&key[24..]), le32(&key[28..])];
        Poly1305 { r, s, h: [0; 5], buf: [0; 16], buf_len: 0 }
    }

    fn block(&mut self, m: &[u8; 16], hibit: u32) {
        let [r0, r1, r2, r3, r4] = self.r.map(u64::from);
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let h = &mut self.h;
        h[0] += le32(&m[0..]) & 0x03ff_ffff;
        h[1] += (le32(&m[3..]) >> 2) & 0x03ff_ffff;
        h[2] += (le32(&m[6..]) >> 4) & 0x03ff_ffff;
        h[3] += (le32(&m[9..]) >> 6) & 0x03ff_ffff;
        h[4] += (le32(&m[12..]) >> 8) | hibit;
        let [h0, h1, h2, h3, h4] = h.map(u64::from);
        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;
        let mut c = d0 >> 26;
        h[0] = (d0 & 0x03ff_ffff) as u32;
        let d1 = d1 + c;
        c = d1 >> 26;
        h[1] = (d1 & 0x03ff_ffff) as u32;
        let d2 = d2 + c;
        c = d2 >> 26;
        h[2] = (d2 & 0x03ff_ffff) as u32;
        let d3 = d3 + c;
        c = d3 >> 26;
        h[3] = (d3 & 0x03ff_ffff) as u32;
        let d4 = d4 + c;
        c = d4 >> 26;
        h[4] = (d4 & 0x03ff_ffff) as u32;
        h[0] += (c * 5) as u32;
        let c = h[0] >> 26;
        h[0] &= 0x03ff_ffff;
        h[1] += c;
    }

    /// Adds data.
    pub fn update(&mut self, mut data: &[u8]) {
        if self.buf_len > 0 {
            let take = (16 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 16 {
                return;
            }
            let b = self.buf;
            self.block(&b, 1 << 24);
            self.buf_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<16>();
        for c in blocks {
            let mut b = [0u8; 16];
            b.copy_from_slice(c);
            self.block(&b, 1 << 24);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Adds zeros up to the next 16-byte boundary (the AEAD's padding).
    fn pad16(&mut self) {
        if self.buf_len > 0 {
            self.update(&[0u8; 16][..16 - self.buf_len]);
        }
    }

    /// Final 16-byte tag.
    pub fn finish(mut self) -> [u8; 16] {
        if self.buf_len > 0 {
            let mut b = [0u8; 16];
            b[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
            b[self.buf_len] = 1;
            self.block(&b, 0);
        }
        let mut h = self.h;
        let mut c = h[1] >> 26;
        h[1] &= 0x03ff_ffff;
        for limb in &mut h[2..5] {
            *limb += c;
            c = *limb >> 26;
            *limb &= 0x03ff_ffff;
        }
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= 0x03ff_ffff;
        h[1] += c;
        // g = h + 5 - 2¹³⁰; keep g when it did not borrow.
        let mut g = [0u32; 5];
        let mut c = 5u32;
        for i in 0..5 {
            g[i] = h[i].wrapping_add(c);
            c = g[i] >> 26;
            g[i] &= 0x03ff_ffff;
        }
        let g4 = g[4].wrapping_add(c).wrapping_sub(1 << 26);
        g[4] = g4 & 0x03ff_ffff;
        let use_g = (g4 >> 31).wrapping_sub(1);
        for i in 0..5 {
            h[i] = (h[i] & !use_g) | (g[i] & use_g);
        }
        let h0 = h[0] | (h[1] << 26);
        let h1 = (h[1] >> 6) | (h[2] << 20);
        let h2 = (h[2] >> 12) | (h[3] << 14);
        let h3 = (h[3] >> 18) | (h[4] << 8);
        let mut out = [0u8; 16];
        let mut f: u64 = 0;
        for (i, hv) in [h0, h1, h2, h3].into_iter().enumerate() {
            f = u64::from(hv) + u64::from(self.s[i]) + (f >> 32);
            out[4 * i..4 * i + 4].copy_from_slice(&(f as u32).to_le_bytes());
        }
        out
    }
}

/// Length of the Poly1305 tag.
pub const POLY1305_TAG_LEN: usize = 16;

/// The ChaCha20-Poly1305 AEAD with a 32-byte key and 12-byte nonces.
#[derive(Clone)]
pub struct ChaCha20Poly1305 {
    key: [u8; 32],
}

impl std::fmt::Debug for ChaCha20Poly1305 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChaCha20Poly1305")
    }
}

impl Drop for ChaCha20Poly1305 {
    fn drop(&mut self) {
        self.key = [0; 32];
        std::hint::black_box(&self.key);
    }
}

impl ChaCha20Poly1305 {
    /// Keys the AEAD.
    pub fn new(key: &[u8; 32]) -> ChaCha20Poly1305 {
        ChaCha20Poly1305 { key: *key }
    }

    fn tag(&self, nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let block0 = chacha20_block(&self.key, 0, nonce);
        let mut otk = [0u8; 32];
        otk.copy_from_slice(&block0[..32]);
        let mut p = Poly1305::new(&otk);
        p.update(aad);
        p.pad16();
        p.update(ciphertext);
        p.pad16();
        p.update(&(aad.len() as u64).to_le_bytes());
        p.update(&(ciphertext.len() as u64).to_le_bytes());
        p.finish()
    }

    /// Encrypts in place and returns the tag.
    pub fn seal_in_place(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> [u8; 16] {
        chacha20_xor(&self.key, 1, nonce, data);
        self.tag(nonce, aad, data)
    }

    /// Checks the tag and decrypts in place; the data is untouched on failure.
    pub fn open_in_place(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8],
    ) -> Result<(), DecryptError> {
        if !ct_eq(&self.tag(nonce, aad, data), tag) {
            return Err(DecryptError);
        }
        chacha20_xor(&self.key, 1, nonce, data);
        Ok(())
    }

    /// Encrypts and appends the tag.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut out = plaintext.to_vec();
        let tag = self.seal_in_place(nonce, aad, &mut out);
        out.extend_from_slice(&tag);
        out
    }

    /// Splits off the tag, checks it and decrypts.
    pub fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        sealed: &[u8],
    ) -> Result<Vec<u8>, DecryptError> {
        if sealed.len() < POLY1305_TAG_LEN {
            return Err(DecryptError);
        }
        let (ct, tag) = sealed.split_at(sealed.len() - POLY1305_TAG_LEN);
        let mut out = ct.to_vec();
        self.open_in_place(nonce, aad, &mut out, tag)?;
        Ok(out)
    }
}
