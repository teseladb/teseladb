//! Range descriptors by key (007 FR-002): Group 0 keeps its ranges by group and, next to them,
//! an index by first key, so the range of a key is the one with the greatest first key at or
//! below it (ranges never overlap) and the ranges of an interval come out in key order. The index
//! is derived from the descriptors: it is rebuilt on restore and never travels in the snapshot.

use std::collections::BTreeMap;
use std::ops::Bound;

use teseladb_types::{GroupId, NodeId};

use crate::group0::{Group0Sm, RangeDesc};

/// First key → range, for the ranges of Group 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeIndex {
    by_start: BTreeMap<Vec<u8>, GroupId>,
}

impl RangeIndex {
    /// Indexes range `g` by its first key.
    pub fn insert(&mut self, start: &[u8], g: GroupId) {
        self.by_start.insert(start.to_vec(), g);
    }

    /// Forgets the range whose first key is `start`.
    pub fn remove(&mut self, start: &[u8]) {
        self.by_start.remove(start);
    }

    /// The range with the greatest first key at or below `key`, if any.
    pub fn floor(&self, key: &[u8]) -> Option<GroupId> {
        self.by_start
            .range::<[u8], _>((Bound::Unbounded, Bound::Included(key)))
            .next_back()
            .map(|(_, g)| *g)
    }

    /// Ranges whose first key is in `[from, to)` (`to` empty: no end), in key order.
    pub fn starting_in<'a>(
        &'a self,
        from: &[u8],
        to: &'a [u8],
    ) -> impl Iterator<Item = GroupId> + 'a {
        self.by_start
            .range::<[u8], _>((Bound::Included(from), Bound::Unbounded))
            .take_while(move |(s, _)| to.is_empty() || s.as_slice() < to)
            .map(|(_, g)| *g)
    }

    /// Number of ranges indexed.
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    /// Whether no range is indexed.
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }
}

impl Group0Sm {
    /// Adds a descriptor with its entry in the index by key.
    pub fn insert_range(&mut self, desc: RangeDesc) {
        self.range_index.insert(&desc.start, desc.group);
        self.ranges.insert(desc.group, desc);
    }

    /// Rebuilds the index by key from the descriptors.
    pub(crate) fn reindex(&mut self) {
        self.range_index = RangeIndex::default();
        for r in self.ranges.values() {
            self.range_index.insert(&r.start, r.group);
        }
    }

    /// The range that holds `key`.
    pub fn range_for(&self, key: &[u8]) -> Option<&RangeDesc> {
        let g = self.range_index.floor(key)?;
        self.ranges.get(&g).filter(|r| r.contains(key))
    }

    /// The core for a new range on `replicas` (FR-004): the one below `cores` with the fewest
    /// ranges summed over those nodes, Group 0 counting on core 0 of each; the lowest on a tie.
    pub(crate) fn least_loaded_core(&self, replicas: &[NodeId], cores: u16) -> u16 {
        let mut load = vec![replicas.len(); usize::from(cores)];
        load[1..].fill(0);
        for r in self.ranges.values() {
            let shared = r.replicas.iter().filter(|(n, _)| replicas.contains(n)).count();
            if let Some(l) = load.get_mut(usize::from(r.core)) {
                *l += shared;
            }
        }
        let best = load.iter().enumerate().min_by_key(|(c, l)| (**l, *c)).map_or(0, |(c, _)| c);
        best as u16
    }

    /// The ranges that hold some key of `[start, end)` (`end` empty: no end), in key order.
    pub fn ranges_over(&self, start: &[u8], end: &[u8]) -> Vec<&RangeDesc> {
        let first = self.range_for(start).filter(|r| r.start.as_slice() < start);
        let rest = self.range_index.starting_in(start, end).filter_map(|g| self.ranges.get(&g));
        first.into_iter().chain(rest).collect()
    }
}
