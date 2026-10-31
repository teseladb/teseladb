//! The generators of the YCSB loads (002 FR-018, R6), from their publication (Cooper and others,
//! «Benchmarking Cloud Serving Systems with YCSB», SoCC 2010, doi:10.1145/1807128.1807152, section
//! 4.1) and, for the zipfian one, from the algorithm it cites (Gray and others, «Quickly Generating
//! Billion-Record Synthetic Databases», SIGMOD 1994, section 3.4): a seeded random source, the
//! uniform ([`Rng::below`]), zipfian, scrambled zipfian and «latest» distributions, the workloads A to F and the
//! records of ten fields of 100 bytes.

/// Exponent of the zipfian distribution in YCSB.
pub const ZIPF_THETA: f64 = 0.99;
/// Fields of a record.
pub const FIELDS: usize = 10;
/// Bytes of each field.
pub const FIELD_BYTES: usize = 100;
/// Longest scan of workload E (uniform from 1).
pub const MAX_SCAN: u64 = 100;

/// xorshift64*: a small seeded source, so that two runs with the same seed draw the same load.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seeded (a zero seed becomes one: xorshift needs a nonzero state).
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n); `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// FNV-1a of the eight bytes of `v`: what YCSB scrambles keys with.
pub fn fnv64(v: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in v.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// ζ(n, θ) = Σ 1/i^θ for i in 1..=n.
fn zeta(n: u64, theta: f64) -> f64 {
    (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum()
}

/// Zipfian over [0, n) with exponent θ (Gray and others, 1994): item 0 is the most popular.
#[derive(Clone, Debug)]
pub struct Zipfian {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
}

impl Zipfian {
    /// Over `n` items (at least 2).
    pub fn new(n: u64, theta: f64) -> Zipfian {
        let zetan = zeta(n, theta);
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta(2, theta) / zetan);
        Zipfian { n, theta, alpha: 1.0 / (1.0 - theta), zetan, eta }
    }

    /// Next item.
    pub fn next(&self, rng: &mut Rng) -> u64 {
        let u = rng.unit();
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(self.theta) {
            return 1;
        }
        let v = (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
        v.min(self.n - 1)
    }
}

/// How a workload picks the key of an operation.
#[derive(Clone, Debug)]
pub enum KeyChooser {
    /// Zipfian popularity spread over the key space with FNV, as YCSB's «scrambled zipfian».
    ScrambledZipfian(Zipfian),
    /// The most recently inserted keys are the most popular.
    Latest(Zipfian),
}

impl KeyChooser {
    /// A key among the `n` inserted so far.
    pub fn next(&self, rng: &mut Rng, n: u64) -> u64 {
        match self {
            KeyChooser::ScrambledZipfian(z) => fnv64(z.next(rng)) % n,
            KeyChooser::Latest(z) => n - 1 - z.next(rng).min(n - 1),
        }
    }
}

/// An operation of a workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// Read a record.
    Read,
    /// Replace one field of a record.
    Update,
    /// Insert a new record.
    Insert,
    /// Read up to `MAX_SCAN` records in key order from a key.
    Scan,
    /// Read a record and write one field of it back.
    ReadModifyWrite,
}

impl Op {
    /// Name in the report.
    pub fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Update => "update",
            Op::Insert => "insert",
            Op::Scan => "scan",
            Op::ReadModifyWrite => "rmw",
        }
    }
}

/// A YCSB workload: its operation mix and its key distribution.
#[derive(Clone, Debug)]
pub struct Workload {
    /// Operations with their shares, adding up to 1.
    pub mix: Vec<(Op, f64)>,
    /// Key distribution.
    pub keys: KeyChooser,
}

impl Workload {
    /// Workload `letter` (`a` to `f`) over `records` records, as the publication defines them.
    pub fn new(letter: char, records: u64) -> Option<Workload> {
        let zipf = || KeyChooser::ScrambledZipfian(Zipfian::new(records.max(2), ZIPF_THETA));
        let (mix, keys) = match letter {
            'a' => (vec![(Op::Read, 0.5), (Op::Update, 0.5)], zipf()),
            'b' => (vec![(Op::Read, 0.95), (Op::Update, 0.05)], zipf()),
            'c' => (vec![(Op::Read, 1.0)], zipf()),
            'd' => (
                vec![(Op::Read, 0.95), (Op::Insert, 0.05)],
                KeyChooser::Latest(Zipfian::new(records.max(2), ZIPF_THETA)),
            ),
            'e' => (vec![(Op::Scan, 0.95), (Op::Insert, 0.05)], zipf()),
            'f' => (vec![(Op::Read, 0.5), (Op::ReadModifyWrite, 0.5)], zipf()),
            _ => return None,
        };
        Some(Workload { mix, keys })
    }

    /// Next operation.
    pub fn next_op(&self, rng: &mut Rng) -> Op {
        let mut u = rng.unit();
        for (op, share) in &self.mix {
            if u < *share {
                return *op;
            }
            u -= share;
        }
        self.mix[self.mix.len() - 1].0
    }
}

/// Key of record `i`: `user` and the FNV of its number, as YCSB names them (hashed so that
/// inserts in order do not all land at the end of the key space).
pub fn key(i: u64) -> String {
    format!("user{}", fnv64(i))
}

/// A field value of 100 random printable bytes.
pub fn field(rng: &mut Rng) -> String {
    (0..FIELD_BYTES).map(|_| char::from(b'a' + (rng.below(26) as u8))).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const DRAWS: u64 = 200_000;

    /// Probability of item `i`: 1/(i+1)^θ / ζ(n, θ).
    fn probability(z: &Zipfian, i: u64) -> f64 {
        1.0 / ((i + 1) as f64).powf(z.theta) / z.zetan
    }

    #[test]
    fn the_source_is_reproducible_and_uniform() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert_eq!(
            (0..10).map(|_| a.next_u64()).collect::<Vec<_>>(),
            (0..10).map(|_| b.next_u64()).collect::<Vec<_>>()
        );
        assert_ne!(Rng::new(0).next_u64(), 0, "a zero seed still draws");
        let mut rng = Rng::new(1);
        let mut counts = [0u64; 10];
        for _ in 0..DRAWS {
            counts[rng.below(10) as usize] += 1;
        }
        // Each bucket within 3 % of a tenth.
        for c in counts {
            assert!((c as f64 - DRAWS as f64 / 10.0).abs() < DRAWS as f64 * 0.003, "{counts:?}");
        }
        let u = rng.unit();
        assert!((0.0..1.0).contains(&u));
    }

    #[test]
    fn zipfian_follows_its_probabilities() {
        let n = 1000;
        let z = Zipfian::new(n, ZIPF_THETA);
        let mut rng = Rng::new(3);
        let mut counts = vec![0u64; n as usize];
        for _ in 0..DRAWS {
            counts[z.next(&mut rng) as usize] += 1;
        }
        // Gray's method draws items 0 and 1 with their exact probabilities, 1/(i+1)^θ / ζ(n, θ),
        // and the rest from an integral approximation (as YCSB does): those two within 5 %, and
        // the average popularity of an item falls from one band of ranks to the next.
        for i in 0..2 {
            let want = probability(&z, i) * DRAWS as f64;
            assert!(
                (counts[i as usize] as f64 - want).abs() < want * 0.05,
                "item {i}: {} vs {want}",
                counts[i as usize]
            );
        }
        let band = |from: usize, to: usize| {
            counts[from..to].iter().sum::<u64>() as f64 / (to - from) as f64
        };
        let bands = [band(0, 1), band(1, 2), band(2, 10), band(10, 100), band(100, 1000)];
        assert!(bands.windows(2).all(|w| w[0] > w[1]), "{bands:?}");
        // The band of the 900 least popular items gets about what the distribution gives it.
        let tail: f64 = (100..n).map(|i| probability(&z, i)).sum::<f64>() * DRAWS as f64;
        let got = counts[100..].iter().sum::<u64>() as f64;
        assert!((got - tail).abs() < tail * 0.1, "{got} vs {tail}");
        let probs: f64 = (0..n).map(|i| probability(&z, i)).sum();
        assert!((probs - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scrambled_zipfian_spreads_the_hot_keys_and_latest_favours_new_ones() {
        let n = 1000;
        let mut rng = Rng::new(5);
        let scrambled = KeyChooser::ScrambledZipfian(Zipfian::new(n, ZIPF_THETA));
        let mut counts = vec![0u64; n as usize];
        for _ in 0..DRAWS {
            counts[scrambled.next(&mut rng, n) as usize] += 1;
        }
        let hottest = (0..n as usize).max_by_key(|&i| counts[i]).expect("keys");
        assert_eq!(hottest as u64, fnv64(0) % n, "the most popular is item 0, scrambled");
        assert!(counts[hottest] as f64 > DRAWS as f64 * 0.05);
        let latest = KeyChooser::Latest(Zipfian::new(n, ZIPF_THETA));
        let draws: Vec<u64> = (0..DRAWS).map(|_| latest.next(&mut rng, n)).collect();
        assert!(draws.iter().all(|&k| k < n));
        let newest = draws.iter().filter(|&&k| k == n - 1).count() as f64;
        assert!(newest > DRAWS as f64 * 0.05, "the newest key is the most read");
        // With fewer keys inserted than the distribution's range, keys stay below the count.
        assert!((0..1000).all(|_| latest.next(&mut rng, 10) < 10));
    }

    #[test]
    fn the_six_workloads_have_their_mixes() {
        let mut rng = Rng::new(9);
        for (letter, op, share) in [
            ('a', Op::Update, 0.5),
            ('b', Op::Read, 0.95),
            ('c', Op::Read, 1.0),
            ('d', Op::Insert, 0.05),
            ('e', Op::Scan, 0.95),
            ('f', Op::ReadModifyWrite, 0.5),
        ] {
            let w = Workload::new(letter, 1000).expect("a workload");
            let got =
                (0..DRAWS).filter(|_| w.next_op(&mut rng) == op).count() as f64 / DRAWS as f64;
            assert!((got - share).abs() < 0.01, "{letter}: {got} vs {share}");
            assert!((w.mix.iter().map(|(_, s)| s).sum::<f64>() - 1.0).abs() < 1e-9);
        }
        assert!(matches!(Workload::new('d', 10).expect("d").keys, KeyChooser::Latest(_)));
        assert!(Workload::new('g', 10).is_none());
        let names: Vec<&str> = [Op::Read, Op::Update, Op::Insert, Op::Scan, Op::ReadModifyWrite]
            .iter()
            .map(|o| o.name())
            .collect();
        assert_eq!(names, ["read", "update", "insert", "scan", "rmw"]);
    }

    #[test]
    fn records_have_ten_fields_of_a_hundred_bytes_and_hashed_keys() {
        let mut rng = Rng::new(11);
        let f = field(&mut rng);
        assert_eq!(f.len(), FIELD_BYTES);
        assert!(f.bytes().all(|b| b.is_ascii_lowercase()));
        assert_eq!(key(1), format!("user{}", fnv64(1)));
        assert_ne!(key(1), key(2));
        assert_eq!(FIELDS, 10);
    }
}
