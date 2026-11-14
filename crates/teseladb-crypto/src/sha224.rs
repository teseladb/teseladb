//! SHA-224 (FIPS 180-4): SHA-256's compression with its own initial value, truncated.

use teseladb_hash::portable::sha256_compress;

const H224: [u32; 8] = [
    0xc105_9ed8,
    0x367c_d507,
    0x3070_dd17,
    0xf70e_5939,
    0xffc0_0b31,
    0x6858_1511,
    0x64f9_8fa7,
    0xbefa_4fa4,
];

/// Incremental SHA-224.
#[derive(Clone, Debug)]
pub struct Sha224 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha224 {
    fn default() -> Sha224 {
        Sha224::new()
    }
}

impl Sha224 {
    /// Initial state.
    #[must_use]
    pub fn new() -> Sha224 {
        Sha224 { state: H224, buf: [0; 64], buf_len: 0, total: 0 }
    }

    /// Adds data.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                return;
            }
            sha256_compress(&mut self.state, &[self.buf]);
            self.buf_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for chunk in blocks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            sha256_compress(&mut self.state, &[block]);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Final digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 28] {
        let bits = self.total.wrapping_mul(8);
        let pad_len = if self.buf_len < 56 { 56 - self.buf_len } else { 120 - self.buf_len };
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        pad[pad_len..pad_len + 8].copy_from_slice(&bits.to_be_bytes());
        self.update(&pad[..pad_len + 8]);
        let mut out = [0u8; 28];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// SHA-224 of `data`.
#[must_use]
pub fn sha224(data: &[u8]) -> [u8; 28] {
    let mut h = Sha224::new();
    h.update(data);
    h.finalize()
}
