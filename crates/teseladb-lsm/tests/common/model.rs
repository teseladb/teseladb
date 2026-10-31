//! The LSM's model (004 T002): an ordered map of versions and a list of range tombstones, with
//! the reading rules of FR-005 to FR-008 written the plain way. A resolved intent is simply
//! removed, where the LSM writes a `Retract` over it.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_lsm::{Kind, RangeTombstone};

/// A version as reads return it: key, timestamp, kind and value.
pub type Row = (Vec<u8>, Hlc, Kind, Vec<u8>);

/// The model.
#[derive(Clone, Debug, Default)]
pub struct Model {
    versions: BTreeMap<(Vec<u8>, Reverse<Hlc>), (Kind, Vec<u8>)>,
    tombstones: Vec<RangeTombstone>,
}

impl Model {
    /// Writes a version (one at the same key and timestamp is replaced).
    pub fn write(&mut self, key: &[u8], ts: Hlc, kind: Kind, value: &[u8]) {
        self.versions.insert((key.to_vec(), Reverse(ts)), (kind, value.to_vec()));
    }

    /// Resolves the intent of `key` at `intent_ts`: it goes away, and the commit is written.
    pub fn resolve(&mut self, key: &[u8], intent_ts: Hlc, commit: Option<(Hlc, Kind, Vec<u8>)>) {
        let at = (key.to_vec(), Reverse(intent_ts));
        assert_eq!(self.versions.get(&at).map(|v| v.0), Some(Kind::Intent), "resolving no intent");
        self.versions.remove(&at);
        if let Some((ts, kind, value)) = commit {
            self.write(key, ts, kind, &value);
        }
    }

    /// Deletes `[start, end)` at `ts`.
    pub fn delete_range(&mut self, start: &[u8], end: &[u8], ts: Hlc) {
        self.tombstones.push(RangeTombstone { start: start.to_vec(), end: end.to_vec(), ts });
    }

    /// The pending intents.
    pub fn intents(&self) -> Vec<(Vec<u8>, Hlc)> {
        self.versions
            .iter()
            .filter(|(_, (kind, _))| *kind == Kind::Intent)
            .map(|((k, Reverse(ts)), _)| (k.clone(), *ts))
            .collect()
    }

    /// What a read of `key` at `ts` sees: its newest version up to `ts`, unless that is a
    /// deletion or a range tombstone hides it.
    pub fn get(&self, key: &[u8], ts: Hlc) -> Option<Row> {
        let from = (key.to_vec(), Reverse(ts));
        let ((k, Reverse(vts)), (kind, value)) = self.versions.range(from..).next()?;
        if k != key || *kind == Kind::Delete {
            return None;
        }
        if self.tombstones.iter().any(|t| t.hides(key, *vts, ts)) {
            return None;
        }
        Some((k.clone(), *vts, *kind, value.clone()))
    }

    /// What a scan of `[start, end)` at `ts` returns, key by key.
    pub fn scan(&self, start: &[u8], end: &[u8], ts: Hlc) -> Vec<Row> {
        let mut keys: Vec<&Vec<u8>> = self
            .versions
            .keys()
            .map(|(k, _)| k)
            .filter(|k| start <= k.as_slice() && k.as_slice() < end)
            .collect();
        keys.dedup();
        keys.into_iter().filter_map(|k| self.get(k, ts)).collect()
    }

    /// Every version of `[start, end)` with a timestamp in `(from, to]`, in key order and from the
    /// newest, deletions included.
    pub fn between(&self, start: &[u8], end: &[u8], from: Hlc, to: Hlc) -> Vec<Row> {
        self.versions
            .iter()
            .filter(|((k, Reverse(ts)), _)| {
                start <= k.as_slice() && k.as_slice() < end && from < *ts && *ts <= to
            })
            .map(|((k, Reverse(ts)), (kind, value))| (k.clone(), *ts, *kind, value.clone()))
            .collect()
    }

    /// Every logical key.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = self.versions.keys().map(|(k, _)| k.clone()).collect();
        keys.dedup();
        keys
    }
}
