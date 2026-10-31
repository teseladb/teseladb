//! In-memory log of a Raft group (data-model §8, `GroupLog`): entries since the latest snapshot,
//! hard state and configuration. `entries()` never touches disk (guarantee 4).

use teseladb_types::{Bytes, GroupId};

use crate::format::wal::EntryKind;

/// Entry of a group's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// Index.
    pub index: u64,
    /// Term.
    pub term: u64,
    /// Type.
    pub kind: EntryKind,
    /// Data.
    pub data: Bytes,
}

/// Hard state of a group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardState {
    /// Current term.
    pub term: u64,
    /// Vote in the current term (0 = none).
    pub vote: u64,
    /// Known commit index.
    pub commit: u64,
}

/// Metadata of a group snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// Index it reflects.
    pub index: u64,
    /// Term of that index.
    pub term: u64,
    /// Group configuration at that index.
    pub conf: Bytes,
}

/// The requested range is not entirely in the in-memory tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogGap {
    /// The requested entries are already covered by the snapshot (`first` is the first available).
    Compacted {
        /// First available entry.
        first: u64,
    },
    /// Entries were requested beyond the last one.
    Unavailable {
        /// Last available entry.
        last: u64,
    },
    /// The group does not exist in this core.
    NoGroup,
}

/// In-memory log of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupLog {
    /// Group.
    pub id: GroupId,
    /// Configuration (the one at creation or the one of the latest snapshot).
    pub conf: Bytes,
    /// Hard state.
    pub hard: HardState,
    /// Index of the latest snapshot.
    pub snapshot_index: u64,
    /// Term of the latest snapshot.
    pub snapshot_term: u64,
    /// Last `TRUNCATE` applied (0 = none).
    pub last_truncate: u64,
    entries: Vec<LogEntry>,
}

impl GroupLog {
    /// Empty log of a newly created group.
    pub fn new(id: GroupId, conf: Bytes) -> GroupLog {
        GroupLog {
            id,
            conf,
            hard: HardState::default(),
            snapshot_index: 0,
            snapshot_term: 0,
            last_truncate: 0,
            entries: Vec::new(),
        }
    }

    /// Last index (the snapshot's if there are no entries).
    pub fn last_index(&self) -> u64 {
        self.entries.last().map_or(self.snapshot_index, |e| e.index)
    }

    /// First index available in RAM.
    pub fn first_index(&self) -> u64 {
        self.snapshot_index + 1
    }

    /// Term of an index, if known.
    pub fn term(&self, index: u64) -> Option<u64> {
        if index == self.snapshot_index {
            return Some(self.snapshot_term);
        }
        let pos = index.checked_sub(self.first_index())? as usize;
        self.entries.get(pos).map(|e| e.term)
    }

    /// Appends consecutive entries. If the first is ≤ the current last, the suffix from it is
    /// discarded first (returning `true` in that case, so that a `TRUNCATE` is recorded). Error if it
    /// would leave a gap or touch entries covered by the snapshot.
    pub fn append(&mut self, new: &[LogEntry]) -> Result<bool, &'static str> {
        let Some(first) = new.first() else { return Ok(false) };
        if new.windows(2).any(|w| w[1].index != w[0].index + 1) {
            return Err("non-consecutive entries");
        }
        if first.index <= self.snapshot_index {
            return Err("entries covered by the snapshot");
        }
        if first.index > self.last_index() + 1 {
            return Err("gap in the log");
        }
        let truncated = first.index <= self.last_index();
        if truncated {
            self.truncate(first.index);
        }
        self.entries.extend(new.iter().cloned());
        Ok(truncated)
    }

    /// Discards the entries with index ≥ `from`.
    pub fn truncate(&mut self, from: u64) {
        let keep = from.saturating_sub(self.first_index()) as usize;
        if keep < self.entries.len() {
            self.entries.truncate(keep);
        }
        self.last_truncate = from;
    }

    /// Entries ≤ `index` become covered by a snapshot.
    pub fn compact(&mut self, index: u64, term: u64) {
        if index <= self.snapshot_index {
            return;
        }
        let drop = (index - self.snapshot_index) as usize;
        if drop >= self.entries.len() {
            self.entries.clear();
        } else {
            self.entries.drain(..drop);
        }
        self.snapshot_index = index;
        self.snapshot_term = term;
    }

    /// Entries `[lo, hi)` up to `max_bytes` of data (at least one if there is any).
    pub fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<&[LogEntry], LogGap> {
        if lo < self.first_index() {
            return Err(LogGap::Compacted { first: self.first_index() });
        }
        if hi > self.last_index() + 1 {
            return Err(LogGap::Unavailable { last: self.last_index() });
        }
        if lo >= hi {
            return Ok(&[]);
        }
        let start = (lo - self.first_index()) as usize;
        let end = (hi - self.first_index()) as usize;
        let mut size = 0u64;
        let mut n = 0;
        for e in &self.entries[start..end] {
            size += e.data.len() as u64;
            if n > 0 && size > max_bytes {
                break;
            }
            n += 1;
        }
        Ok(&self.entries[start..start + n])
    }

    /// Bytes of data in RAM (for metrics and budgets).
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.data.len() as u64).sum()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn e(index: u64, term: u64) -> LogEntry {
        LogEntry { index, term, kind: EntryKind::Normal, data: Bytes::from_vec(vec![0; 10]) }
    }

    #[test]
    fn append_truncate_and_terms() {
        let mut g = GroupLog::new(GroupId(1), Bytes::from("c"));
        assert_eq!(g.last_index(), 0);
        assert_eq!(g.term(0), Some(0));
        assert_eq!(g.append(&[]), Ok(false));
        assert_eq!(g.append(&[e(1, 1), e(2, 1), e(3, 2)]), Ok(false));
        assert_eq!(g.term(3), Some(2));
        assert_eq!(g.term(4), None);
        // Overwriting a suffix.
        assert_eq!(g.append(&[e(3, 3), e(4, 3)]), Ok(true));
        assert_eq!(g.term(3), Some(3));
        assert_eq!(g.last_index(), 4);
        assert_eq!(g.last_truncate, 3);
        assert_eq!(g.append(&[e(6, 3)]), Err("gap in the log"));
        assert_eq!(g.append(&[e(5, 3), e(7, 3)]), Err("non-consecutive entries"));
        g.truncate(2);
        assert_eq!(g.last_index(), 1);
        g.truncate(10);
        assert_eq!(g.last_index(), 1);
        assert_eq!(g.bytes(), 10);
    }

    #[test]
    fn compaction_and_ranges() {
        let mut g = GroupLog::new(GroupId(1), Bytes::new());
        g.append(&(1..=10).map(|i| e(i, 1)).collect::<Vec<_>>()).unwrap();
        g.compact(4, 1);
        assert_eq!(g.first_index(), 5);
        assert_eq!(g.term(4), Some(1));
        assert_eq!(g.term(3), None);
        assert_eq!(g.entries(3, 6, u64::MAX), Err(LogGap::Compacted { first: 5 }));
        assert_eq!(g.entries(5, 12, u64::MAX), Err(LogGap::Unavailable { last: 10 }));
        assert_eq!(g.entries(5, 11, u64::MAX).unwrap().len(), 6);
        assert_eq!(g.entries(5, 11, 25).unwrap().len(), 2);
        assert_eq!(g.entries(5, 11, 0).unwrap().len(), 1, "at least one");
        assert!(g.entries(7, 7, 100).unwrap().is_empty());
        assert_eq!(g.append(&[e(3, 1)]), Err("entries covered by the snapshot"));
        g.compact(2, 1);
        assert_eq!(g.first_index(), 5, "compacting backwards does nothing");
        g.compact(20, 5);
        assert_eq!(g.last_index(), 20);
        assert_eq!(g.term(20), Some(5));
    }
}
