//! Log-linear latency histogram: relative error < 1 % between 1 ns and 100 s, exact merging.

/// Sub-buckets per power of two (2⁷): maximum relative error 1/128 ≈ 0.8 %.
const SUB_BITS: u32 = 7;
const SUB: usize = 1 << SUB_BITS;
/// Largest value represented with precision: 100 s in nanoseconds.
const MAX_VALUE: u64 = 100_000_000_000;
/// Powers of two needed to cover `MAX_VALUE` (2³⁷ > 10¹¹).
const OCTAVES: usize = 37;
const BUCKETS: usize = (OCTAVES + 1) * SUB;

/// Histogram of `u64` values (usually nanoseconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram::new()
    }
}

fn bucket_of(v: u64) -> usize {
    let v = v.clamp(1, MAX_VALUE);
    if v < SUB as u64 {
        // Small values: one bucket per value (exact).
        return v as usize;
    }
    let msb = 63 - v.leading_zeros(); // ≥ SUB_BITS
    let octave = (msb - SUB_BITS + 1) as usize;
    let sub = ((v >> (msb - SUB_BITS)) as usize) & (SUB - 1);
    octave * SUB + sub
}

/// Highest value that falls in bucket `b`.
fn bucket_upper(b: usize) -> u64 {
    if b < SUB {
        return b as u64;
    }
    let octave = (b / SUB) as u32;
    let sub = (b % SUB) as u64;
    let shift = octave - 1;
    (((SUB as u64) | sub) << shift) + ((1u64 << shift) - 1)
}

impl Histogram {
    /// Empty histogram.
    pub fn new() -> Histogram {
        Histogram { counts: vec![0; BUCKETS], total: 0, min: u64::MAX, max: 0, sum: 0 }
    }

    /// Records a value.
    pub fn record(&mut self, v: u64) {
        self.record_n(v, 1);
    }

    /// Records the same value `n` times.
    pub fn record_n(&mut self, v: u64, n: u64) {
        if n == 0 {
            return;
        }
        self.counts[bucket_of(v)] += n;
        self.total += n;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += u128::from(v) * u128::from(n);
    }

    /// Number of recorded values.
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Exact minimum (0 if empty).
    pub fn min(&self) -> u64 {
        if self.total == 0 { 0 } else { self.min }
    }

    /// Exact maximum (0 if empty).
    pub fn max(&self) -> u64 {
        self.max
    }

    /// Exact mean (0 if empty).
    pub fn mean(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.sum as f64 / self.total as f64 }
    }

    /// Percentile `q` in `[0, 100]`: the smallest representable value such that at least `q` % of
    /// the records are ≤ it (bounded by the exact minimum and maximum).
    pub fn percentile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let q = q.clamp(0.0, 100.0);
        let rank = ((q / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        // `rank ≤ total`, so there is always a bucket that reaches it.
        let b = self
            .counts
            .iter()
            .position(|&c| {
                seen += c;
                seen >= rank
            })
            .unwrap_or(BUCKETS - 1);
        // The last bucket also collects the values above the range: its bound is the exact
        // maximum.
        if b >= bucket_of(MAX_VALUE) {
            return self.max;
        }
        bucket_upper(b).clamp(self.min, self.max)
    }

    /// Adds another histogram to this one (identical to having recorded both).
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    /// Clears the histogram.
    pub fn reset(&mut self) {
        *self = Histogram::new();
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::Rng;

    #[test]
    fn empty_histogram() {
        let h = Histogram::default();
        assert_eq!(h.count(), 0);
        assert_eq!(h.min(), 0);
        assert_eq!(h.max(), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.percentile(99.0), 0);
    }

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 1..=100 {
            h.record(v);
        }
        h.record_n(5, 0);
        assert_eq!(h.count(), 100);
        assert_eq!(h.percentile(50.0), 50);
        assert_eq!(h.percentile(99.0), 99);
        assert_eq!(h.percentile(100.0), 100);
        assert_eq!(h.percentile(0.0), 1);
        assert_eq!(h.min(), 1);
        assert_eq!(h.max(), 100);
        assert!((h.mean() - 50.5).abs() < 1e-9);
    }

    #[test]
    fn relative_error_below_one_percent() {
        let mut rng = Rng::new(11);
        for _ in 0..20_000 {
            let v = rng.range(1, MAX_VALUE + 1);
            let b = bucket_of(v);
            let up = bucket_upper(b);
            assert!(up >= v, "{v} > bound {up}");
            assert!((up - v) as f64 / v as f64 <= 1.0 / SUB as f64, "error at {v}: {up}");
            assert!(b < BUCKETS);
        }
        // Out of range: clamped, but min/max stay exact.
        let mut h = Histogram::new();
        h.record(0);
        h.record(MAX_VALUE * 10);
        assert_eq!(h.min(), 0);
        assert_eq!(h.max(), MAX_VALUE * 10);
        assert_eq!(h.percentile(100.0), MAX_VALUE * 10);
    }

    #[test]
    fn merge_equals_recording_both() {
        let mut rng = Rng::new(5);
        let (mut a, mut b, mut both) = (Histogram::new(), Histogram::new(), Histogram::new());
        for i in 0..5_000 {
            let v = rng.range(1, 10_000_000);
            if i % 2 == 0 {
                a.record(v)
            } else {
                b.record(v)
            }
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a, both);
        assert_eq!(a.percentile(99.9), both.percentile(99.9));
        a.reset();
        assert_eq!(a.count(), 0);
    }
}
