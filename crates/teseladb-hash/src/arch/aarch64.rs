//! Implementations using the ARMv8 cryptographic and CRC extensions.

use core::arch::aarch64::*;

/// Raw CRC32C with the `crc32c*` instructions.
///
/// # Safety
/// The CPU must have the `crc` extension.
#[target_feature(enable = "crc")]
pub(super) unsafe fn crc32c_update(mut crc: u32, data: &[u8]) -> u32 {
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        crc = __crc32cd(crc, u64::from_le_bytes(*c));
    }
    for &b in rest {
        crc = __crc32cb(crc, b);
    }
    crc
}

const SHA1_K: [u32; 4] = [0x5a82_7999, 0x6ed9_eba1, 0x8f1b_bcdc, 0xca62_c1d6];

/// SHA-1 compression with `sha1c/sha1p/sha1m/sha1h/sha1su0/sha1su1`.
///
/// # Safety
/// The CPU must have the ARMv8 `sha2` extension (which includes the SHA-1 instructions).
#[target_feature(enable = "sha2")]
pub(super) unsafe fn sha1_compress(state: &mut [u32; 5], blocks: &[[u8; 64]]) {
    // SAFETY: `state` has 5 u32s; the first 4 are read.
    let mut abcd = unsafe { vld1q_u32(state.as_ptr()) };
    let mut e_state = state[4];
    for block in blocks {
        let abcd_saved = abcd;
        let e_saved = e_state;
        // SAFETY: each block is 64 bytes; four 16-byte chunks are read.
        let mut msg = unsafe {
            [
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr()))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(16)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(32)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(48)))),
            ]
        };
        let mut tmp =
            [vaddq_u32(msg[0], vdupq_n_u32(SHA1_K[0])), vaddq_u32(msg[1], vdupq_n_u32(SHA1_K[0]))];
        let mut e = [e_state, 0];
        // 20 groups of 4 rounds. Group g uses e[g%2] and tmp[g%2], prepares the `e` of group g+1 and
        // the message+constant sum of group g+2, and advances the message expansion.
        for g in 0..20 {
            let cur = g % 2;
            let next = 1 - cur;
            e[next] = vsha1h_u32(vgetq_lane_u32::<0>(abcd));
            abcd = match g / 5 {
                0 => vsha1cq_u32(abcd, e[cur], tmp[cur]),
                2 => vsha1mq_u32(abcd, e[cur], tmp[cur]),
                _ => vsha1pq_u32(abcd, e[cur], tmp[cur]),
            };
            if g + 2 < 20 {
                tmp[cur] = vaddq_u32(msg[(g + 2) % 4], vdupq_n_u32(SHA1_K[(g + 2) / 5]));
            }
            if (1..=16).contains(&g) {
                msg[(g - 1) % 4] = vsha1su1q_u32(msg[(g - 1) % 4], msg[(g + 2) % 4]);
            }
            if g <= 15 {
                msg[g % 4] = vsha1su0q_u32(msg[g % 4], msg[(g + 1) % 4], msg[(g + 2) % 4]);
            }
        }
        e_state = e[0].wrapping_add(e_saved);
        abcd = vaddq_u32(abcd, abcd_saved);
    }
    // SAFETY: the first 4 u32s of `state` are written.
    unsafe { vst1q_u32(state.as_mut_ptr(), abcd) };
    state[4] = e_state;
}

/// SHA-256 compression with `sha256h/sha256h2/sha256su0/sha256su1`.
///
/// # Safety
/// The CPU must have the ARMv8 `sha2` extension.
#[target_feature(enable = "sha2")]
pub(super) unsafe fn sha256_compress(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    use crate::sha256::K;
    // SAFETY: `state` has 8 u32s: two vectors of 4.
    let (mut s0, mut s1) = unsafe { (vld1q_u32(state.as_ptr()), vld1q_u32(state.as_ptr().add(4))) };
    for block in blocks {
        let (s0_saved, s1_saved) = (s0, s1);
        // SAFETY: each block is 64 bytes; four 16-byte chunks are read.
        let mut msg = unsafe {
            [
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr()))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(16)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(32)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(48)))),
            ]
        };
        for i in 0..16 {
            // SAFETY: `K` has 64 constants; the 4 of this group are read.
            let k = unsafe { vld1q_u32(K.as_ptr().add(4 * i)) };
            let wk = vaddq_u32(msg[i % 4], k);
            let prev = s0;
            s0 = vsha256hq_u32(s0, s1, wk);
            s1 = vsha256h2q_u32(s1, prev, wk);
            if i < 12 {
                msg[i % 4] = vsha256su1q_u32(
                    vsha256su0q_u32(msg[i % 4], msg[(i + 1) % 4]),
                    msg[(i + 2) % 4],
                    msg[(i + 3) % 4],
                );
            }
        }
        s0 = vaddq_u32(s0, s0_saved);
        s1 = vaddq_u32(s1, s1_saved);
    }
    // SAFETY: the 8 u32s of `state` are written.
    unsafe {
        vst1q_u32(state.as_mut_ptr(), s0);
        vst1q_u32(state.as_mut_ptr().add(4), s1);
    }
}

/// Weights of the bytes of a 16-byte block in `b` (the first byte is added 16 times).
const ADLER_WEIGHTS: [u8; 16] = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];

/// Adler-32 with NEON, 16 bytes at a time.
///
/// For `n` blocks after `(a, b)`: `a += Σx`, `b += 16n·a + 16·Σ_k(blocks before k) + Σ(16−j)·x_j`;
/// every lane stays far below `u32::MAX` within `NMAX` bytes (see `crate::adler32`).
///
/// # Safety
/// The CPU must have NEON (every AArch64 CPU does).
#[target_feature(enable = "neon")]
pub(super) unsafe fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let mut a = u64::from(adler & 0xffff);
    let mut b = u64::from(adler >> 16);
    // SAFETY: 16 bytes are read from the constant.
    let w = unsafe { vld1q_u8(ADLER_WEIGHTS.as_ptr()) };
    for chunk in data.chunks(crate::adler32::NMAX) {
        let (blocks, rest) = chunk.as_chunks::<16>();
        let (mut s1, mut acc, mut s2) = (vdupq_n_u32(0), vdupq_n_u32(0), vdupq_n_u32(0));
        for blk in blocks {
            // SAFETY: each block has 16 bytes.
            let v = unsafe { vld1q_u8(blk.as_ptr()) };
            acc = vaddq_u32(acc, s1);
            s1 = vpadalq_u16(s1, vpaddlq_u8(v));
            s2 = vpadalq_u16(s2, vmull_u8(vget_low_u8(v), vget_low_u8(w)));
            s2 = vpadalq_u16(s2, vmull_u8(vget_high_u8(v), vget_high_u8(w)));
        }
        let n = 16 * blocks.len() as u64;
        b += n * a + 16 * u64::from(vaddvq_u32(acc)) + u64::from(vaddvq_u32(s2));
        a += u64::from(vaddvq_u32(s1));
        for &x in rest {
            a += u64::from(x);
            b += a;
        }
        a %= crate::adler32::MOD;
        b %= crate::adler32::MOD;
    }
    ((b << 16) | a) as u32
}
