//! Latches (FR-007): a write holds its keys (document and index entries) exclusively from its
//! evaluation to its application. It takes them before its mark exists and receives the mark when
//! it is proposed. A read at t waits for the latches of the keys it covers with a mark at or below
//! t, so no version shows up later below a mark already read; a `local` or `available` read waits
//! for every latch of its keys (DV-010), unless it chose its own mark and no write of its keys was
//! answered before being applied (`w: 1` and `w: <n>`, D-034): then it reads just below the
//! lowest mark in flight, where everything is applied, as a RefDb primary reads past a write it
//! has not applied yet. A latch without a mark yet does not hold a read back: the read enters the
//! read cache when it checks the latches, so that write is proposed above it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use teseladb_hlc::Hlc;

/// Which latches a read waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitFor {
    /// Those of writes proposed at or below its mark (`majority`, `linearizable`).
    AtOrBelow,
    /// Every one: a `local` or `available` read at a mark it was given (DV-010), and a split or
    /// a merge, which copies what the range holds with the idempotence records of the writes in
    /// flight.
    Every,
}

/// A held set of latches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LatchId(pub u64);

/// Whether a read may go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadWait {
    /// No latch it has to wait for.
    Free,
    /// It waits for this write to be applied.
    On(LatchId),
}

/// The latches of a range's leader.
#[derive(Clone, Debug, Default)]
pub struct Latches {
    by_key: BTreeMap<Vec<u8>, (Option<Hlc>, LatchId)>,
    /// The writes already answered to their client while their latches are held.
    answered: BTreeSet<LatchId>,
    next: u64,
    /// The writes holding latches, and how many they are for the reads outside the core (005
    /// FR-028): a write is counted when it takes its first latch, before it gets a mark, and a
    /// read outside the core that sees any goes to the core.
    holders: BTreeSet<LatchId>,
    held: Arc<AtomicU32>,
}

impl Latches {
    /// Takes `keys` for a write; if one is held, the holder it has to wait for.
    pub fn acquire(&mut self, keys: &[Vec<u8>]) -> Result<LatchId, LatchId> {
        self.next += 1;
        let id = LatchId(self.next);
        self.extend(id, keys)?;
        Ok(id)
    }

    /// Adds `keys` to the latches of `id` (the index entries its evaluation found); if one is
    /// held by another write, nothing is taken and that write is returned.
    pub fn extend(&mut self, id: LatchId, keys: &[Vec<u8>]) -> Result<(), LatchId> {
        let other = keys.iter().filter_map(|k| self.by_key.get(k)).find(|(_, h)| *h != id);
        // Faulty variant (005 T017): writes never wait for each other, so two increments of one
        // document read the same version.
        #[cfg(teseladb_bug = "no_latch_inc")]
        let other: Option<&(Option<Hlc>, LatchId)> = other.filter(|_| false);
        if let Some((_, holder)) = other {
            return Err(*holder);
        }
        let ts = self.by_key.values().find(|(_, h)| *h == id).and_then(|(t, _)| *t);
        for k in keys {
            self.by_key.insert(k.clone(), (ts, id));
        }
        if !keys.is_empty() && self.holders.insert(id) {
            self.held
                .store(u32::try_from(self.holders.len()).unwrap_or(u32::MAX), Ordering::SeqCst);
        }
        Ok(())
    }

    /// The write `id` was proposed at `ts`.
    pub fn set_mark(&mut self, id: LatchId, ts: Hlc) {
        for (t, holder) in self.by_key.values_mut() {
            if *holder == id {
                *t = Some(ts);
            }
        }
    }

    /// The write `id` was answered to its client before it was applied.
    pub fn mark_answered(&mut self, id: LatchId) {
        if self.by_key.values().any(|(_, h)| *h == id) {
            self.answered.insert(id);
        }
    }

    /// Releases a write's latches once it is applied (or abandoned).
    pub fn release(&mut self, id: LatchId) {
        self.by_key.retain(|_, (_, holder)| *holder != id);
        self.answered.remove(&id);
        if self.holders.remove(&id) {
            self.held
                .store(u32::try_from(self.holders.len()).unwrap_or(u32::MAX), Ordering::SeqCst);
        }
    }

    /// How many writes hold latches, for the reads outside the core (005 FR-028).
    pub fn held(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.held)
    }

    /// Whether `key` is held.
    pub fn is_held(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Whether no latch is held.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// What a read of `[start, end)` at `ts` waits for, among the latches `which` names.
    pub fn read_wait(&self, start: &[u8], end: &[u8], ts: Hlc, which: WaitFor) -> ReadWait {
        self.by_key
            .range(start.to_vec()..end.to_vec())
            .find(|(_, (t, _))| match which {
                WaitFor::AtOrBelow => t.is_some_and(|t| t <= ts),
                WaitFor::Every => true,
            })
            .map_or(ReadWait::Free, |(_, (_, id))| ReadWait::On(*id))
    }

    /// The mark at which a `local` or `available` read of `[start, end)` that chose its own mark
    /// `ts` reads without waiting: just below the lowest mark of a write in flight there, or `ts`.
    /// A range applies its writes in the order of its log, which is the order of their marks, so
    /// below the lowest one in flight every write is applied. `None` when a write of those keys
    /// was answered before it was applied: the read waits for it, so it sees it (DV-010).
    pub fn own_read_mark(&self, start: &[u8], end: &[u8], ts: Hlc) -> Option<Hlc> {
        let held = || self.by_key.range(start.to_vec()..end.to_vec());
        if held().any(|(_, (_, id))| self.answered.contains(id)) {
            return None;
        }
        let lowest = held().filter_map(|(_, (t, _))| *t).filter(|t| *t <= ts).min();
        Some(lowest.map_or(ts, crate::intents::pred))
    }
}
