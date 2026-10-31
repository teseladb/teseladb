//! Implementations using SSE4.2 (CRC32C), SSSE3 (Adler-32) and the SHA extensions (SHA-1 and
//! SHA-256).

use core::arch::x86_64::*;

/// Raw CRC32C with the SSE4.2 `crc32` instruction.
///
/// # Safety
/// The CPU must have SSE4.2.
#[target_feature(enable = "sse4.2")]
pub(super) unsafe fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    let (chunks, rest) = data.as_chunks::<8>();
    let mut c = u64::from(crc);
    for chunk in chunks {
        c = _mm_crc32_u64(c, u64::from_le_bytes(*chunk));
    }
    let mut c = c as u32;
    for &b in rest {
        c = _mm_crc32_u8(c, b);
    }
    c
}

/// Sum of the four 32-bit lanes.
#[target_feature(enable = "ssse3")]
fn hsum(v: __m128i) -> u64 {
    let mut lanes = [0u32; 4];
    // SAFETY: `lanes` has 16 bytes; the store is unaligned.
    unsafe { _mm_storeu_si128(lanes.as_mut_ptr().cast(), v) };
    lanes.iter().map(|&x| u64::from(x)).sum()
}

/// Adler-32 with SSSE3, 16 bytes at a time (same scheme as the NEON version).
///
/// # Safety
/// The CPU must have SSSE3.
#[target_feature(enable = "ssse3")]
pub(super) unsafe fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let mut a = u64::from(adler & 0xffff);
    let mut b = u64::from(adler >> 16);
    let w = _mm_setr_epi8(16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1);
    let ones = _mm_set1_epi16(1);
    let zero = _mm_setzero_si128();
    for chunk in data.chunks(crate::adler32::NMAX) {
        let (blocks, rest) = chunk.as_chunks::<16>();
        let (mut s1, mut acc, mut s2) = (zero, zero, zero);
        for blk in blocks {
            // SAFETY: each block has 16 bytes; the load is unaligned.
            let v = unsafe { _mm_loadu_si128(blk.as_ptr().cast()) };
            acc = _mm_add_epi32(acc, s1);
            // Two sums of 8 bytes in the low bits of lanes 0 and 2.
            s1 = _mm_add_epi32(s1, _mm_sad_epu8(v, zero));
            // Weighted pairs (at most 255·31 per i16), then pairs of pairs into i32.
            s2 = _mm_add_epi32(s2, _mm_madd_epi16(_mm_maddubs_epi16(v, w), ones));
        }
        let n = 16 * blocks.len() as u64;
        b += n * a + 16 * hsum(acc) + hsum(s2);
        a += hsum(s1);
        for &x in rest {
            a += u64::from(x);
            b += a;
        }
        a %= crate::adler32::MOD;
        b %= crate::adler32::MOD;
    }
    ((b << 16) | a) as u32
}

/// Loads the 16 bytes at `at` of a block.
#[target_feature(enable = "sse2")]
fn load(block: &[u8; 64], at: usize) -> __m128i {
    let chunk: &[u8; 16] = block[at..at + 16].try_into().expect("16 bytes");
    // SAFETY: `chunk` has 16 bytes; the load is unaligned.
    unsafe { _mm_loadu_si128(chunk.as_ptr().cast()) }
}

/// One SHA-1 round group of four rounds; the function depends on the group (0..=19).
#[target_feature(enable = "sha,sse2")]
fn sha1_rounds(abcd: __m128i, e: __m128i, group: usize) -> __m128i {
    match group / 5 {
        0 => _mm_sha1rnds4_epu32::<0>(abcd, e),
        1 => _mm_sha1rnds4_epu32::<1>(abcd, e),
        2 => _mm_sha1rnds4_epu32::<2>(abcd, e),
        _ => _mm_sha1rnds4_epu32::<3>(abcd, e),
    }
}

/// SHA-1 compression with `sha1rnds4/sha1nexte/sha1msg1/sha1msg2`.
///
/// # Safety
/// The CPU must have the SHA extensions, SSSE3 and SSE4.1.
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
pub(super) unsafe fn sha1_compress(state: &mut [u32; 5], blocks: &[[u8; 64]]) {
    // Big-endian words with the first one in the highest lane, as `sha1rnds4` wants them.
    let mask = _mm_set_epi64x(0x0001_0203_0405_0607, 0x0809_0a0b_0c0d_0e0f);
    let words: &[u32; 4] = state[..4].try_into().expect("4 words");
    // SAFETY: `words` has 16 bytes; the load is unaligned.
    let mut abcd = _mm_shuffle_epi32::<0x1b>(unsafe { _mm_loadu_si128(words.as_ptr().cast()) });
    let mut e0 = _mm_set_epi32(state[4] as i32, 0, 0, 0);
    for block in blocks {
        let (abcd_saved, e0_saved) = (abcd, e0);
        let mut w = [_mm_setzero_si128(); 4];
        let mut e = e0;
        let mut previous = abcd;
        for group in 0..20 {
            let msg = if group < 4 {
                _mm_shuffle_epi8(load(block, 16 * group), mask)
            } else {
                // W[g] = msg2(msg1(W[g-4], W[g-3]) ^ W[g-2], W[g-1]).
                let x = _mm_sha1msg1_epu32(w[group % 4], w[(group + 1) % 4]);
                _mm_sha1msg2_epu32(_mm_xor_si128(x, w[(group + 2) % 4]), w[(group + 3) % 4])
            };
            w[group % 4] = msg;
            e = if group == 0 { _mm_add_epi32(e, msg) } else { _mm_sha1nexte_epu32(previous, msg) };
            previous = abcd;
            abcd = sha1_rounds(abcd, e, group);
        }
        e0 = _mm_sha1nexte_epu32(previous, e0_saved);
        abcd = _mm_add_epi32(abcd, abcd_saved);
    }
    let mut out = [0u32; 4];
    // SAFETY: `out` has 16 bytes; the store is unaligned.
    unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), _mm_shuffle_epi32::<0x1b>(abcd)) };
    state[..4].copy_from_slice(&out);
    state[4] = _mm_extract_epi32::<3>(e0) as u32;
}

/// SHA-256 compression with `sha256rnds2/sha256msg1/sha256msg2`.
///
/// # Safety
/// The CPU must have the SHA extensions, SSSE3 and SSE4.1.
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
pub(super) unsafe fn sha256_compress(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    // Byte swap of every 32-bit word.
    let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
    let mut words = [_mm_setzero_si128(); 2];
    for (i, half) in state.as_chunks::<4>().0.iter().enumerate() {
        // SAFETY: `half` has 16 bytes; the load is unaligned.
        words[i] = unsafe { _mm_loadu_si128(half.as_ptr().cast()) };
    }
    // `sha256rnds2` works on the halves ABEF and CDGH.
    let tmp = _mm_shuffle_epi32::<0xb1>(words[0]); // CDAB
    let efgh = _mm_shuffle_epi32::<0x1b>(words[1]); // HGFE
    let mut abef = _mm_alignr_epi8::<8>(tmp, efgh);
    let mut cdgh = _mm_blend_epi16::<0xf0>(efgh, tmp);
    for block in blocks {
        let (abef_saved, cdgh_saved) = (abef, cdgh);
        let mut w = [_mm_setzero_si128(); 4];
        for group in 0..16 {
            let msg = if group < 4 {
                _mm_shuffle_epi8(load(block, 16 * group), mask)
            } else {
                // W[g] = msg2(msg1(W[g-4], W[g-3]) + alignr(W[g-1], W[g-2], 4), W[g-1]).
                let x = _mm_sha256msg1_epu32(w[group % 4], w[(group + 1) % 4]);
                let y = _mm_alignr_epi8::<4>(w[(group + 3) % 4], w[(group + 2) % 4]);
                _mm_sha256msg2_epu32(_mm_add_epi32(x, y), w[(group + 3) % 4])
            };
            w[group % 4] = msg;
            let k: &[u32; 4] = crate::sha256::K[4 * group..4 * group + 4].try_into().expect("4");
            // SAFETY: `k` has 16 bytes; the load is unaligned.
            let wk = _mm_add_epi32(msg, unsafe { _mm_loadu_si128(k.as_ptr().cast()) });
            cdgh = _mm_sha256rnds2_epu32(cdgh, abef, wk);
            abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32::<0x0e>(wk));
        }
        abef = _mm_add_epi32(abef, abef_saved);
        cdgh = _mm_add_epi32(cdgh, cdgh_saved);
    }
    let feba = _mm_shuffle_epi32::<0x1b>(abef);
    let dchg = _mm_shuffle_epi32::<0xb1>(cdgh);
    let halves = [_mm_blend_epi16::<0xf0>(feba, dchg), _mm_alignr_epi8::<8>(dchg, feba)];
    for (half, v) in state.as_chunks_mut::<4>().0.iter_mut().zip(halves) {
        let mut out = [0u32; 4];
        // SAFETY: `out` has 16 bytes; the store is unaligned.
        unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), v) };
        half.copy_from_slice(&out);
    }
}
