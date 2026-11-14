//! Deterministic pseudo-random generator (xoshiro256** seeded with splitmix64).
//!
//! Not cryptographic: the simulator, election timers and tests use it. All randomness of the
//! core comes from an `Rng` handed in by the caller (constitution, principle II).

/// xoshiro256** generator. The same seed always yields the same sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    /// Creates a generator from a 64-bit seed.
    #[must_use]
    pub fn new(seed: u64) -> Rng {
        let mut st = seed;
        let s =
            [splitmix64(&mut st), splitmix64(&mut st), splitmix64(&mut st), splitmix64(&mut st)];
        Rng { s }
    }

    /// Next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Next 32-bit value (the high bits, which have the best quality).
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform integer in `[lo, hi)`, unbiased (Lemire's method).
    ///
    /// # Panics
    ///
    /// When `lo >= hi`.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Lemire's method keeps the low and the high 64 bits of the 128-bit product"
    )]
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo < hi, "empty range {lo}..{hi}");
        let span = hi - lo;
        let mut m = u128::from(self.next_u64()) * u128::from(span);
        let mut low = m as u64;
        if low < span {
            let threshold = span.wrapping_neg() % span;
            while low < threshold {
                m = u128::from(self.next_u64()) * u128::from(span);
                low = m as u64;
            }
        }
        lo + (m >> 64) as u64
    }

    /// Uniform index in `[0, n)`, as [`Self::range`] draws it.
    ///
    /// # Panics
    ///
    /// When `n` is 0.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a usize widens losslessly to u64, and the drawn value is below n, so it narrows back"
    )]
    pub fn below(&mut self, n: usize) -> usize {
        self.range(0, n as u64) as usize
    }

    /// Uniform real in `[0, 1)` with 53 bits of precision.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a 53-bit integer is exact in the 53-bit mantissa of f64"
    )]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `true` with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    /// Fills `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// Shuffles a slice (Fisher–Yates).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the drawn index is at most i, which is a usize"
    )]
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.range(0, i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// Independent generator derived from this one (to give each simulated node its own).
    #[must_use]
    pub fn fork(&mut self) -> Rng {
        Rng::new(self.next_u64())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_seed_dependent() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let va: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let vb: Vec<u64> = (0..100).map(|_| b.next_u64()).collect();
        let vc: Vec<u64> = (0..100).map(|_| c.next_u64()).collect();
        assert_eq!(va, vb);
        assert_ne!(va, vc);
        // Fixed reference value: if the algorithm changes, every simulator trace changes.
        assert_eq!(Rng::new(0).next_u64(), 0x99ec_5f36_cb75_f2b4);
    }

    #[test]
    fn range_is_bounded_and_roughly_uniform() {
        let mut r = Rng::new(7);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            let v = r.range(10, 16);
            assert!((10..16).contains(&v));
            counts[usize::try_from(v - 10).expect("below 6")] += 1;
        }
        for c in counts {
            assert!((9_000..11_000).contains(&c), "biased distribution: {counts:?}");
        }
        assert_eq!(r.range(5, 6), 5);
        // Range of 2⁶³ + 1: Lemire's rejection threshold is ~2⁶³, so rejections are frequent.
        let mut r = Rng::new(9);
        let span = (1u64 << 63) + 1;
        for _ in 0..1000 {
            assert!(r.range(0, span) < span);
        }
    }

    #[test]
    #[should_panic(expected = "empty range")]
    fn empty_range_panics() {
        Rng::new(1).range(3, 3);
    }

    #[test]
    fn floats_bytes_shuffle_and_fork() {
        let mut r = Rng::new(3);
        for _ in 0..1000 {
            let f = r.next_f64();
            assert!((0.0..1.0).contains(&f));
        }
        assert!(!r.chance(0.0));
        assert!(r.chance(1.0));
        let _ = r.next_u32();
        let mut buf = [0u8; 13];
        r.fill(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
        let mut v: Vec<u32> = (0..50).collect();
        r.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
        assert_ne!(v, sorted);
        let mut f1 = r.clone().fork();
        let mut f2 = r.fork();
        assert_eq!(f1.next_u64(), f2.next_u64());
    }
}
