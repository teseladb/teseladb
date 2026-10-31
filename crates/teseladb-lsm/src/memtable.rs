//! Memtables (FR-002): the writes not yet in an SST.
//!
//! The active memtable is an ordered tree by internal key; when it reaches its budget it is frozen
//! into a sorted vector, which reads walk by position, and waits for its flush. Each memtable
//! keeps its range tombstones, the lowest and highest timestamps it holds and, per range, the
//! lowest and highest Raft index applied to it (FR-010, FR-011).

use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_types::{Bytes, GroupId};

use crate::entry::{self, Kind, RangeTombstone};

/// Fixed memory cost counted per version, besides its key and value.
pub(crate) const ENTRY_OVERHEAD: u64 = 64;

/// Internal key ordered as [`entry::compare`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IKey(pub(crate) Box<[u8]>);

impl Ord for IKey {
    fn cmp(&self, other: &IKey) -> std::cmp::Ordering {
        entry::compare(&self.0, &other.0)
    }
}

impl PartialOrd for IKey {
    fn partial_cmp(&self, other: &IKey) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A version stored in a memtable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) kind: Kind,
    pub(crate) value: Bytes,
}

/// Lowest and highest Raft index applied to a memtable for one range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) first: u64,
    pub(crate) last: u64,
}

/// A memtable.
#[derive(Debug, Default)]
pub(crate) struct Memtable {
    tree: BTreeMap<IKey, Slot>,
    frozen: Vec<(IKey, Slot)>,
    pub(crate) tombstones: Vec<RangeTombstone>,
    pub(crate) applied: BTreeMap<GroupId, Applied>,
    pub(crate) min_ts: Option<Hlc>,
    pub(crate) max_ts: Option<Hlc>,
    bytes: u64,
}

impl Memtable {
    /// Bytes counted against the budget.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Number of versions.
    pub(crate) fn len(&self) -> usize {
        self.tree.len() + self.frozen.len()
    }

    /// Whether it holds nothing (no version and no range tombstone).
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0 && self.tombstones.is_empty()
    }

    fn note(&mut self, range: GroupId, index: u64, ts: Hlc) {
        self.note_index(range, index);
        self.min_ts = Some(self.min_ts.map_or(ts, |m| m.min(ts)));
        self.max_ts = Some(self.max_ts.map_or(ts, |m| m.max(ts)));
    }

    /// Adds a version; one already there at the same internal key is replaced.
    pub(crate) fn insert(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        kind: Kind,
        value: &[u8],
    ) {
        debug_assert!(self.frozen.is_empty(), "a frozen memtable takes no writes");
        let ikey = IKey(entry::internal_key(key, ts).into_boxed_slice());
        let key_bytes = ikey.0.len() as u64;
        let slot = Slot { kind, value: Bytes::copy_from_slice(value) };
        if let Some(old) = self.tree.insert(ikey, slot) {
            self.bytes -= key_bytes + old.value.len() as u64 + ENTRY_OVERHEAD;
        }
        self.bytes += key_bytes + value.len() as u64 + ENTRY_OVERHEAD;
        self.note(range, index, ts);
    }

    /// Adds a range tombstone.
    pub(crate) fn insert_tombstone(&mut self, range: GroupId, index: u64, t: RangeTombstone) {
        debug_assert!(self.frozen.is_empty(), "a frozen memtable takes no writes");
        self.bytes += (t.start.len() + t.end.len()) as u64 + ENTRY_OVERHEAD;
        self.note(range, index, t.ts);
        self.tombstones.push(t);
    }

    /// Records an applied index of a range.
    pub(crate) fn note_index(&mut self, range: GroupId, index: u64) {
        self.applied
            .entry(range)
            .and_modify(|a| {
                a.first = a.first.min(index);
                a.last = a.last.max(index);
            })
            .or_insert(Applied { first: index, last: index });
    }

    /// Turns the tree into a sorted vector; no more writes come.
    pub(crate) fn freeze(&mut self) {
        let tree = std::mem::take(&mut self.tree);
        self.frozen = tree.into_iter().collect();
    }

    /// The first version at or after the internal key `at`, with its position for
    /// [`Memtable::next_after`] (a position only means something in a frozen memtable).
    pub(crate) fn seek(&self, at: &[u8]) -> Option<(usize, &[u8], &Slot)> {
        if self.frozen.is_empty() {
            use std::ops::Bound;
            let probe = IKey(at.into());
            return self
                .tree
                .range((Bound::Included(probe), Bound::Unbounded))
                .next()
                .map(|(k, s)| (usize::MAX, &*k.0, s));
        }
        let i = self.frozen.partition_point(|(k, _)| entry::compare(&k.0, at).is_lt());
        self.frozen.get(i).map(|(k, s)| (i, &*k.0, s))
    }

    /// The version after the one at `pos` (found by [`Memtable::seek`] at `key`). A position
    /// taken before the memtable was frozen is found again by its key.
    pub(crate) fn next_after(&self, pos: usize, key: &[u8]) -> Option<(usize, &[u8], &Slot)> {
        if self.frozen.is_empty() {
            use std::ops::Bound;
            let probe = IKey(key.into());
            return self
                .tree
                .range((Bound::Excluded(probe), Bound::Unbounded))
                .next()
                .map(|(k, s)| (usize::MAX, &*k.0, s));
        }
        let next = if pos == usize::MAX {
            self.frozen.partition_point(|(k, _)| entry::compare(&k.0, key).is_le())
        } else {
            pos + 1
        };
        self.frozen.get(next).map(|(k, s)| (next, &*k.0, s))
    }

    /// Every version in order, once frozen (what a flush writes).
    pub(crate) fn sorted(&self) -> &[(IKey, Slot)] {
        &self.frozen
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    fn walk(m: &Memtable) -> Vec<(Vec<u8>, Hlc, Kind)> {
        let mut out = Vec::new();
        let mut at = m.seek(&entry::first_version(b""));
        while let Some((pos, k, s)) = at {
            let (l, t) = entry::split(k);
            out.push((l.to_vec(), t, s.kind));
            let key = k.to_vec();
            at = m.next_after(pos, &key);
        }
        out
    }

    #[test]
    fn walks_the_same_before_and_after_freezing() {
        let mut m = Memtable::default();
        let g = GroupId(1);
        m.insert(g, 5, b"b", ts(1), Kind::Put, b"x");
        m.insert(g, 3, b"a", ts(2), Kind::Put, b"y");
        m.insert(GroupId(2), 9, b"a", ts(7), Kind::Delete, b"");
        m.insert(g, 4, b"a", ts(2), Kind::Intent, b"zz");
        assert_eq!(m.len(), 3);
        let before = walk(&m);
        assert_eq!(
            before,
            vec![
                (b"a".to_vec(), ts(7), Kind::Delete),
                (b"a".to_vec(), ts(2), Kind::Intent),
                (b"b".to_vec(), ts(1), Kind::Put)
            ]
        );
        let (pos, k, _) = m.seek(&entry::first_version(b"a")).unwrap();
        let held = k.to_vec();
        m.freeze();
        assert_eq!(walk(&m), before);
        // A position from before the freeze goes on by key.
        let (_, k, _) = m.next_after(pos, &held).unwrap();
        assert_eq!(entry::split(k), (&b"a"[..], ts(2)));
        assert_eq!(m.sorted().len(), 3);
        assert_eq!(m.applied[&g], Applied { first: 3, last: 5 });
        assert_eq!((m.min_ts, m.max_ts), (Some(ts(1)), Some(ts(7))));
        let (_, k, _) = m.seek(&entry::internal_key(b"a", ts(3))).unwrap();
        assert_eq!(entry::split(k), (&b"a"[..], ts(2)));
        assert!(m.seek(&entry::after_all_versions(b"b")).is_none());
    }

    #[test]
    fn replacing_a_version_keeps_the_count_of_bytes() {
        let mut m = Memtable::default();
        m.insert(GroupId(1), 1, b"k", ts(1), Kind::Put, b"1234");
        let one = m.bytes();
        m.insert(GroupId(1), 2, b"k", ts(1), Kind::Put, b"5678");
        assert_eq!(m.bytes(), one);
        m.insert_tombstone(
            GroupId(1),
            3,
            RangeTombstone { start: b"a".to_vec(), end: b"z".to_vec(), ts: ts(9) },
        );
        assert!(m.bytes() > one);
        m.note_index(GroupId(4), 8);
        assert_eq!(m.applied[&GroupId(4)], Applied { first: 8, last: 8 });
        assert!(!m.is_empty());
        assert!(Memtable::default().is_empty());
    }
}
