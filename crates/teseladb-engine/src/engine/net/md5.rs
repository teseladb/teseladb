//! MD5 (RFC 1321) for the initial sequence numbers of the native TCP stack.
//!
//! The TCP layer derives each connection's initial sequence number from the MD5 of the
//! connection's addresses and ports and a per-process secret (RFC 6528). With the feature
//! `tls` the digest comes from gnutls; without it, from [`digest`], which produces the same
//! 16 bytes, so the stack's sequence numbers do not depend on the feature. MD5 serves here
//! as the keyed pseudo-random function RFC 6528 asks for, not for integrity.

/// The size of an MD5 digest, in bytes.
pub(crate) const DIGEST_SIZE: usize = 16;

/// The size of the blocks MD5 processes, in bytes.
const BLOCK_SIZE: usize = 64;

/// The per-round left-rotation amounts.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The per-round additive constants, `floor(abs(sin(i + 1)) * 2^32)`.
#[rustfmt::skip]
const SINES: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
    0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
    0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
    0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
    0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
    0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
    0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
    0xeb86_d391,
];

/// The initial chaining values A, B, C and D.
const INITIAL_STATE: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// Mixes one 64-byte block into the chaining state.
fn compress(state: &mut [u32; 4], block: &[u8; BLOCK_SIZE]) {
    let words: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(block.as_chunks::<4>().0[i]));
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let rotated =
            a.wrapping_add(f).wrapping_add(SINES[i]).wrapping_add(words[g]).rotate_left(SHIFTS[i]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(rotated);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

/// The MD5 digest of the concatenation of `parts`.
///
/// Allocation-free: the input is processed in 64-byte blocks through one stack buffer.
pub(crate) fn digest(parts: &[&[u8]]) -> [u8; DIGEST_SIZE] {
    let mut state = INITIAL_STATE;
    let mut block = [0u8; BLOCK_SIZE];
    let mut filled = 0usize;
    let mut total_len = 0u64;
    for part in parts {
        total_len = total_len.wrapping_add(part.len() as u64);
        for &byte in *part {
            block[filled] = byte;
            filled += 1;
            if filled == BLOCK_SIZE {
                compress(&mut state, &block);
                filled = 0;
            }
        }
    }
    // Padding: a single 0x80 byte, zeros up to 56 bytes into a block, then the message
    // length in bits as a little-endian 64-bit integer.
    block[filled] = 0x80;
    block[filled + 1..].fill(0);
    if filled >= BLOCK_SIZE - 8 {
        compress(&mut state, &block);
        block.fill(0);
    }
    block[BLOCK_SIZE - 8..].copy_from_slice(&total_len.wrapping_mul(8).to_le_bytes());
    compress(&mut state, &block);

    let mut out = [0u8; DIGEST_SIZE];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_le_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_the_rfc_1321_test_suite() {
        let cases: [(&str, &str); 7] = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(hex(&digest(&[input.as_bytes()])), expected, "input {input:?}");
        }
    }

    #[test]
    fn splitting_the_input_does_not_change_the_digest() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 7 + 3) as u8).collect();
        let whole = digest(&[&data]);
        for split in [0, 1, 55, 56, 63, 64, 65, 120, 128, 199, 200] {
            assert_eq!(digest(&[&data[..split], &data[split..]]), whole, "split at {split}");
        }
    }

    /// The input of the initial sequence numbers (12 bytes of addresses and ports and a
    /// 64-byte secret) hashed by gnutls and by [`digest`] gives the same bytes.
    #[cfg(feature = "tls")]
    #[test]
    fn matches_gnutls_on_sequence_number_input() {
        use std::ffi::{c_int, c_void};

        const GNUTLS_DIG_MD5: c_int = 2;
        #[link(name = "gnutls")]
        unsafe extern "C" {
            fn gnutls_hash_fast(
                algorithm: c_int,
                text: *const c_void,
                textlen: usize,
                digest: *mut c_void,
            ) -> c_int;
        }

        let input: Vec<u8> = (0..76u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut expected = [0u8; DIGEST_SIZE];
        // SAFETY: `input` is valid for its length and `expected` holds the 16 bytes of an MD5
        // digest.
        let ret = unsafe {
            gnutls_hash_fast(
                GNUTLS_DIG_MD5,
                input.as_ptr().cast(),
                input.len(),
                expected.as_mut_ptr().cast(),
            )
        };
        assert_eq!(ret, 0);
        assert_eq!(digest(&[&input[..12], &input[12..]]), expected);
    }
}
