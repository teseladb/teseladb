//! The load a range's leader measures for a split by load (007 FR-006, plan «Divisiones por
//! carga»). Every operation the leader serves is counted in its second of the node's monotonic
//! clock and the keys it touched, from the first to the last, go into a reservoir of
//! [`RESERVOIR`] samples (each operation since the count started has the same chance of being in
//! it). Once [`LOAD_SECONDS`] whole seconds in a row each had at least the threshold of
//! operations, the cut key is the first key of a sample with at least [`MIN_SIDE_PERCENT`] of the
//! samples on each side that cuts through the fewest of them, at most [`MAX_SPLIT_PERCENT`], and
//! then leaves the halves closest to equal: an operation on both sides of a cut becomes a
//! transaction across two ranges, which costs more than the operation did (a write of a
//! document and of its index entries, H-051 of the native tests). A single hot key gives no cut,
//! nor do operations that all span the same keys. A second below the threshold, or one without
//! operations, starts the count and the reservoir again.

use teseladb_types::Nanos;

/// Operations per second above which a range splits by load, until the cluster parameter
/// `teselaLoadSplitOpsPerSecond` sets another (FR-006).
pub const LOAD_SPLIT_OPS: u64 = 2500;

/// Keys the reservoir keeps.
pub const RESERVOIR: usize = 1024;

/// Whole seconds in a row above the threshold before a cut (FR-006).
pub const LOAD_SECONDS: u64 = 10;

/// Least share of the samples, in percent, each side of a cut keeps.
pub const MIN_SIDE_PERCENT: usize = 20;

/// Most share of the samples, in percent, a cut may fall inside of.
pub const MAX_SPLIT_PERCENT: usize = 5;

const SECOND: u64 = 1_000_000_000;

/// The load of one range, as its leader measures it.
#[derive(Clone, Debug)]
pub struct Load {
    threshold: u64,
    rng: u64,
    /// The second being counted (`None` before the first operation).
    second: Option<u64>,
    /// Operations in that second.
    count: u64,
    /// Operations in the last whole second.
    last: u64,
    /// Whole seconds in a row at or above the threshold.
    streak: u64,
    /// Operations offered to the reservoir since it started.
    seen: u64,
    /// The first and the last key of each operation sampled.
    keys: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Load {
    /// A count with `threshold` operations per second, whose reservoir draws from `seed`.
    pub fn new(threshold: u64, seed: u64) -> Load {
        Load {
            threshold,
            rng: seed | 1,
            second: None,
            count: 0,
            last: 0,
            streak: 0,
            seen: 0,
            keys: Vec::new(),
        }
    }

    fn random(&mut self) -> u64 {
        // xorshift64*.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn restart(&mut self) {
        self.streak = 0;
        self.seen = 0;
        self.keys.clear();
    }

    /// Closes the seconds before the one of `now`.
    fn roll(&mut self, now: Nanos) {
        let second = now.0 / SECOND;
        let Some(current) = self.second else {
            self.second = Some(second);
            return;
        };
        if second <= current {
            return;
        }
        self.last = self.count;
        if self.count >= self.threshold {
            self.streak += 1;
        } else {
            self.restart();
        }
        // Seconds without any operation in between.
        if second > current + 1 {
            self.last = 0;
            self.restart();
        }
        self.second = Some(second);
        self.count = 0;
    }

    /// An operation on `key` at `now`.
    pub fn record(&mut self, key: &[u8], now: Nanos) {
        self.record_span(key, key, now);
    }

    /// An operation at `now` on keys from `first` to `last`.
    pub fn record_span(&mut self, first: &[u8], last: &[u8], now: Nanos) {
        self.roll(now);
        self.count += 1;
        self.seen += 1;
        let sample = || (first.to_vec(), last.to_vec());
        if self.keys.len() < RESERVOIR {
            self.keys.push(sample());
            return;
        }
        let j = self.random() % self.seen;
        if let Some(slot) = usize::try_from(j).ok().and_then(|j| self.keys.get_mut(j)) {
            *slot = sample();
        }
    }

    /// `ops` operations at `now`, of which `keys` are a uniform sample: the reads a connection's
    /// thread served without the core (005 FR-028), reported in batches. Each key stands for an
    /// equal share of them, as if the core had served them.
    pub fn record_sampled(&mut self, keys: &[Vec<u8>], ops: u64, now: Nanos) {
        let n = keys.len() as u64;
        if n == 0 {
            return;
        }
        for (i, key) in (0u64..).zip(keys) {
            let share = ops / n + u64::from(i < ops % n);
            for _ in 0..share {
                self.record(key, now);
            }
        }
    }

    /// Operations in the last whole second.
    pub fn ops_per_second(&self) -> u64 {
        self.last
    }

    /// Keys in the reservoir.
    pub fn samples(&self) -> usize {
        self.keys.len()
    }

    /// The cut key of a range that starts at `start` once the load stayed above the threshold
    /// for [`LOAD_SECONDS`] seconds: strictly above `start`, with at least
    /// [`MIN_SIDE_PERCENT`] of the samples on each side and inside at most
    /// [`MAX_SPLIT_PERCENT`] of them. Asking ends the count either way.
    pub fn cut(&mut self, now: Nanos, start: &[u8]) -> Option<Vec<u8>> {
        self.roll(now);
        if self.streak < LOAD_SECONDS {
            return None;
        }
        let samples = std::mem::take(&mut self.keys);
        self.restart();
        let mut firsts: Vec<&[u8]> = samples.iter().map(|(f, _)| f.as_slice()).collect();
        let mut lasts: Vec<&[u8]> = samples.iter().map(|(_, l)| l.as_slice()).collect();
        firsts.sort_unstable();
        lasts.sort_unstable();
        let n = firsts.len();
        // The fewest operations cut through first, then the halves closest to equal.
        let mut best: Option<((usize, usize), &[u8])> = None;
        for (left, k) in firsts.iter().enumerate() {
            let first_of_key = left == 0 || firsts[left - 1] != *k;
            let sides =
                left * 100 >= MIN_SIDE_PERCENT * n && (n - left) * 100 >= MIN_SIDE_PERCENT * n;
            if !first_of_key || !sides || *k <= start {
                continue;
            }
            // The samples that start below `k` and do not end below it: a cut at `k` falls
            // inside them (a sample's last key is never below its first).
            let ended = lasts.partition_point(|l| l < k);
            let inside = left - ended;
            if inside * 100 > MAX_SPLIT_PERCENT * n {
                continue;
            }
            let score = (inside, (2 * left).abs_diff(n));
            if best.is_none_or(|(b, _)| score < b) {
                best = Some((score, k));
            }
        }
        best.map(|(_, k)| k.to_vec())
    }
}
