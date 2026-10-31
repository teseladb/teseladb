//! View of a group's log: what is already in the store ([`LogStore`], which `teseladb-cluster`
//! implements over `CoreStore`) plus the unstable entries not yet handed to the caller to persist,
//! and the committed, persisted and applied indexes.

use crate::message::{Entry, Snapshot, SnapshotMeta};

/// The requested range is not in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogGap {
    /// Covered by the snapshot.
    Compacted,
    /// Beyond the last index.
    Unavailable,
}

/// Log store (entries since the snapshot, in RAM: `entries` never does I/O).
pub trait LogStore {
    /// Entries `[lo, hi)` up to `max_bytes` (at least one if there is any).
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<Vec<Entry>, LogGap>;
    /// Term of an entry (also the one at the snapshot's index).
    fn term(&self, index: u64) -> Result<u64, LogGap>;
    /// First available index (the snapshot's + 1).
    fn first_index(&self) -> u64;
    /// Last index (the snapshot's if there are no entries).
    fn last_index(&self) -> u64;
    /// Metadata of the latest snapshot.
    fn snapshot_meta(&self) -> SnapshotMeta;
}

/// In-memory store (tests and `teseladb dev`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemLogStore {
    /// Snapshot.
    pub snapshot: SnapshotMeta,
    /// Entries from `snapshot.index + 1`.
    pub entries: Vec<Entry>,
}

impl MemLogStore {
    /// Appends consecutive entries, discarding the suffix they overwrite.
    pub fn append(&mut self, new: &[Entry]) {
        let Some(first) = new.first() else { return };
        let keep = first.index.saturating_sub(self.snapshot.index + 1) as usize;
        self.entries.truncate(keep);
        self.entries.extend_from_slice(new);
    }

    /// Discards the entries from `from`.
    pub fn truncate(&mut self, from: u64) {
        let keep = from.saturating_sub(self.snapshot.index + 1) as usize;
        self.entries.truncate(keep);
    }

    /// Installs a snapshot (discards the whole log).
    pub fn apply_snapshot(&mut self, s: &Snapshot) {
        self.snapshot = s.meta.clone();
        self.entries.clear();
    }

    /// Compacts up to `index` (inclusive) with the given metadata.
    pub fn compact(&mut self, meta: SnapshotMeta) {
        let drop =
            (meta.index.saturating_sub(self.snapshot.index) as usize).min(self.entries.len());
        self.entries.drain(..drop);
        self.snapshot = meta;
    }
}

impl LogStore for MemLogStore {
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<Vec<Entry>, LogGap> {
        if lo <= self.snapshot.index {
            return Err(LogGap::Compacted);
        }
        if hi > self.last_index() + 1 {
            return Err(LogGap::Unavailable);
        }
        let start = (lo - self.snapshot.index - 1) as usize;
        let end = (hi - self.snapshot.index - 1) as usize;
        Ok(limit(&self.entries[start..end.max(start)], max_bytes))
    }

    fn term(&self, index: u64) -> Result<u64, LogGap> {
        if index == self.snapshot.index {
            return Ok(self.snapshot.term);
        }
        if index < self.snapshot.index {
            return Err(LogGap::Compacted);
        }
        self.entries
            .get((index - self.snapshot.index - 1) as usize)
            .map(|e| e.term)
            .ok_or(LogGap::Unavailable)
    }

    fn first_index(&self) -> u64 {
        self.snapshot.index + 1
    }

    fn last_index(&self) -> u64 {
        self.snapshot.index + self.entries.len() as u64
    }

    fn snapshot_meta(&self) -> SnapshotMeta {
        self.snapshot.clone()
    }
}

/// The first entries that fit in `max_bytes` (at least one).
pub(crate) fn limit(entries: &[Entry], max_bytes: u64) -> Vec<Entry> {
    let mut size = 0u64;
    let mut n = 0;
    for e in entries {
        size += e.size() as u64;
        if n > 0 && size > max_bytes {
            break;
        }
        n += 1;
    }
    entries[..n].to_vec()
}

/// View of the log inside a `RawNode`.
#[derive(Clone, Debug, Default)]
pub(crate) struct RaftLog {
    /// Entries not yet handed out to persist, or handed out and not yet confirmed (from
    /// `unstable_offset`).
    pub unstable: Vec<Entry>,
    /// Index of `unstable[0]`.
    pub unstable_offset: u64,
    /// Entries of `unstable` already handed out in a `Ready` (waiting for `on_persisted`).
    pub handed: usize,
    /// Received snapshot waiting to be handed out and installed.
    pub pending_snapshot: Option<Snapshot>,
    /// Snapshot already handed out but not yet confirmed as persisted.
    pub unstable_snapshot: Option<SnapshotMeta>,
    /// Last persisted index (confirmed with `on_persisted`).
    pub persisted: u64,
    /// Committed index.
    pub committed: u64,
    /// Last index handed out to apply.
    pub handed_apply: u64,
    /// Last applied index (`on_applied`).
    pub applied: u64,
    /// Truncation from this index waiting to be handed out.
    pub truncate_from: Option<u64>,
}

impl RaftLog {
    /// View over a freshly recovered store.
    pub fn new(store: &dyn LogStore, commit: u64) -> RaftLog {
        let last = store.last_index();
        let snap = store.snapshot_meta().index;
        RaftLog {
            unstable: Vec::new(),
            unstable_offset: last + 1,
            handed: 0,
            pending_snapshot: None,
            unstable_snapshot: None,
            persisted: last,
            committed: commit.clamp(snap, last),
            handed_apply: snap,
            applied: snap,
            truncate_from: None,
        }
    }

    fn snapshot_index(&self, store: &dyn LogStore) -> u64 {
        match (&self.pending_snapshot, &self.unstable_snapshot) {
            (Some(s), _) => s.meta.index,
            (None, Some(m)) => m.index,
            _ => store.snapshot_meta().index,
        }
    }

    pub fn first_index(&self, store: &dyn LogStore) -> u64 {
        match (&self.pending_snapshot, &self.unstable_snapshot) {
            (None, None) => store.first_index(),
            _ => self.snapshot_index(store) + 1,
        }
    }

    pub fn last_index(&self, store: &dyn LogStore) -> u64 {
        if let Some(last) = self.unstable.last() {
            return last.index;
        }
        match (&self.pending_snapshot, &self.unstable_snapshot) {
            (Some(s), _) => s.meta.index,
            (None, Some(m)) => m.index,
            _ => store.last_index().min(self.unstable_offset - 1).max(store.snapshot_meta().index),
        }
    }

    pub fn term(&self, store: &dyn LogStore, index: u64) -> Result<u64, LogGap> {
        if index >= self.unstable_offset {
            return self
                .unstable
                .get((index - self.unstable_offset) as usize)
                .map(|e| e.term)
                .ok_or(LogGap::Unavailable);
        }
        if let Some(s) = &self.pending_snapshot {
            return if index == s.meta.index { Ok(s.meta.term) } else { Err(LogGap::Compacted) };
        }
        // Every index after a snapshot not yet persisted is in the unstable part (answered above).
        if let Some(m) = &self.unstable_snapshot {
            return if index == m.index { Ok(m.term) } else { Err(LogGap::Compacted) };
        }
        store.term(index)
    }

    pub fn last_term(&self, store: &dyn LogStore) -> u64 {
        let last = self.last_index(store);
        self.term(store, last).unwrap_or(0)
    }

    /// Entries `[lo, hi)` from the store and the unstable part, up to `max_bytes`.
    pub fn entries(
        &self,
        store: &dyn LogStore,
        lo: u64,
        hi: u64,
        max_bytes: u64,
    ) -> Result<Vec<Entry>, LogGap> {
        if lo < self.first_index(store) {
            return Err(LogGap::Compacted);
        }
        if hi > self.last_index(store) + 1 {
            return Err(LogGap::Unavailable);
        }
        if lo >= hi {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if lo < self.unstable_offset {
            out = store.entries(lo, hi.min(self.unstable_offset), max_bytes)?;
            if (out.len() as u64) < hi.min(self.unstable_offset) - lo {
                return Ok(out);
            }
        }
        if hi > self.unstable_offset {
            let from = lo.max(self.unstable_offset);
            let s = (from - self.unstable_offset) as usize;
            let e = (hi - self.unstable_offset) as usize;
            let mut used: u64 = out.iter().map(|x| x.size() as u64).sum();
            let rest = &self.unstable[s..e];
            if out.is_empty() {
                out = limit(rest, max_bytes);
            } else {
                for x in rest {
                    used += x.size() as u64;
                    if used > max_bytes {
                        break;
                    }
                    out.push(x.clone());
                }
            }
        }
        Ok(out)
    }

    /// Tells whether a log ending at (`last_index`, `last_term`) is at least as up to date.
    pub fn is_up_to_date(&self, store: &dyn LogStore, last_index: u64, last_term: u64) -> bool {
        let my_term = self.last_term(store);
        last_term > my_term || (last_term == my_term && last_index >= self.last_index(store))
    }

    /// Tells whether the log contains `index` with term `term`.
    pub fn matches(&self, store: &dyn LogStore, index: u64, term: u64) -> bool {
        self.term(store, index) == Ok(term)
    }

    /// Appends entries at the end (leader).
    pub fn append(&mut self, store: &dyn LogStore, entries: &[Entry]) {
        let Some(first) = entries.first() else { return };
        let last = self.last_index(store);
        debug_assert_eq!(first.index, last + 1, "append without a gap");
        if self.unstable.is_empty() {
            self.unstable_offset = first.index;
            self.handed = 0;
        }
        self.unstable.extend_from_slice(entries);
    }

    /// Appends a leader's entries after checking `(index, log_term)`; discards the suffix they
    /// contradict. Returns the last new index or `None` if it does not match.
    pub fn maybe_append(
        &mut self,
        store: &dyn LogStore,
        index: u64,
        log_term: u64,
        entries: &[Entry],
    ) -> Option<u64> {
        if !self.matches(store, index, log_term) {
            return None;
        }
        let last_new = index + entries.len() as u64;
        // First entry that is missing or has another term.
        let conflict =
            entries.iter().find(|e| !self.matches(store, e.index, e.term)).map(|e| e.index);
        if let Some(c) = conflict {
            debug_assert!(c > self.committed, "what is committed is never contradicted");
            self.truncate_and_append(store, &entries[(c - entries[0].index) as usize..]);
        }
        Some(last_new)
    }

    fn truncate_and_append(&mut self, store: &dyn LogStore, entries: &[Entry]) {
        let first = entries[0].index;
        let last = self.last_index(store);
        if first > last {
            self.append(store, entries);
            return;
        }
        // A suffix is overwritten.
        if first >= self.unstable_offset {
            let keep = (first - self.unstable_offset) as usize;
            self.unstable.truncate(keep);
            self.handed = self.handed.min(keep);
        } else {
            // Persisted part: the caller has to truncate the store.
            self.unstable.clear();
            self.unstable_offset = first;
            self.handed = 0;
            self.truncate_from = Some(self.truncate_from.map_or(first, |t| t.min(first)));
        }
        if first <= self.persisted {
            self.persisted = first - 1;
        }
        self.unstable.extend_from_slice(entries);
    }

    /// Entries waiting to be handed out to persist.
    pub fn take_unpersisted(&mut self) -> Vec<Entry> {
        let out = self.unstable[self.handed..].to_vec();
        self.handed = self.unstable.len();
        out
    }

    /// The caller confirmed that what was handed out up to (`index`, `term`) is durable.
    pub fn stable_to(&mut self, store: &dyn LogStore, index: u64, term: u64) {
        if let Some(m) = &self.unstable_snapshot
            && index >= m.index
        {
            self.unstable_snapshot = None;
        }
        if self.term(store, index) != Ok(term) {
            // What was persisted is no longer what is there (another leader overwrote it meanwhile).
            return;
        }
        self.persisted = self.persisted.max(index);
        if index >= self.unstable_offset {
            let n = ((index - self.unstable_offset + 1) as usize).min(self.handed);
            self.unstable.drain(..n);
            self.handed -= n;
            self.unstable_offset += n as u64;
        }
    }

    pub fn commit_to(&mut self, index: u64) -> bool {
        if index > self.committed {
            self.committed = index;
            true
        } else {
            false
        }
    }

    /// Installs a received snapshot at an entry the durable log has, keeping the log past it: the
    /// state machine waited for a snapshot, and the log and the commit index stay (007 T016).
    pub fn restore_keeping_log(&mut self, s: Snapshot) {
        let idx = s.meta.index;
        debug_assert!(idx <= self.persisted, "the log past the snapshot is durable");
        self.committed = self.committed.max(idx);
        self.handed_apply = idx;
        self.applied = self.applied.max(idx);
        self.pending_snapshot = Some(s);
    }

    /// Installs a received snapshot (discards the whole log).
    pub fn restore(&mut self, s: Snapshot) {
        let idx = s.meta.index;
        self.unstable.clear();
        self.handed = 0;
        self.unstable_offset = idx + 1;
        self.committed = idx;
        self.persisted = self.persisted.min(idx);
        self.handed_apply = idx;
        self.applied = self.applied.max(idx);
        self.truncate_from = None;
        self.pending_snapshot = Some(s);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::message::EntryKind;
    use teseladb_types::Bytes;

    fn e(index: u64, term: u64) -> Entry {
        Entry { index, term, kind: EntryKind::Normal, data: Bytes::from_vec(vec![index as u8; 10]) }
    }

    fn store(terms: &[u64]) -> MemLogStore {
        let mut s = MemLogStore::default();
        let es: Vec<Entry> = terms.iter().enumerate().map(|(i, &t)| e(i as u64 + 1, t)).collect();
        s.append(&es);
        s
    }

    #[test]
    fn mem_store_ranges_and_compaction() {
        let mut s = store(&[1, 1, 2, 2, 3]);
        assert_eq!((s.first_index(), s.last_index()), (1, 5));
        assert_eq!(s.term(0), Ok(0));
        assert_eq!(s.term(6), Err(LogGap::Unavailable));
        assert_eq!(s.entries(2, 4, u64::MAX).unwrap().len(), 2);
        assert_eq!(s.entries(2, 4, 1).unwrap().len(), 1, "at least one");
        assert_eq!(s.entries(3, 7, u64::MAX), Err(LogGap::Unavailable));
        s.compact(SnapshotMeta { index: 3, term: 2, ..Default::default() });
        assert_eq!(s.term(3), Ok(2));
        assert_eq!(s.term(2), Err(LogGap::Compacted));
        assert_eq!(s.entries(3, 4, u64::MAX), Err(LogGap::Compacted));
        assert_eq!(s.entries(4, 6, u64::MAX).unwrap().len(), 2);
        s.truncate(5);
        assert_eq!(s.last_index(), 4);
        s.append(&[]);
        s.apply_snapshot(&Snapshot {
            meta: SnapshotMeta { index: 10, term: 4, ..Default::default() },
            data: Bytes::new(),
        });
        assert_eq!((s.first_index(), s.last_index(), s.term(10)), (11, 10, Ok(4)));
    }

    #[test]
    fn unstable_entries_overlay_the_store() {
        let s = store(&[1, 1, 2]);
        let mut l = RaftLog::new(&s, 2);
        assert_eq!((l.committed, l.persisted, l.last_index(&s)), (2, 3, 3));
        l.append(&s, &[e(4, 3), e(5, 3)]);
        assert_eq!(l.last_index(&s), 5);
        assert_eq!(l.term(&s, 5), Ok(3));
        assert_eq!(l.term(&s, 6), Err(LogGap::Unavailable));
        // Range spanning the store and the unstable part, with a byte limit.
        assert_eq!(l.entries(&s, 2, 6, u64::MAX).unwrap().len(), 4);
        assert_eq!(l.entries(&s, 3, 6, 40).unwrap().len(), 1);
        assert_eq!(l.entries(&s, 3, 6, 70).unwrap().len(), 2);
        assert_eq!(l.entries(&s, 4, 6, 1).unwrap().len(), 1);
        assert_eq!(l.entries(&s, 5, 5, u64::MAX).unwrap(), vec![]);
        assert_eq!(l.entries(&s, 1, 9, u64::MAX), Err(LogGap::Unavailable));
        assert!(
            l.is_up_to_date(&s, 5, 3) && l.is_up_to_date(&s, 1, 4) && !l.is_up_to_date(&s, 9, 2)
        );
        // Hand out and confirm persistence.
        assert_eq!(l.take_unpersisted().len(), 2);
        assert!(l.take_unpersisted().is_empty());
        l.stable_to(&s, 5, 99);
        assert_eq!(l.persisted, 3, "different term: not considered persisted");
        l.stable_to(&s, 4, 3);
        assert_eq!((l.persisted, l.unstable_offset), (4, 5));
        assert!(!l.commit_to(1) && l.commit_to(4));
    }

    #[test]
    fn conflicts_truncate_unstable_or_persisted_entries() {
        let s = store(&[1, 1, 2, 2]);
        let mut l = RaftLog::new(&s, 1);
        // Does not match.
        assert_eq!(l.maybe_append(&s, 4, 9, &[e(5, 9)]), None);
        // Same as what is there: nothing to change.
        assert_eq!(l.maybe_append(&s, 1, 1, &[e(2, 1), e(3, 2)]), Some(3));
        assert!(l.truncate_from.is_none());
        // Conflict in the persisted part: the store has to be truncated.
        assert_eq!(l.maybe_append(&s, 2, 1, &[e(3, 5), e(4, 5)]), Some(4));
        assert_eq!(l.truncate_from, Some(3));
        assert_eq!(l.persisted, 2);
        assert_eq!(l.term(&s, 4), Ok(5));
        // Conflict inside the unstable part, already partly handed out.
        l.take_unpersisted();
        assert_eq!(l.maybe_append(&s, 3, 5, &[e(4, 6)]), Some(4));
        assert_eq!(l.handed, 1);
        // Append without a conflict after the unstable part.
        assert_eq!(l.maybe_append(&s, 4, 6, &[e(5, 6)]), Some(5));
        assert_eq!(l.last_term(&s), 6);
        // A second conflict in the persisted part before the store was truncated: the store has to
        // be truncated from the lower of the two.
        assert_eq!(l.maybe_append(&s, 1, 1, &[e(2, 7)]), Some(2));
        assert_eq!(l.truncate_from, Some(2));
        // Appending nothing changes nothing.
        l.append(&s, &[]);
        assert_eq!(l.last_index(&s), 2);
    }

    #[test]
    fn restored_snapshot_hides_the_old_log() {
        let s = store(&[1, 1, 1]);
        let mut l = RaftLog::new(&s, 3);
        let snap = Snapshot {
            meta: SnapshotMeta { index: 10, term: 4, ..Default::default() },
            data: Bytes::new(),
        };
        l.restore(snap.clone());
        assert_eq!((l.first_index(&s), l.last_index(&s), l.committed), (11, 10, 10));
        assert_eq!(l.term(&s, 10), Ok(4));
        assert_eq!(l.term(&s, 5), Err(LogGap::Compacted));
        assert_eq!(l.entries(&s, 5, 6, u64::MAX), Err(LogGap::Compacted));
        // Handed out but not persisted: it still hides the store.
        l.unstable_snapshot = l.pending_snapshot.take().map(|s| s.meta);
        assert_eq!((l.first_index(&s), l.last_index(&s)), (11, 10));
        assert_eq!(l.term(&s, 10), Ok(4));
        assert_eq!(l.term(&s, 9), Err(LogGap::Compacted));
        let mut after = MemLogStore::default();
        after.apply_snapshot(&snap);
        // Persisting something below the snapshot does not persist the snapshot.
        l.stable_to(&after, 5, 1);
        assert!(l.unstable_snapshot.is_some());
        l.stable_to(&after, 10, 4);
        assert!(l.unstable_snapshot.is_none());
        assert_eq!(l.persisted, 10);
        assert_eq!(limit(&[], 10), vec![]);
    }
}
