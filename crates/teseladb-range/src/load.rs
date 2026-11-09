//! The load a range's leader measures for a split by load (007 FR-006, plan «Divisiones por
//! carga»). Every operation the leader serves is counted in its second of the node's monotonic
//! clock and its key goes into a reservoir of [`RESERVOIR`] keys (each key served since the count
//! started has the same chance of being in it). Once [`LOAD_SECONDS`] whole seconds in a row each
//! had at least the threshold of operations, the cut key is the key of the reservoir that leaves
//! the halves closest to equal, with at least [`MIN_SIDE_PERCENT`] of the samples on each side;
//! a single hot key gives none (no cut separates its operations). A second below the threshold,
//! or one without operations, starts the count and the reservoir again.

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
    keys: Vec<Vec<u8>>,
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
        self.roll(now);
        self.count += 1;
        self.seen += 1;
        if self.keys.len() < RESERVOIR {
            self.keys.push(key.to_vec());
            return;
        }
        let j = self.random() % self.seen;
        if let Some(slot) = usize::try_from(j).ok().and_then(|j| self.keys.get_mut(j)) {
            *slot = key.to_vec();
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
    /// [`MIN_SIDE_PERCENT`] of the samples on each side. Asking ends the count either way.
    pub fn cut(&mut self, now: Nanos, start: &[u8]) -> Option<Vec<u8>> {
        self.roll(now);
        if self.streak < LOAD_SECONDS {
            return None;
        }
        let mut keys = std::mem::take(&mut self.keys);
        self.restart();
        keys.sort_unstable();
        let n = keys.len();
        let mut best: Option<(usize, &[u8])> = None;
        for (left, k) in keys.iter().enumerate() {
            let first_of_key = left == 0 || keys[left - 1] != *k;
            let sides =
                left * 100 >= MIN_SIDE_PERCENT * n && (n - left) * 100 >= MIN_SIDE_PERCENT * n;
            if !first_of_key || !sides || k.as_slice() <= start {
                continue;
            }
            let off = (2 * left).abs_diff(n);
            if best.is_none_or(|(o, _)| off < o) {
                best = Some((off, k));
            }
        }
        best.map(|(_, k)| k.to_vec())
    }
}
