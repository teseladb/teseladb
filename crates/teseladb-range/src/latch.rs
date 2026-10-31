//! Latches (FR-007): a write holds its keys (document and index entries) exclusively from its
//! evaluation to its application. It takes them before its mark exists and receives the mark when
//! it is proposed. A read at t waits for the latches of the keys it covers with a mark at or below
//! t, so no version shows up later below a mark already read; a `local` or `available` read waits
//! for every latch of its keys (DV-010). A latch without a mark yet does not hold a read back: the
//! read enters the read cache when it checks the latches, so that write is proposed above it.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;

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
    next: u64,
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

    /// Releases a write's latches once it is applied (or abandoned).
    pub fn release(&mut self, id: LatchId) {
        self.by_key.retain(|_, (_, holder)| *holder != id);
    }

    /// Whether `key` is held.
    pub fn is_held(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Whether no latch is held.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// What a read of `[start, end)` at `ts` waits for; `all` for `local` and `available` reads.
    pub fn read_wait(&self, start: &[u8], end: &[u8], ts: Hlc, all: bool) -> ReadWait {
        self.by_key
            .range(start.to_vec()..end.to_vec())
            .find(|(_, (t, _))| all || t.is_some_and(|t| t <= ts))
            .map_or(ReadWait::Free, |(_, (_, id))| ReadWait::On(*id))
    }
}
