//! How often each index of the node served a query plan, for `$indexStats` (010 FR-018):
//! a reference replica set member 9.0.4 counts in `accesses.ops` each read whose winning plan scans
//! the index, since `accesses.since`, the moment it began counting (measured 01-11-2026).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The uses of each index, by namespace and index name: how many and since when (ms). The map is
/// nested so that counting a use of an index already listed allocates nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexUsage {
    uses: BTreeMap<String, BTreeMap<String, (i64, i64)>>,
}

impl IndexUsage {
    /// Counts one use of index `name` of `ns` at `now_ms`.
    pub fn used(&mut self, ns: &str, name: &str, now_ms: i64) {
        self.entry(ns, name, now_ms).0 += 1;
    }

    /// The uses of index `name` of `ns` and since when, counting from `now_ms` for an index not
    /// counted before.
    pub fn of(&mut self, ns: &str, name: &str, now_ms: i64) -> (i64, i64) {
        *self.entry(ns, name, now_ms)
    }

    /// Forgets the indexes of `ns` that are not in `names` (dropped since): one created again
    /// with the same name counts from zero.
    pub fn keep_only(&mut self, ns: &str, names: &[String]) {
        if let Some(of_ns) = self.uses.get_mut(ns) {
            of_ns.retain(|i, _| names.contains(i));
        }
    }

    fn entry(&mut self, ns: &str, name: &str, now_ms: i64) -> &mut (i64, i64) {
        if !self.uses.contains_key(ns) {
            self.uses.insert(ns.to_owned(), BTreeMap::new());
        }
        let of_ns = self.uses.get_mut(ns).expect("inserted above");
        if !of_ns.contains_key(name) {
            of_ns.insert(name.to_owned(), (0, now_ms));
        }
        of_ns.get_mut(name).expect("inserted above")
    }
}

/// Stripes of [`IndexUses`]: the uses of an index live in stripe `hash(ns, name) % STRIPES`.
const STRIPES: usize = 16;

/// The node's [`IndexUsage`] in stripes that each take their own lock, outside the node's
/// (H-013 of the native tests): every read through an index counts a use, and with 8
/// connections the node's lock was where their threads queued. Each index lives in one stripe,
/// so its count and the moment it began counting are those of one table.
#[derive(Debug, Default)]
pub struct IndexUses {
    stripes: [Mutex<IndexUsage>; STRIPES],
}

impl IndexUses {
    fn stripe(&self, ns: &str, name: &str) -> MutexGuard<'_, IndexUsage> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (ns, name).hash(&mut h);
        let i = usize::try_from(h.finish() % STRIPES as u64).unwrap_or(0);
        self.stripes[i].lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// [`IndexUsage::used`].
    pub fn used(&self, ns: &str, name: &str, now_ms: i64) {
        self.stripe(ns, name).used(ns, name, now_ms);
    }

    /// [`IndexUsage::of`].
    pub fn of(&self, ns: &str, name: &str, now_ms: i64) -> (i64, i64) {
        self.stripe(ns, name).of(ns, name, now_ms)
    }

    /// [`IndexUsage::keep_only`], in every stripe.
    pub fn keep_only(&self, ns: &str, names: &[String]) {
        for s in &self.stripes {
            s.lock().unwrap_or_else(PoisonError::into_inner).keep_only(ns, names);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IndexUsage;

    #[test]
    fn the_stripes_count_each_index_once_from_any_thread() {
        let u = std::sync::Arc::new(super::IndexUses::default());
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let u = u.clone();
                std::thread::spawn(move || {
                    for i in 0..200 {
                        u.used("d.c", &format!("i{}", i % 20), 10 + w);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        for i in 0..20 {
            let (ops, since) = u.of("d.c", &format!("i{i}"), 99);
            assert_eq!(ops, 80, "i{i}");
            assert!((10..18).contains(&since), "counts from its first use: {since}");
        }
        u.keep_only("d.c", &[String::from("i3")]);
        assert_eq!(u.of("d.c", "i3", 99).0, 80);
        assert_eq!(u.of("d.c", "i4", 99), (0, 99), "a dropped index counts from zero again");
    }

    #[test]
    fn uses_count_from_their_first_moment() {
        let mut u = IndexUsage::default();
        assert_eq!(u.of("d.c", "a_1", 5), (0, 5));
        u.used("d.c", "a_1", 9);
        u.used("d.c", "b_1", 7);
        assert_eq!(u.of("d.c", "a_1", 20), (1, 5));
        assert_eq!(u.of("d.c", "b_1", 20), (1, 7));
        u.keep_only("d.c", &["a_1".to_owned()]);
        assert_eq!(u.of("d.c", "b_1", 30), (0, 30));
        assert_eq!(u.of("d.c", "a_1", 30), (1, 5));
    }
}
