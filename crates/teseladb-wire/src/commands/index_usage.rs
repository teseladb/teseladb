//! How often each index of the node served a query plan, for `$indexStats` (010 FR-018): a
//! `refnode` 9.0.4 counts in `accesses.ops` each read whose winning plan scans the index, since
//! `accesses.since`, the moment it began counting (measured 01-11-2026).

use std::collections::BTreeMap;

/// The uses of each index, by namespace and index name: how many and since when (ms).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexUsage {
    uses: BTreeMap<(String, String), (i64, i64)>,
}

impl IndexUsage {
    /// Counts one use of index `name` of `ns` at `now_ms`.
    pub fn used(&mut self, ns: &str, name: &str, now_ms: i64) {
        self.uses.entry((ns.to_owned(), name.to_owned())).or_insert((0, now_ms)).0 += 1;
    }

    /// The uses of index `name` of `ns` and since when, counting from `now_ms` for an index not
    /// counted before.
    pub fn of(&mut self, ns: &str, name: &str, now_ms: i64) -> (i64, i64) {
        *self.uses.entry((ns.to_owned(), name.to_owned())).or_insert((0, now_ms))
    }

    /// Forgets the indexes of `ns` that are not in `names` (dropped since): one created again
    /// with the same name counts from zero.
    pub fn keep_only(&mut self, ns: &str, names: &[String]) {
        self.uses.retain(|(n, i), _| n != ns || names.contains(i));
    }
}

#[cfg(test)]
mod tests {
    use super::IndexUsage;

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
