//! Ribbon filter of the logical keys of a file (FR-042): a banded linear system over GF(2) whose
//! solution answers, for each key, an 8-bit fingerprint (about 0.4 % false positives) in about
//! 8.8 bits per key, against the 10 of the Bloom filter it replaces.
//!
//! Each key gets a start row `s`, a 64-bit coefficient row `c` whose lowest bit is set, and a
//! fingerprint `r`. The filter stores one byte `z[i]` per row such that the XOR of `z[s + j]`
//! over the bits `j` set in `c` is `r` for every key. Building puts the keys' rows in echelon
//! form (each row keeps the first column it still has set) and solves from the last row up; a
//! key that would make the system inconsistent makes it start again with another seed, and
//! after a few seeds with more rows.
//!
//! Payload: `z (one byte per row) · seed(u32 LE) · RIBBON(u8)`. The Bloom filters of files written
//! before end with their number of probes, from 1 to 30.

use super::Bad;

/// The last byte of a Ribbon filter's payload.
pub(crate) const RIBBON: u8 = 0xA1;
/// Columns of a coefficient row.
const WIDTH: usize = 64;
/// Seeds tried with one number of rows before it grows.
const SEEDS: u32 = 16;

fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The start row, coefficients and fingerprint of a key's hashes over `rows` rows.
fn row((h1, h2): (u32, u32), seed: u32, rows: usize) -> (usize, u64, u8) {
    let h =
        (u64::from(h1) << 32 | u64::from(h2)) ^ u64::from(seed).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let a = mix(h);
    let b = mix(a ^ 0x2545_f491_4f6c_dd1d);
    let starts = (rows - WIDTH + 1) as u128;
    let s = ((u128::from(a) * starts) >> 64) as usize;
    // The fingerprint comes from bits of its own: the start takes the high bits of `a`.
    (s, b | 1, mix(b ^ a.rotate_left(17)) as u8)
}

/// Rows for `n` keys at the `attempt`-th growth.
fn rows_for(n: usize, attempt: u32) -> usize {
    let base = n + n / 10 + WIDTH;
    base + base * attempt as usize / 10
}

/// Solves the system of `keys` over `rows` rows with `seed`, if it is consistent.
fn solve(keys: &[(u32, u32)], seed: u32, rows: usize) -> Option<Vec<u8>> {
    let mut coeff = vec![0u64; rows];
    let mut result = vec![0u8; rows];
    for &k in keys {
        let (mut s, mut c, mut r) = row(k, seed, rows);
        loop {
            if coeff[s] == 0 {
                coeff[s] = c;
                result[s] = r;
                break;
            }
            c ^= coeff[s];
            r ^= result[s];
            if c == 0 {
                if r != 0 {
                    return None;
                }
                break;
            }
            let tz = c.trailing_zeros() as usize;
            s += tz;
            c >>= tz;
        }
    }
    let mut z = vec![0u8; rows];
    for i in (0..rows).rev() {
        let mut v = result[i];
        let mut c = coeff[i] >> 1;
        let mut j = i + 1;
        while c != 0 {
            let tz = c.trailing_zeros() as usize;
            j += tz;
            v ^= z[j];
            c >>= tz + 1;
            j += 1;
        }
        z[i] = if coeff[i] == 0 { 0 } else { v };
    }
    Some(z)
}

/// Builds the payload of the filter of `keys` (their hashes).
pub(crate) fn build(keys: &[(u32, u32)]) -> Vec<u8> {
    for attempt in 0.. {
        let rows = rows_for(keys.len(), attempt);
        for seed in 0..SEEDS {
            if let Some(mut z) = solve(keys, seed, rows) {
                z.extend_from_slice(&seed.to_le_bytes());
                z.push(RIBBON);
                return z;
            }
        }
    }
    unreachable!("the rows grow until the system is consistent")
}

/// Whether the filter in `payload` (built by [`build`]) may hold the key of `hashes`.
pub(crate) fn may_contain(z: &[u8], seed: u32, hashes: (u32, u32)) -> bool {
    let (s, mut c, r) = row(hashes, seed, z.len());
    let mut v = 0u8;
    let mut j = s;
    while c != 0 {
        let tz = c.trailing_zeros() as usize;
        j += tz;
        v ^= z[j];
        c >>= tz + 1;
        j += 1;
    }
    v == r
}

/// The rows and seed of a Ribbon payload.
pub(crate) fn parse(payload: &[u8]) -> Result<(usize, u32), Bad> {
    let body = payload.len().checked_sub(5).ok_or(Bad)?;
    if payload[payload.len() - 1] != RIBBON || body < WIDTH {
        return Err(Bad);
    }
    let seed = u32::from_le_bytes(payload[body..body + 4].try_into().map_err(|_| Bad)?);
    Ok((body, seed))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::sst::filter::hashes;

    fn filter(keys: &[(u32, u32)]) -> (Vec<u8>, u32) {
        let p = build(keys);
        let (rows, seed) = parse(&p).unwrap();
        (p[..rows].to_vec(), seed)
    }

    #[test]
    fn every_key_is_found_and_others_rarely() {
        for n in [0usize, 1, 5, 63, 64, 1000, 20_000] {
            let keys: Vec<_> = (0..n).map(|i| hashes(format!("user:{i}").as_bytes())).collect();
            let (z, seed) = filter(&keys);
            assert!(keys.iter().all(|k| may_contain(&z, seed, *k)), "{n} keys");
            if n >= 1000 {
                assert!(z.len() * 8 < n * 9 + 600, "{} bits for {n} keys", z.len() * 8);
                let probes = 50_000;
                let fp = (0..probes)
                    .filter(|i| may_contain(&z, seed, hashes(format!("other:{i}").as_bytes())))
                    .count();
                assert!(fp * 100 < probes, "{fp} in {probes}");
            }
        }
    }

    #[test]
    fn repeated_keys_build_and_garbage_is_refused() {
        let k = hashes(b"same");
        let (z, seed) = filter(&[k, k, k]);
        assert!(may_contain(&z, seed, k));
        assert_eq!(parse(&[1, 2, 3]), Err(Bad));
        assert_eq!(parse(&[0; 80]), Err(Bad));
        let mut short = vec![0; 10];
        short.push(RIBBON);
        assert_eq!(parse(&short), Err(Bad));
    }
}
