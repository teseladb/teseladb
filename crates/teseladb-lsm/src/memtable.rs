//! Memtables (FR-002, FR-026): the writes not yet in an SST.
//!
//! A memtable is a skiplist by internal key with one writer, the core, and readers on any thread
//! without a lock ([`crate::skiplist`]); when it reaches its budget it is frozen (no more writes)
//! and waits for its flush. Each memtable keeps its range tombstones, the lowest and highest
//! timestamps it holds and, per range, the lowest and highest Raft index applied to it (FR-010,
//! FR-011). The exact timestamps and the applied indexes are the core's; readers on other threads
//! see the physical part of the timestamps, which only ever widens, to skip a memtable that cannot
//! hold what they look for.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use teseladb_keys::Hlc;
use teseladb_types::{Bytes, GroupId};

use crate::entry::{self, Kind, RangeTombstone};
use crate::skiplist::{SkipList, WriterCell};

/// Fixed memory cost counted per version, besides its key and value.
pub(crate) const ENTRY_OVERHEAD: u64 = 64;

/// A version's kind and value.
#[derive(Clone, Debug)]
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

/// What only the core reads and writes.
#[derive(Debug, Default)]
struct Meta {
    applied: BTreeMap<GroupId, Applied>,
    min_ts: Option<Hlc>,
    max_ts: Option<Hlc>,
    bytes: u64,
    tombstones: u64,
}

/// A memtable.
pub(crate) struct Memtable {
    versions: SkipList<Slot>,
    /// Range tombstones by their order of arrival.
    tombstones: SkipList<RangeTombstone>,
    /// Lowest and highest physical time of its versions and tombstones (`u64::MAX` and 0 while
    /// empty), for readers on any thread.
    min_physical: AtomicU64,
    max_physical: AtomicU64,
    frozen: AtomicBool,
    meta: WriterCell<Meta>,
}

impl std::fmt::Debug for Memtable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memtable").field("versions", &self.versions.len()).finish()
    }
}

impl Default for Memtable {
    fn default() -> Memtable {
        Memtable {
            versions: SkipList::new(entry::compare),
            tombstones: SkipList::new(|a, b| a.cmp(b)),
            min_physical: AtomicU64::new(u64::MAX),
            max_physical: AtomicU64::new(0),
            frozen: AtomicBool::new(false),
            meta: WriterCell::new(Meta::default()),
        }
    }
}

impl Memtable {
    /// Bytes counted against the budget.
    pub(crate) fn bytes(&self) -> u64 {
        self.meta.with(|m| m.bytes)
    }

    /// Number of versions.
    pub(crate) fn len(&self) -> usize {
        self.versions.len()
    }

    /// Whether it holds nothing (no version and no range tombstone).
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0 && self.tombstones.len() == 0
    }

    /// The applied indexes of each range (the core's).
    pub(crate) fn applied(&self) -> BTreeMap<GroupId, Applied> {
        self.meta.with(|m| m.applied.clone())
    }

    /// The applied indexes of `range` (the core's).
    pub(crate) fn applied_of(&self, range: GroupId) -> Option<Applied> {
        self.meta.with(|m| m.applied.get(&range).copied())
    }

    /// Its lowest and highest timestamps (the core's).
    pub(crate) fn times(&self) -> (Option<Hlc>, Option<Hlc>) {
        self.meta.with(|m| (m.min_ts, m.max_ts))
    }

    /// Whether it may hold a version at or below `read`.
    pub(crate) fn may_hold_at(&self, read: Hlc) -> bool {
        self.min_physical.load(Ordering::Acquire) <= read.physical
    }

    /// Whether it may hold a version newer than `ts`.
    pub(crate) fn may_hold_after(&self, ts: Hlc) -> bool {
        self.max_physical.load(Ordering::Acquire) >= ts.physical
    }

    fn note(&self, m: &mut Meta, range: GroupId, index: u64, ts: Hlc) {
        note_applied(m, range, index);
        m.min_ts = Some(m.min_ts.map_or(ts, |t| t.min(ts)));
        m.max_ts = Some(m.max_ts.map_or(ts, |t| t.max(ts)));
        // Only the core writes them: a store when they widen, no read-modify-write per version.
        if ts.physical < self.min_physical.load(Ordering::Relaxed) {
            self.min_physical.store(ts.physical, Ordering::Release);
        }
        if ts.physical > self.max_physical.load(Ordering::Relaxed) {
            self.max_physical.store(ts.physical, Ordering::Release);
        }
    }

    /// Adds a version; one already there at the same internal key is replaced.
    pub(crate) fn insert(
        &self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        kind: Kind,
        value: &[u8],
    ) {
        debug_assert!(!self.frozen.load(Ordering::Relaxed), "a frozen memtable takes no writes");
        let ikey = entry::internal_key(key, ts);
        let key_bytes = ikey.len() as u64;
        let slot = Slot { kind, value: Bytes::copy_from_slice(value) };
        self.meta.with(|m| {
            // The time first: a reader that finds the version also finds the memtable may hold
            // it.
            self.note(m, range, index, ts);
            if let Some(old) = self.versions.insert(ikey, slot) {
                m.bytes -= key_bytes + old.value.len() as u64 + ENTRY_OVERHEAD;
            }
            m.bytes += key_bytes + value.len() as u64 + ENTRY_OVERHEAD;
        });
    }

    /// Adds a range tombstone.
    pub(crate) fn insert_tombstone(&self, range: GroupId, index: u64, t: RangeTombstone) {
        debug_assert!(!self.frozen.load(Ordering::Relaxed), "a frozen memtable takes no writes");
        let order = self.meta.with(|m| {
            m.bytes += (t.start.len() + t.end.len()) as u64 + ENTRY_OVERHEAD;
            self.note(m, range, index, t.ts);
            m.tombstones += 1;
            m.tombstones
        });
        self.tombstones.insert(order.to_be_bytes(), t);
    }

    /// Records an applied index of a range.
    pub(crate) fn note_index(&self, range: GroupId, index: u64) {
        self.meta.with(|m| note_applied(m, range, index));
    }

    /// No more writes come.
    pub(crate) fn freeze(&self) {
        self.frozen.store(true, Ordering::Release);
    }

    /// Its range tombstones, in order of arrival.
    pub(crate) fn tombstones(&self) -> impl Iterator<Item = &RangeTombstone> {
        self.tombstones.iter().map(|e| e.value())
    }

    /// The first version at or after the internal key `at`, with a position for
    /// [`Memtable::next_after`] (positions are not used: the next version is found by key).
    pub(crate) fn seek(&self, at: &[u8]) -> Option<(usize, &[u8], &Slot)> {
        self.versions.seek(at).map(|e| (0, e.key(), e.value()))
    }

    /// The versions after `key` (from the first without it), in order: a flush walks them one
    /// chunk at a time without seeking each one.
    pub(crate) fn from(&self, key: Option<&[u8]>) -> impl Iterator<Item = (&[u8], &Slot)> {
        let start = match key {
            Some(k) => self.versions.after(k),
            None => self.versions.first(),
        };
        std::iter::successors(start, |e| e.next()).map(|e| (e.key(), e.value()))
    }

    /// The version after the one at `key`.
    pub(crate) fn next_after(&self, _pos: usize, key: &[u8]) -> Option<(usize, &[u8], &Slot)> {
        self.versions.after(key).map(|e| (0, e.key(), e.value()))
    }

    /// The last version before the internal key `at`.
    pub(crate) fn before(&self, at: &[u8]) -> Option<&[u8]> {
        self.versions.before(at).map(|e| e.key())
    }
}

fn note_applied(m: &mut Meta, range: GroupId, index: u64) {
    m.applied
        .entry(range)
        .and_modify(|a| {
            a.first = a.first.min(index);
            a.last = a.last.max(index);
        })
        .or_insert(Applied { first: index, last: index });
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
        let m = Memtable::default();
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
        let (_, k, _) = m.next_after(pos, &held).unwrap();
        assert_eq!(entry::split(k), (&b"a"[..], ts(2)));
        assert_eq!(m.len(), 3);
        assert_eq!(m.applied()[&g], Applied { first: 3, last: 5 });
        assert_eq!(m.times(), (Some(ts(1)), Some(ts(7))));
        assert!(m.may_hold_at(ts(1)) && !m.may_hold_at(ts(0)));
        assert!(m.may_hold_after(ts(7)) && !m.may_hold_after(ts(8)));
        let (_, k, _) = m.seek(&entry::internal_key(b"a", ts(3))).unwrap();
        assert_eq!(entry::split(k), (&b"a"[..], ts(2)));
        assert!(m.seek(&entry::after_all_versions(b"b")).is_none());
        let last_a = m.before(&entry::first_version(b"b")).map(|k| entry::split(k).0.to_vec());
        assert_eq!(last_a, Some(b"a".to_vec()));
    }

    #[test]
    fn replacing_a_version_keeps_the_count_of_bytes() {
        let m = Memtable::default();
        m.insert(GroupId(1), 1, b"k", ts(1), Kind::Put, b"1234");
        let one = m.bytes();
        m.insert(GroupId(1), 2, b"k", ts(1), Kind::Put, b"5678");
        assert_eq!(m.bytes(), one);
        m.insert_tombstone(
            GroupId(1),
            3,
            RangeTombstone { start: b"a".to_vec(), end: b"z".to_vec(), ts: ts(9) },
        );
        m.insert_tombstone(
            GroupId(1),
            4,
            RangeTombstone { start: b"b".to_vec(), end: b"c".to_vec(), ts: ts(8) },
        );
        assert!(m.bytes() > one);
        let order: Vec<Hlc> = m.tombstones().map(|t| t.ts).collect();
        assert_eq!(order, vec![ts(9), ts(8)], "in order of arrival");
        m.note_index(GroupId(4), 8);
        assert_eq!(m.applied_of(GroupId(4)), Some(Applied { first: 8, last: 8 }));
        assert!(!m.is_empty());
        assert!(Memtable::default().is_empty());
        assert!(!Memtable::default().may_hold_at(ts(u64::MAX - 1)));
    }
}
