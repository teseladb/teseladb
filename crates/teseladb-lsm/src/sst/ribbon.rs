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

/// Builds the filter of a file's keys in steps: a step adds some keys to the system, or solves
/// some of its rows, so that whoever writes the file serves other work between two of them. The
/// filter is the one [`build`] gives: the same keys go in, in the same order, with the same seeds.
#[derive(Debug)]
pub(crate) struct Builder {
    keys: Vec<(u32, u32)>,
    /// Growths of the rows so far, and the seed tried with them.
    attempt: u32,
    seed: u32,
    /// The system: each row's coefficients and fingerprint once it holds a key.
    coeff: Vec<u64>,
    result: Vec<u8>,
    /// Keys added to the system so far.
    added: usize,
    /// The solution, from row `solved` to the last one, once every key is in.
    z: Vec<u8>,
    solved: usize,
}

impl Builder {
    /// A builder of the filter of `keys` (their hashes).
    pub(crate) fn new(keys: Vec<(u32, u32)>) -> Builder {
        let mut b = Builder {
            keys,
            attempt: 0,
            seed: 0,
            coeff: Vec::new(),
            result: Vec::new(),
            added: 0,
            z: Vec::new(),
            solved: 0,
        };
        b.restart();
        b
    }

    /// Starts the system again with the current seed and growth.
    fn restart(&mut self) {
        let rows = rows_for(self.keys.len(), self.attempt);
        self.coeff.clear();
        self.coeff.resize(rows, 0);
        self.result.clear();
        self.result.resize(rows, 0);
        self.added = 0;
        self.z.clear();
        self.solved = rows;
    }

    /// The next seed, or more rows after [`SEEDS`] of them: the system was inconsistent.
    fn next_seed(&mut self) {
        self.seed += 1;
        if self.seed == SEEDS {
            self.seed = 0;
            self.attempt += 1;
        }
        self.restart();
    }

    /// Adds key `k`; `false` when it makes the system inconsistent.
    fn add(&mut self, k: (u32, u32)) -> bool {
        let rows = self.coeff.len();
        let (mut s, mut c, mut r) = row(k, self.seed, rows);
        loop {
            if self.coeff[s] == 0 {
                self.coeff[s] = c;
                self.result[s] = r;
                return true;
            }
            c ^= self.coeff[s];
            r ^= self.result[s];
            if c == 0 {
                return r == 0;
            }
            let tz = c.trailing_zeros() as usize;
            s += tz;
            c >>= tz;
        }
    }

    /// Solves row `i`, the rows after it solved.
    fn solve_row(&mut self, i: usize) {
        let mut v = self.result[i];
        let mut c = self.coeff[i] >> 1;
        let mut j = i + 1;
        while c != 0 {
            let tz = c.trailing_zeros() as usize;
            j += tz;
            v ^= self.z[j];
            c >>= tz + 1;
            j += 1;
        }
        self.z[i] = if self.coeff[i] == 0 { 0 } else { v };
    }

    /// Adds up to `work` keys, or solves up to `work` rows once every key is in. The payload of
    /// the filter when it is done (see [`build`]).
    pub(crate) fn step(&mut self, work: usize) -> Option<Vec<u8>> {
        let work = work.max(1);
        if self.added < self.keys.len() {
            let end = self.keys.len().min(self.added.saturating_add(work));
            while self.added < end {
                if !self.add(self.keys[self.added]) {
                    self.next_seed();
                    return None;
                }
                self.added += 1;
            }
            return None;
        }
        if self.z.is_empty() {
            self.z.resize(self.coeff.len(), 0);
        }
        let stop = self.solved.saturating_sub(work);
        while self.solved > stop {
            self.solved -= 1;
            self.solve_row(self.solved);
        }
        if self.solved > 0 {
            return None;
        }
        let mut z = std::mem::take(&mut self.z);
        z.extend_from_slice(&self.seed.to_le_bytes());
        z.push(RIBBON);
        Some(z)
    }
}

/// Builds the payload of the filter of `keys` (their hashes) at once.
#[cfg(test)]
pub(crate) fn build(keys: &[(u32, u32)]) -> Vec<u8> {
    let mut b = Builder::new(keys.to_vec());
    loop {
        if let Some(z) = b.step(usize::MAX) {
            return z;
        }
    }
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

    /// The filter as it was built before [`Builder`]: the whole system at once.
    fn built_at_once(keys: &[(u32, u32)]) -> Vec<u8> {
        let solve = |seed: u32, rows: usize| -> Option<Vec<u8>> {
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
        };
        for attempt in 0.. {
            let rows = rows_for(keys.len(), attempt);
            for seed in 0..SEEDS {
                if let Some(mut z) = solve(seed, rows) {
                    z.extend_from_slice(&seed.to_le_bytes());
                    z.push(RIBBON);
                    return z;
                }
            }
        }
        unreachable!()
    }

    /// The filter built in steps of `work`, and the steps it took.
    fn built_in_steps(keys: &[(u32, u32)], work: usize) -> (Vec<u8>, usize) {
        let mut b = Builder::new(keys.to_vec());
        let mut steps = 1;
        loop {
            if let Some(z) = b.step(work) {
                return (z, steps);
            }
            steps += 1;
        }
    }

    /// A filter built in steps is the one built at once, byte for byte, and each step adds or
    /// solves at most its share.
    #[test]
    fn a_filter_built_in_steps_is_the_filter_built_at_once() {
        for n in [0usize, 1, 63, 64, 1000, 20_000] {
            let keys: Vec<_> = (0..n).map(|i| hashes(format!("doc:{i}").as_bytes())).collect();
            let whole = built_at_once(&keys);
            assert_eq!(build(&keys), whole, "{n} keys at once");
            for work in [1, 7, 1000] {
                let (z, steps) = built_in_steps(&keys, work);
                assert_eq!(z, whole, "{n} keys in steps of {work}");
                let rows = whole.len() - 5;
                assert_eq!(steps, n.div_ceil(work) + rows.div_ceil(work), "{n} keys, {work}");
            }
        }
    }

    /// Keys that make the first seed's system inconsistent: the builder starts again with the
    /// next seed, as the filter built at once does, and every key is found.
    #[test]
    fn an_inconsistent_system_starts_again_with_another_seed() {
        const N: usize = 300;
        let rows = rows_for(N, 0);
        // 70 keys whose rows all start at row 0 over 64 columns: they cannot all be independent.
        let mut keys: Vec<_> = (0u32..)
            .map(|i| hashes(format!("first:{i}").as_bytes()))
            .filter(|k| row(*k, 0, rows).0 == 0)
            .take(70)
            .collect();
        keys.extend((0..N - 70).map(|i| hashes(format!("rest:{i}").as_bytes())));
        let whole = built_at_once(&keys);
        let (z, _) = built_in_steps(&keys, 16);
        assert_eq!(z, whole);
        let (rows, seed) = parse(&z).unwrap();
        assert!(seed > 0, "the first seed fails");
        assert!(keys.iter().all(|k| may_contain(&z[..rows], seed, *k)));
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
