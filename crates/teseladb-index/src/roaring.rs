//! Roaring bitmaps of local document ids (009 FR-023): the 32-bit ids are split by their high 16
//! bits into containers of 2^16 values, each kept as whichever of three forms takes less room: a
//! sorted array (up to 4,096 values), a bitmap of 1,024 words, or runs of consecutive values.
//! `$and`, `$or` and `$in` across indexes become intersections and unions of these sets.

/// The most values an array container holds before a bitmap is smaller.
const ARRAY_MAX: usize = 4096;
/// Words of a bitmap container.
const WORDS: usize = 1024;

/// One container's values (their low 16 bits).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Container {
    Array(Vec<u16>),
    Bitmap(Box<[u64; WORDS]>),
    /// Runs `(start, length - 1)`, sorted and apart.
    Run(Vec<(u16, u16)>),
}

/// A set of 32-bit ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roaring {
    /// Containers by their high 16 bits, sorted.
    keys: Vec<u16>,
    containers: Vec<Container>,
}

impl Container {
    fn len(&self) -> usize {
        match self {
            Container::Array(a) => a.len(),
            Container::Bitmap(b) => b.iter().map(|w| w.count_ones() as usize).sum(),
            Container::Run(r) => r.iter().map(|&(_, l)| usize::from(l) + 1).sum(),
        }
    }

    fn contains(&self, v: u16) -> bool {
        match self {
            Container::Array(a) => a.binary_search(&v).is_ok(),
            Container::Bitmap(b) => b[usize::from(v >> 6)] >> (v & 63) & 1 == 1,
            Container::Run(r) => {
                let i = r.partition_point(|&(s, _)| s <= v);
                i > 0 && {
                    let (s, l) = r[i - 1];
                    u32::from(v) <= u32::from(s) + u32::from(l)
                }
            }
        }
    }

    fn bitmap(&self) -> Box<[u64; WORDS]> {
        let mut b = Box::new([0u64; WORDS]);
        for v in self.values() {
            b[usize::from(v >> 6)] |= 1 << (v & 63);
        }
        b
    }

    /// The values in order.
    fn values(&self) -> Box<dyn Iterator<Item = u16> + '_> {
        match self {
            Container::Array(a) => Box::new(a.iter().copied()),
            Container::Bitmap(b) => Box::new(b.iter().enumerate().flat_map(|(i, &w)| {
                let mut w = w;
                std::iter::from_fn(move || {
                    (w != 0).then(|| {
                        let bit = w.trailing_zeros();
                        w &= w - 1;
                        (i as u16) << 6 | bit as u16
                    })
                })
            })),
            Container::Run(r) => Box::new(r.iter().flat_map(|&(s, l)| {
                (u32::from(s)..=u32::from(s) + u32::from(l)).map(|v| v as u16)
            })),
        }
    }

    /// The form that takes the fewest bytes for these values.
    fn optimized(self) -> Container {
        let values: Vec<u16> = self.values().collect();
        let mut runs: Vec<(u16, u16)> = Vec::new();
        for &v in &values {
            match runs.last_mut() {
                Some((s, l)) if u32::from(*s) + u32::from(*l) + 1 == u32::from(v) => *l += 1,
                _ => runs.push((v, 0)),
            }
        }
        let (array_bytes, run_bytes, bitmap_bytes) = (values.len() * 2, runs.len() * 4, WORDS * 8);
        if run_bytes < array_bytes.min(bitmap_bytes) {
            Container::Run(runs)
        } else if values.len() <= ARRAY_MAX {
            Container::Array(values)
        } else {
            let mut b = Box::new([0u64; WORDS]);
            for v in values {
                b[usize::from(v >> 6)] |= 1 << (v & 63);
            }
            Container::Bitmap(b)
        }
    }

    fn and(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Bitmap(a), Container::Bitmap(b)) => {
                let mut out = Box::new([0u64; WORDS]);
                // A loop over words the compiler vectorizes.
                for ((o, x), y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
                    *o = x & y;
                }
                Container::Bitmap(out).optimized()
            }
            (Container::Array(a), b) | (b, Container::Array(a)) => {
                Container::Array(a.iter().copied().filter(|&v| b.contains(v)).collect())
            }
            (a, b) => {
                let (x, y) = (a.bitmap(), b.bitmap());
                Container::Bitmap(x).and(&Container::Bitmap(y))
            }
        }
    }

    fn or(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Array(a), Container::Array(b)) if a.len() + b.len() <= ARRAY_MAX => {
                let mut out = Vec::with_capacity(a.len() + b.len());
                let (mut i, mut j) = (0, 0);
                while i < a.len() || j < b.len() {
                    let next = match (a.get(i), b.get(j)) {
                        (Some(&x), Some(&y)) if x == y => {
                            i += 1;
                            j += 1;
                            x
                        }
                        (Some(&x), Some(&y)) if x < y => {
                            i += 1;
                            x
                        }
                        (Some(_), Some(&y)) | (None, Some(&y)) => {
                            j += 1;
                            y
                        }
                        (Some(&x), None) => {
                            i += 1;
                            x
                        }
                        (None, None) => break,
                    };
                    out.push(next);
                }
                Container::Array(out).optimized()
            }
            _ => {
                let mut out = self.bitmap();
                for (o, w) in out.iter_mut().zip(other.bitmap().iter()) {
                    *o |= w;
                }
                Container::Bitmap(out).optimized()
            }
        }
    }
}

impl Roaring {
    /// The empty set.
    pub fn new() -> Roaring {
        Roaring::default()
    }

    /// The set of `ids` (in any order).
    pub fn from_ids(ids: impl IntoIterator<Item = u32>) -> Roaring {
        let mut sorted: Vec<u32> = ids.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut out = Roaring::new();
        let mut i = 0;
        while i < sorted.len() {
            let high = (sorted[i] >> 16) as u16;
            let end = i + sorted[i..].partition_point(|&v| (v >> 16) as u16 == high);
            let low: Vec<u16> = sorted[i..end].iter().map(|&v| v as u16).collect();
            out.keys.push(high);
            out.containers.push(Container::Array(low).optimized());
            i = end;
        }
        out
    }

    /// How many ids.
    pub fn len(&self) -> usize {
        self.containers.iter().map(Container::len).sum()
    }

    /// Whether it has no id.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `id` is in the set.
    pub fn contains(&self, id: u32) -> bool {
        match self.keys.binary_search(&((id >> 16) as u16)) {
            Ok(i) => self.containers[i].contains(id as u16),
            Err(_) => false,
        }
    }

    /// The ids in order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.keys
            .iter()
            .zip(&self.containers)
            .flat_map(|(&k, c)| c.values().map(move |v| u32::from(k) << 16 | u32::from(v)))
    }

    /// Adds `id`; whether it was not in the set yet. An array container that outgrows its
    /// largest size becomes a bitmap, and so does a container of runs that takes a value.
    pub fn insert(&mut self, id: u32) -> bool {
        let (high, low) = ((id >> 16) as u16, id as u16);
        let i = match self.keys.binary_search(&high) {
            Ok(i) => i,
            Err(i) => {
                self.keys.insert(i, high);
                self.containers.insert(i, Container::Array(vec![low]));
                return true;
            }
        };
        let c = &mut self.containers[i];
        if c.contains(low) {
            return false;
        }
        match c {
            Container::Array(a) if a.len() < ARRAY_MAX => {
                let at = a.partition_point(|v| *v < low);
                a.insert(at, low);
            }
            Container::Bitmap(b) => b[usize::from(low >> 6)] |= 1 << (low & 63),
            Container::Array(_) | Container::Run(_) => {
                let mut b = c.bitmap();
                b[usize::from(low >> 6)] |= 1 << (low & 63);
                *c = Container::Bitmap(b);
            }
        }
        true
    }

    /// The ids in both (`$and`).
    pub fn and(&self, other: &Roaring) -> Roaring {
        let mut out = Roaring::new();
        let (mut i, mut j) = (0, 0);
        while i < self.keys.len() && j < other.keys.len() {
            match self.keys[i].cmp(&other.keys[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    let c = self.containers[i].and(&other.containers[j]);
                    if c.len() > 0 {
                        out.keys.push(self.keys[i]);
                        out.containers.push(c);
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    /// The ids in either (`$or`, `$in`).
    pub fn or(&self, other: &Roaring) -> Roaring {
        let mut out = Roaring::new();
        let (mut i, mut j) = (0, 0);
        while i < self.keys.len() || j < other.keys.len() {
            let take = match (self.keys.get(i), other.keys.get(j)) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match take {
                std::cmp::Ordering::Less => {
                    out.keys.push(self.keys[i]);
                    out.containers.push(self.containers[i].clone());
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.keys.push(other.keys[j]);
                    out.containers.push(other.containers[j].clone());
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.keys.push(self.keys[i]);
                    out.containers.push(self.containers[i].or(&other.containers[j]));
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    /// The bytes its containers take, for the planner's estimate.
    pub fn size_in_bytes(&self) -> usize {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Array(a) => a.len() * 2,
                Container::Bitmap(_) => WORDS * 8,
                Container::Run(r) => r.len() * 4,
            })
            .sum::<usize>()
            + self.keys.len() * 2
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::collections::BTreeSet;

    use super::{Container, Roaring};

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// Ids of one of three shapes: sparse, dense (bitmaps) and in runs.
    fn ids(rng: &mut Rng) -> BTreeSet<u32> {
        let mut out = BTreeSet::new();
        match rng.below(3) {
            0 => (0..rng.below(3000)).for_each(|_| {
                out.insert(rng.below(300_000) as u32);
            }),
            1 => (0..rng.below(20_000)).for_each(|_| {
                out.insert(rng.below(70_000) as u32);
            }),
            _ => (0..rng.below(20)).for_each(|_| {
                let start = rng.below(200_000) as u32;
                out.extend(start..start + rng.below(5000) as u32);
            }),
        }
        out
    }

    #[test]
    fn set_operations_equal_ordered_sets() {
        let mut rng = Rng(0x0BAD_CAFE_F00D_1234);
        for _ in 0..60 {
            let (a, b) = (ids(&mut rng), ids(&mut rng));
            let (ra, rb) =
                (Roaring::from_ids(a.iter().copied()), Roaring::from_ids(b.iter().copied()));
            assert_eq!(ra.iter().collect::<Vec<_>>(), a.iter().copied().collect::<Vec<_>>());
            assert_eq!(ra.len(), a.len());
            let and: Vec<u32> = a.intersection(&b).copied().collect();
            let or: Vec<u32> = a.union(&b).copied().collect();
            assert_eq!(ra.and(&rb).iter().collect::<Vec<_>>(), and);
            assert_eq!(ra.or(&rb).iter().collect::<Vec<_>>(), or);
            for probe in 0..200 {
                let id = rng.below(300_000) as u32 + probe;
                assert_eq!(ra.contains(id), a.contains(&id));
            }
        }
    }

    #[test]
    fn inserting_ids_equals_an_ordered_set() {
        let mut rng = Rng(0x5EED_0F1D_0000_0045);
        for _ in 0..40 {
            let mut want = BTreeSet::new();
            let base = ids(&mut rng);
            let mut set = Roaring::from_ids(base.iter().copied());
            want.extend(base);
            for _ in 0..rng.below(6000) {
                let id = rng.below(200_000) as u32;
                assert_eq!(set.insert(id), want.insert(id), "id {id}");
            }
            assert_eq!(set.iter().collect::<Vec<_>>(), want.iter().copied().collect::<Vec<_>>());
        }
    }

    #[test]
    fn each_container_takes_its_smallest_form() {
        let runs = Roaring::from_ids(0..60_000);
        assert!(matches!(runs.containers[0], Container::Run(_)));
        let sparse = Roaring::from_ids((0..100).map(|i| i * 7));
        assert!(matches!(sparse.containers[0], Container::Array(_)));
        let dense = Roaring::from_ids((0..65_536).filter(|i| i % 3 == 0));
        assert!(matches!(dense.containers[0], Container::Bitmap(_)));
        assert!(runs.size_in_bytes() < 16);
        assert!(Roaring::new().is_empty());
    }
}
