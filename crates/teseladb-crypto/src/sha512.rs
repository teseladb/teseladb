//! SHA-512 and SHA-384 (FIPS 180-4).

const K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

const H512: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

const H384: [u64; 8] = [
    0xcbbb_9d5d_c105_9ed8,
    0x629a_292a_367c_d507,
    0x9159_015a_3070_dd17,
    0x152f_ecd8_f70e_5939,
    0x6733_2667_ffc0_0b31,
    0x8eb4_4a87_6858_1511,
    0xdb0c_2e0d_64f9_8fa7,
    0x47b5_481d_befa_4fa4,
];

#[expect(
    clippy::many_single_char_names,
    reason = "a to h and w are the names of FIPS 180-4, which the code follows step by step"
)]
fn compress(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 80];
    for (i, chunk) in block.as_chunks::<8>().0.iter().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(chunk);
        w[i] = u64::from_be_bytes(b);
    }
    for t in 16..80 {
        let s0 = w[t - 15].rotate_right(1) ^ w[t - 15].rotate_right(8) ^ (w[t - 15] >> 7);
        let s1 = w[t - 2].rotate_right(19) ^ w[t - 2].rotate_right(61) ^ (w[t - 2] >> 6);
        w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for t in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[t]).wrapping_add(w[t]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *s = s.wrapping_add(v);
    }
}

#[derive(Clone, Debug)]
struct Engine {
    state: [u64; 8],
    buf: [u8; 128],
    buf_len: usize,
    total: u128,
}

impl Engine {
    fn new(h: [u64; 8]) -> Engine {
        Engine { state: h, buf: [0; 128], buf_len: 0, total: 0 }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u128);
        if self.buf_len > 0 {
            let take = (128 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 128 {
                return;
            }
            let block = self.buf;
            compress(&mut self.state, &block);
            self.buf_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<128>();
        for chunk in blocks {
            let mut block = [0u8; 128];
            block.copy_from_slice(chunk);
            compress(&mut self.state, &block);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    fn finalize(mut self) -> [u8; 64] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = [0u8; 256];
        pad[0] = 0x80;
        let pad_len = if self.buf_len < 112 { 112 - self.buf_len } else { 240 - self.buf_len };
        pad[pad_len..pad_len + 16].copy_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad[..pad_len + 16]);
        debug_assert_eq!(self.buf_len, 0, "padding left {total} bytes unaligned");
        let mut out = [0u8; 64];
        for (chunk, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Incremental SHA-512.
#[derive(Clone, Debug)]
pub struct Sha512(Engine);

impl Default for Sha512 {
    fn default() -> Sha512 {
        Sha512::new()
    }
}

impl Sha512 {
    /// Initial state.
    #[must_use]
    pub fn new() -> Sha512 {
        Sha512(Engine::new(H512))
    }
    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    /// Final digest.
    #[must_use]
    pub fn finalize(self) -> [u8; 64] {
        self.0.finalize()
    }
}

/// Incremental SHA-384.
#[derive(Clone, Debug)]
pub struct Sha384(Engine);

impl Default for Sha384 {
    fn default() -> Sha384 {
        Sha384::new()
    }
}

impl Sha384 {
    /// Initial state.
    #[must_use]
    pub fn new() -> Sha384 {
        Sha384(Engine::new(H384))
    }
    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    /// Final digest.
    #[must_use]
    pub fn finalize(self) -> [u8; 48] {
        let full = self.0.finalize();
        let mut out = [0u8; 48];
        out.copy_from_slice(&full[..48]);
        out
    }
}

/// SHA-512 of `data`.
#[must_use]
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(data);
    h.finalize()
}

/// SHA-384 of `data`.
#[must_use]
pub fn sha384(data: &[u8]) -> [u8; 48] {
    let mut h = Sha384::new();
    h.update(data);
    h.finalize()
}
