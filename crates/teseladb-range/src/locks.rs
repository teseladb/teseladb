//! The locks a range's leader keeps in memory for the transactions that hold their writes until
//! the commit (008 FR-022, `spec/tla/TxnCommit.tla` with `Buffered`). A statement locks its keys
//! with `LockKeys`, without an entry in the log: another transaction that wants one of them fails
//! with `WriteConflict` and a write outside a transaction waits, as they do with an intent. The
//! locks are not replicated, so a change of leader loses them, and those of a transaction with no
//! activity for [`ABANDONED_AFTER_MS`] stop counting: the check of the transaction's `OnePhase`
//! finds then the conflict the lock no longer stops.

use std::collections::{BTreeMap, HashMap};

use crate::intents::{ABANDONED_AFTER_MS, TxnId};

/// A transaction that holds locks.
#[derive(Debug)]
struct Owner {
    keys: Vec<Vec<u8>>,
    /// Cluster time of its last request, in milliseconds.
    last_ms: u64,
}

/// The locks of one range on its leader.
#[derive(Debug, Default)]
pub struct LockTable {
    keys: BTreeMap<Vec<u8>, TxnId>,
    owners: HashMap<TxnId, Owner>,
}

impl LockTable {
    /// Whether `txn` is still active at `now_ms`.
    fn live(&self, txn: TxnId, now_ms: u64) -> bool {
        self.owners.get(&txn).is_some_and(|o| now_ms.saturating_sub(o.last_ms) < ABANDONED_AFTER_MS)
    }

    /// The live transaction other than `txn` that holds one of `keys` at `now_ms`, if any
    /// (`txn` `None`: any holder).
    pub fn holder<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a Vec<u8>>,
        txn: Option<TxnId>,
        now_ms: u64,
    ) -> Option<TxnId> {
        if self.keys.is_empty() {
            return None;
        }
        keys.into_iter()
            .filter_map(|k| self.keys.get(k).copied())
            .find(|t| Some(*t) != txn && self.live(*t, now_ms))
    }

    /// Locks `keys` for `txn` at `now_ms` and counts it as active; a key a transaction that is no
    /// longer live held passes to `txn`. The caller checked that no live transaction holds them.
    pub fn lock(&mut self, txn: TxnId, keys: &[Vec<u8>], now_ms: u64) {
        self.expire(now_ms);
        for k in keys {
            if let Some(old) = self.keys.insert(k.clone(), txn)
                && old != txn
                && let Some(o) = self.owners.get_mut(&old)
            {
                o.keys.retain(|x| x != k);
            }
        }
        let owner =
            self.owners.entry(txn).or_insert_with(|| Owner { keys: Vec::new(), last_ms: 0 });
        owner.last_ms = owner.last_ms.max(now_ms);
        for k in keys {
            if !owner.keys.contains(k) {
                owner.keys.push(k.clone());
            }
        }
    }

    /// Lets go of every lock of `txn`.
    pub fn release(&mut self, txn: TxnId) {
        let Some(owner) = self.owners.remove(&txn) else { return };
        for k in owner.keys {
            if self.keys.get(&k) == Some(&txn) {
                self.keys.remove(&k);
            }
        }
    }

    /// Drops the transactions that have been idle for [`ABANDONED_AFTER_MS`] at `now_ms`.
    fn expire(&mut self, now_ms: u64) {
        let gone: Vec<TxnId> = self
            .owners
            .iter()
            .filter(|(_, o)| now_ms.saturating_sub(o.last_ms) >= ABANDONED_AFTER_MS)
            .map(|(t, _)| *t)
            .collect();
        for t in gone {
            self.release(t);
        }
    }

    /// How many keys are locked.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is locked.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn another_live_transaction_finds_the_key_held_and_its_owner_does_not() {
        let mut t = LockTable::default();
        let (a, b) = (TxnId(1), TxnId(2));
        t.lock(a, &[k("x"), k("y")], 1_000);
        assert_eq!(t.holder(&[k("y")], Some(b), 1_500), Some(a));
        assert_eq!(t.holder(&[k("y")], None, 1_500), Some(a));
        assert_eq!(t.holder(&[k("x"), k("y")], Some(a), 1_500), None);
        assert_eq!(t.holder(&[k("z")], Some(b), 1_500), None);
    }

    #[test]
    fn the_locks_of_an_idle_transaction_stop_counting_and_go_with_the_next_lock() {
        let mut t = LockTable::default();
        let (a, b) = (TxnId(1), TxnId(2));
        t.lock(a, &[k("x")], 1_000);
        let late = 1_000 + ABANDONED_AFTER_MS;
        assert_eq!(t.holder(&[k("x")], Some(b), late - 1), Some(a));
        assert_eq!(t.holder(&[k("x")], Some(b), late), None);
        t.lock(b, &[k("x")], late);
        assert_eq!(t.holder(&[k("x")], Some(a), late), Some(b));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn a_new_request_keeps_the_transaction_active() {
        let mut t = LockTable::default();
        let a = TxnId(1);
        t.lock(a, &[k("x")], 1_000);
        t.lock(a, &[], 4_000);
        assert_eq!(t.holder(&[k("x")], Some(TxnId(2)), 1_000 + ABANDONED_AFTER_MS), Some(a));
    }

    #[test]
    fn a_release_frees_only_the_keys_of_its_transaction() {
        let mut t = LockTable::default();
        let (a, b) = (TxnId(1), TxnId(2));
        t.lock(a, &[k("x")], 1_000);
        t.lock(b, &[k("y")], 1_000);
        t.release(a);
        assert_eq!(t.holder(&[k("x")], None, 1_000), None);
        assert_eq!(t.holder(&[k("y")], None, 1_000), Some(b));
        t.release(b);
        t.release(b);
        assert!(t.is_empty());
    }
}
