//! Elias-Fano code of a non-decreasing sequence of integers (FR-040): each value's low `l` bits
//! go packed in one array, and its high bits in unary in another (bit `high + i` set for the
//! `i`-th value), about `2 + log2(universe / n)` bits per value. Access by position and the search
//! of the first value at or above a bound work on the code as it is.

/// A non-decreasing sequence of `u64`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EliasFano {
    len: usize,
    low_bits: u32,
    low: Vec<u64>,
    high: Vec<u64>,
    /// Position in `high` of every 64th set bit, so [`EliasFano::get`] starts near its value.
    samples: Vec<u32>,
}

const SAMPLE: usize = 64;

fn get_bits(words: &[u64], at: usize, width: u32) -> u64 {
    if width == 0 {
        return 0;
    }
    let (w, b) = (at / 64, at % 64);
    let mut v = words[w] >> b;
    if b + width as usize > 64 {
        v |= words[w + 1] << (64 - b);
    }
    v & (u64::MAX >> (64 - width))
}

fn set_bits(words: &mut [u64], at: usize, width: u32, v: u64) {
    if width == 0 {
        return;
    }
    let (w, b) = (at / 64, at % 64);
    words[w] |= v << b;
    if b + width as usize > 64 {
        words[w + 1] |= v >> (64 - b);
    }
}

impl EliasFano {
    /// The code of `values`, which must not decrease.
    pub(crate) fn new(values: &[u64]) -> EliasFano {
        let len = values.len();
        let Some(&max) = values.last() else { return EliasFano::default() };
        debug_assert!(values.windows(2).all(|w| w[0] <= w[1]));
        let low_bits = match max / len as u64 {
            0 => 0,
            q => 63 - q.leading_zeros(),
        };
        let mut low = vec![0u64; (len * low_bits as usize).div_ceil(64) + 1];
        let high_len = len + (max >> low_bits) as usize + 1;
        let mut high = vec![0u64; high_len.div_ceil(64)];
        let mut samples = Vec::with_capacity(len / SAMPLE + 1);
        for (i, &v) in values.iter().enumerate() {
            set_bits(&mut low, i * low_bits as usize, low_bits, v & ((1u64 << low_bits) - 1));
            let bit = (v >> low_bits) as usize + i;
            high[bit / 64] |= 1 << (bit % 64);
            if i % SAMPLE == 0 {
                samples.push(bit as u32);
            }
        }
        EliasFano { len, low_bits, low, high, samples }
    }

    #[cfg(test)]
    /// How many values it has.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The position in `high` of the set bit of value `i`.
    fn high_bit(&self, i: usize) -> usize {
        let mut bit = self.samples[i / SAMPLE] as usize;
        let mut left = i % SAMPLE;
        let mut word = bit / 64;
        let mut w = self.high[word] & (u64::MAX << (bit % 64));
        loop {
            let ones = w.count_ones() as usize;
            if left < ones {
                for _ in 0..left {
                    w &= w - 1;
                }
                bit = word * 64 + w.trailing_zeros() as usize;
                return bit;
            }
            left -= ones;
            word += 1;
            w = self.high[word];
        }
    }

    /// Value `i`.
    pub(crate) fn get(&self, i: usize) -> u64 {
        assert!(i < self.len, "value {i} of {}", self.len);
        let high = (self.high_bit(i) - i) as u64;
        high << self.low_bits | get_bits(&self.low, i * self.low_bits as usize, self.low_bits)
    }

    #[cfg(test)]
    /// The position of the first value at or above `bound`, or `len` if none.
    pub(crate) fn successor(&self, bound: u64) -> usize {
        // The values are sorted: a binary search over positions reads each one in place.
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.get(mid) < bound {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    #[cfg(test)]
    /// Bytes it takes.
    pub(crate) fn bytes(&self) -> usize {
        8 * (self.low.len() + self.high.len()) + 4 * self.samples.len()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_value_and_successor_reads_back() {
        let mut x = 7u64;
        let mut v = 0u64;
        let values: Vec<u64> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                v += x % 20_000;
                v
            })
            .collect();
        let ef = EliasFano::new(&values);
        assert_eq!(ef.len(), values.len());
        for (i, want) in values.iter().enumerate() {
            assert_eq!(ef.get(i), *want, "value {i}");
        }
        for probe in [0, 1, values[10], values[10] + 1, values[4999], values[4999] + 1] {
            assert_eq!(ef.successor(probe), values.partition_point(|v| *v < probe), "{probe}");
        }
        // About 2 + log2(10 000) bits per value, well under the 64 of the plain array.
        assert!(ef.bytes() * 8 < values.len() * 20, "{} bytes", ef.bytes());
    }

    #[test]
    fn repeats_zeros_and_edge_cases() {
        for values in [vec![], vec![0], vec![0, 0, 0], vec![5, 5, 9], vec![u64::MAX / 2; 3]] {
            let ef = EliasFano::new(&values);
            let back: Vec<u64> = (0..ef.len()).map(|i| ef.get(i)).collect();
            assert_eq!(back, values);
            assert_eq!(ef.successor(6), values.partition_point(|v| *v < 6));
        }
        let aligned: Vec<u64> = (0..300).map(|i| i * 4096).collect();
        let ef = EliasFano::new(&aligned);
        assert!((0..300).all(|i| ef.get(i) == aligned[i]));
    }
}
