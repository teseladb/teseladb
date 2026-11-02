//! MD5 (RFC 1321) for `hex_md5` of the shell, written from the RFC.
//!
//! `hex_md5` encodes the UTF-16 code units of its argument as UTF-8 and hashes the bytes. What a
//! `refrouter` 9.0.4 does with values that are not code units (measured on 02-11, through objects
//! whose `charCodeAt` returns them) is kept: a unit is compared as a number (`NaN` takes the
//! three-byte path), a lone surrogate is encoded in three bytes, and a "byte" outside 0..=255
//! (`-1`, or the lead byte of `70000`) is merged into its 32-bit word as a 32-bit integer.

/// The per-round shift amounts of RFC 1321, section 3.4.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];
/// The initial state of RFC 1321, section 3.3.
const INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const HIGH_SURROGATES: std::ops::RangeInclusive<f64> = 55_296.0..=56_319.0;
const LOW_SURROGATES: std::ops::RangeInclusive<f64> = 56_320.0..=57_343.0;

/// `ToInt32` of ECMAScript.
fn int32(x: f64) -> i32 {
    if !x.is_finite() {
        return 0;
    }
    // Truncated, then reduced modulo 2^32: the wrapping cast is the reduction.
    (x.trunc().rem_euclid(4_294_967_296.0) as u64) as u32 as i32
}

/// The UTF-8 "bytes" of `units`, as `hex_md5` computes them.
pub fn encode(units: &[f64]) -> Vec<i32> {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        let next = units.get(i + 1).copied();
        if c < 128.0 {
            out.push(int32(c));
        } else if c < 2048.0 {
            let n = int32(c);
            out.extend([0xc0 | (n >> 6), 0x80 | (n & 63)]);
        } else if let Some(low) =
            next.filter(|l| HIGH_SURROGATES.contains(&c) && LOW_SURROGATES.contains(l))
        {
            let cp = 65_536 + ((int32(c) - 55_296) << 10) + (int32(low) - 56_320);
            out.extend([0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 63), 0x80 | ((cp >> 6) & 63)]);
            out.push(0x80 | (cp & 63));
            i += 1;
        } else {
            let n = int32(c);
            out.extend([0xe0 | (n >> 12), 0x80 | ((n >> 6) & 63), 0x80 | (n & 63)]);
        }
        i += 1;
    }
    out
}

/// The message of RFC 1321 sections 3.1 and 3.2 as little-endian words: the bytes, the `0x80`
/// that starts the padding, and the length in bits.
fn words(bytes: &[i32]) -> Vec<u32> {
    let n = bytes.len();
    let count = (((n + 8) >> 6) + 1) * 16;
    let mut w = vec![0u32; count];
    for (i, b) in bytes.iter().enumerate() {
        w[i >> 2] |= (*b as u32).wrapping_shl(((i & 3) * 8) as u32);
    }
    w[n >> 2] |= 0x80u32 << ((n & 3) * 8);
    let bits = (n as u64).wrapping_mul(8);
    w[count - 2] = bits as u32;
    w[count - 1] = (bits >> 32) as u32;
    w
}

/// The digest of `bytes` (each one merged as a 32-bit integer), in lowercase hexadecimal.
pub fn hex_digest(bytes: &[i32]) -> String {
    // T[i] = floor(2^32 * |sin(i + 1)|), RFC 1321 section 3.4.
    let table: Vec<u32> =
        (0..64).map(|i| (f64::from(i + 1).sin().abs() * 4_294_967_296.0) as u32).collect();
    let mut state = INIT;
    for block in words(bytes).chunks(16) {
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(table[i]).wrapping_add(block[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(SHIFTS[i]));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    state.iter().flat_map(|w| w.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md5_of(s: &str) -> String {
        hex_digest(&s.bytes().map(i32::from).collect::<Vec<_>>())
    }

    /// The test suite of RFC 1321, appendix A.5.
    #[test]
    fn the_test_suite_of_the_rfc() {
        let suite = [
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
        for (input, digest) in suite {
            assert_eq!(md5_of(input), digest, "{input:?}");
        }
    }

    /// Code units against the refrouter 9.0.4 (measured on 02-11).
    #[test]
    fn code_units_as_the_refrouter_encodes_them() {
        let cases: [(&[f64], &str); 7] = [
            (&[233.0], "66ddcd97cfdeabb2f6fb8a999b4bc76f"),
            (&[55_357.0, 56_832.0], "2a02eac39d716a70ecf37579185927b6"),
            (&[56_832.0, 55_357.0], "18ff608e5f3cbc5f69e0ecc7aa77f412"),
            (&[-1.0], "7215484a42c9bd1321db26ce12faed82"),
            (&[f64::NAN], "b18e2e55624239ca74cce3de1ea87c0d"),
            (&[70_000.0], "8e5a4a0a845fb0d7c39515086ff5a088"),
            (&[1e10], "77168b5b9b07a56728121a0e2cf147be"),
        ];
        for (units, digest) in cases {
            assert_eq!(hex_digest(&encode(units)), digest, "{units:?}");
        }
    }
}
