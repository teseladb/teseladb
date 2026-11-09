//! The write path of [`Lsm`]: puts, deletions, entries, intents and their resolution, and range
//! tombstones go into the active memtable, which freezes once it holds `memtable_bytes`.

use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_types::GroupId;

use super::{CLEARED, Commit, EntryWrite, Lsm, Pressure, VersionWrite};
use crate::entry::{Kind, RangeTombstone};
use crate::error::LsmError;

impl Lsm {
    fn write(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        kind: Kind,
        value: &[u8],
    ) -> Result<(), LsmError> {
        self.writable()?;
        self.insert(range, index, key, ts, kind, value);
        self.freeze_if_full();
        Ok(())
    }

    /// Adds a version to the active memtable without freezing it: an entry that writes several
    /// versions keeps them in one memtable, so a flush never holds half of an entry.
    fn insert(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        kind: Kind,
        value: &[u8],
    ) {
        self.note_pressure();
        self.version.memtables[0].insert(range, index, key, ts, kind, value);
        self.counters.user_bytes += (key.len() + value.len()) as u64;
    }

    fn freeze_if_full(&mut self) {
        if self.version.memtables[0].bytes() >= self.cfg.memtable_bytes {
            self.freeze();
        }
    }

    fn note_pressure(&mut self) {
        match self.pressure() {
            Pressure::Normal => {}
            Pressure::Slow => self.counters.stalls += 1,
            Pressure::Reject => self.counters.rejects += 1,
        }
    }

    /// Writes `value` at `key` and `ts` (entry `index` of `range`).
    pub fn put(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        value: &[u8],
    ) -> Result<(), LsmError> {
        self.write(range, index, key, ts, Kind::Put, value)
    }

    /// Deletes `key` at `ts`.
    pub fn delete(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
    ) -> Result<(), LsmError> {
        self.write(range, index, key, ts, Kind::Delete, &[])
    }

    /// Writes every version of entry `index` at `ts` into one memtable, so a flush holds all of
    /// the entry or none of it (005 FR-002).
    pub fn write_entry(
        &mut self,
        range: GroupId,
        index: u64,
        ts: Hlc,
        writes: &[EntryWrite<'_>],
    ) -> Result<(), LsmError> {
        assert!(
            writes.iter().all(|w| matches!(w.kind, Kind::Put | Kind::Delete)),
            "an entry writes values or deletions"
        );
        self.writable()?;
        for w in writes {
            self.insert(range, index, w.key, ts, w.kind, w.value);
        }
        self.freeze_if_full();
        Ok(())
    }

    /// Writes every version of entry `index`, each at its own timestamp, into one memtable (008:
    /// intents, the `Retract` of an intent and its commit in one entry).
    pub fn write_versions(
        &mut self,
        range: GroupId,
        index: u64,
        versions: &[VersionWrite<'_>],
    ) -> Result<(), LsmError> {
        self.writable()?;
        for v in versions {
            self.insert(range, index, v.key, v.ts, v.kind, v.value);
        }
        self.freeze_if_full();
        Ok(())
    }

    /// Writes an intent of 008 at `key` and `ts`; the LSM does not read `payload` (FR-006).
    pub fn put_intent(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        ts: Hlc,
        payload: &[u8],
    ) -> Result<(), LsmError> {
        self.write(range, index, key, ts, Kind::Intent, payload)
    }

    /// Resolves the intent of `key` at `intent_ts`: it goes away and, if the transaction
    /// committed, `commit` is written (FR-007). The same entry gives the same result on every
    /// replica.
    pub fn resolve_intent(
        &mut self,
        range: GroupId,
        index: u64,
        key: &[u8],
        intent_ts: Hlc,
        commit: Option<Commit<'_>>,
    ) -> Result<(), LsmError> {
        if let Some(c) = commit {
            assert!(
                matches!(c.kind, Kind::Put | Kind::Delete),
                "a commit writes a value or a deletion"
            );
            if c.ts == intent_ts {
                return self.write(range, index, key, c.ts, c.kind, c.value);
            }
        }
        self.writable()?;
        self.insert(range, index, key, intent_ts, Kind::Retract, &[]);
        if let Some(c) = commit {
            self.insert(range, index, key, c.ts, c.kind, c.value);
        }
        self.freeze_if_full();
        Ok(())
    }

    /// Deletes every version of `[start, end)` up to `ts` (FR-008).
    pub fn delete_range(
        &mut self,
        range: GroupId,
        index: u64,
        start: &[u8],
        end: &[u8],
        ts: Hlc,
    ) -> Result<(), LsmError> {
        self.writable()?;
        let m = Arc::clone(&self.version.memtables[0]);
        if start < end {
            m.insert_tombstone(
                range,
                index,
                RangeTombstone { start: start.to_vec(), end: end.to_vec(), ts },
            );
        } else {
            m.note_index(range, index);
        }
        let full = m.bytes() >= self.cfg.memtable_bytes;
        drop(m);
        if full {
            self.freeze();
        }
        Ok(())
    }

    /// A tombstone of a clearing: index 0, so its flush never raises the range's flushed index.
    pub(super) fn insert_tombstone(&mut self, range: GroupId, start: &[u8], end: &[u8]) {
        let t = RangeTombstone { start: start.to_vec(), end: end.to_vec(), ts: CLEARED };
        self.version.memtables[0].insert_tombstone(range, 0, t);
    }
}
