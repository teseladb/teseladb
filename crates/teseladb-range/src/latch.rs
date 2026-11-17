//! Latches (FR-007): a write holds its keys (document and index entries) exclusively from its
//! evaluation to its application. It takes them before its mark exists and receives the mark when
//! it is proposed. A read at t waits for the latches of the keys it covers with a mark at or below
//! t, so no version shows up later below a mark already read; a `local` or `available` read waits
//! for every latch of its keys (DV-010), unless it chose its own mark and no write of its keys was
//! answered before being applied (`w: 1` and `w: <n>`, D-034): then it reads just below the lowest
//! mark in flight, where everything is applied, as a reference server primary reads past a write it
//! has not applied yet. A latch without a mark yet does not hold a read back: the read enters the
//! read cache when it checks the latches, so that write is proposed above it.
//!
//! A write without a transaction gives its latches up to the next write of its keys once its
//! entry is in the leader's log (FR-029): they become pipelined, still held for every read and for
//! the transactions, and the next plain write takes the keys and is evaluated on the pending
//! value. A retry of the same statement still waits: the idempotence record of a pipelined write
//! stays exclusive for the writes too.
//!
//! For the point reads served outside the core (005 FR-028) a write also counts each key it
//! latches in [`KeySlots`], a counting filter shared with the range's view, and uncounts it when
//! it releases them. Every key a write puts or deletes is latched by its evaluation before its
//! mark is compared with the bound of those reads, so a read that finds the slot of its key at
//! zero after raising the bound either sees the write or makes it take a mark above
//! (`spec/tla/HlcKeyLatches.tla`, which fails when a write counts a key after its mark).
//!
//! The same keys go, in order, to [`SpanKeys`] for the scans served outside the core. A scan
//! knows which keys it read only once it has read them, so it checks after: it raises the bound,
//! notes how many writes the range has released, reads, and keeps what it read only if no key of
//! it is latched and no write was released meanwhile; a write released between its read and its
//! check would hide from both (H-049 of the native tests, `spec/tla/HlcKeyLatches.tla`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use teseladb_lsm::Published;

use teseladb_hlc::Hlc;

/// A latched key. The latches hold one copy of each key, which every map of them shares: a key
/// may be an index entry of many MiB (an index on a long string), and the maps of the exclusive
/// latches, the counted keys, the keys in order and the last releases each held their own copy.
type Key = Arc<[u8]>;

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

/// How many slots [`KeySlots`] has: with a few dozen writes in flight in a range, a read finds the
/// slot of its key taken by another key about one time in a hundred, and then goes to the core.
pub const KEY_SLOTS: usize = 4096;

/// A counting filter of the keys latched in a range (Fan et al., «Summary Cache», 2000): each
/// key counts in one slot, chosen by its hash, so a slot at zero means no write holds that key.
/// Two keys may share a slot: a read of one then sees the other's write and goes to the core.
#[derive(Debug)]
pub struct KeySlots {
    slots: Box<[AtomicU32]>,
}

impl Default for KeySlots {
    fn default() -> KeySlots {
        KeySlots { slots: (0..KEY_SLOTS).map(|_| AtomicU32::new(0)).collect() }
    }
}

impl KeySlots {
    /// The slot of `key` (FNV-1a over its bytes).
    pub fn slot(key: &[u8]) -> usize {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0100_0000_01b3;
        let h = key.iter().fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME));
        // The slot count is a power of two: the low bits after folding in the high ones.
        usize::try_from((h ^ (h >> 32)) & (KEY_SLOTS as u64 - 1)).unwrap_or(0)
    }

    /// Whether a write may hold `key`: its slot is not at zero.
    pub fn maybe_held(&self, key: &[u8]) -> bool {
        self.slots[Self::slot(key)].load(Ordering::SeqCst) != 0
    }

    fn count(&self, slot: usize) {
        self.slots[slot].fetch_add(1, Ordering::SeqCst);
    }

    fn uncount(&self, slot: usize) {
        self.slots[slot].fetch_sub(1, Ordering::SeqCst);
    }
}

/// The keys latched in a range, in order, for the scans outside the core (005 FR-028): the core
/// publishes them whenever the set changes, before the write that added a key compares its mark
/// with the bound of those reads, and a scan raises the bound before it loads them. Both steps
/// are sequentially consistent, so either the scan loads a set with the key or the write sees its
/// bound (`spec/tla/HlcKeyLatches.tla`).
pub struct SpanKeys {
    published: Published<Vec<Key>>,
    /// The keys, first and last, of the last writes released, each with its number in the count
    /// of releases: published before the count goes up, so a scan that sees a release counted
    /// finds it here, and goes to the core only if one of them was in what it read.
    recent: Published<Released>,
}

/// The last [`RECENT_RELEASES`] writes released with counted keys: number, and a key at or
/// below their first key and one at or above their last, each of at most
/// [`crate::load::SAMPLE_KEY`] bytes (whole keys of many MiB, kept for 256 writes, held them all).
#[derive(Debug, Default)]
pub struct Released {
    writes: VecDeque<(u64, Arc<[u8]>, Arc<[u8]>)>,
}

/// Writes released that [`SpanKeys`] keeps for the scans outside the core.
pub const RECENT_RELEASES: usize = 256;

impl Released {
    /// Bytes of the keys it keeps.
    pub fn key_bytes(&self) -> usize {
        self.writes.iter().map(|(_, f, l)| f.len() + l.len()).sum()
    }

    /// Whether a write numbered in `(after, upto]` had a key in `[start, end)`; `None` when it
    /// does not keep all of them.
    pub fn any_in(&self, (after, upto): (u64, u64), (start, end): (&[u8], &[u8])) -> Option<bool> {
        let oldest = self.writes.front().map_or(u64::MAX, |(n, _, _)| *n);
        if after < upto && oldest > after + 1 {
            return None;
        }
        Some(self.writes.iter().any(|(n, first, last)| {
            *n > after && *n <= upto && &first[..] < end && start <= &last[..]
        }))
    }
}

impl Default for SpanKeys {
    fn default() -> SpanKeys {
        SpanKeys {
            published: Published::new(Arc::new(Vec::new())),
            recent: Published::new(Arc::new(Released::default())),
        }
    }
}

impl std::fmt::Debug for SpanKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanKeys").field("keys", &self.published.load().len()).finish()
    }
}

impl SpanKeys {
    /// Whether a write holds a key of `[start, end)`.
    pub fn any_in(&self, start: &[u8], end: &[u8]) -> bool {
        if start >= end {
            return false;
        }
        let keys = self.published.load();
        let first = keys.partition_point(|k| &k[..] < start);
        keys.get(first).is_some_and(|k| &k[..] < end)
    }

    /// Publishes the keys of `held`, in order.
    fn publish(&self, held: &BTreeMap<Key, u32>) {
        self.published.store(Arc::new(held.keys().cloned().collect()));
    }

    /// The writes released last, for a scan that saw the count of releases go up.
    pub fn released(&self) -> Arc<Released> {
        self.recent.load()
    }

    /// Records write number `n` released, with its keys from `first` to `last`.
    fn record_release(&self, n: u64, first: &Key, last: &Key) {
        let now = self.recent.load();
        let mut writes = now.writes.clone();
        let bound =
            |k: &Key, b: Vec<u8>| if b.len() == k.len() { Arc::clone(k) } else { Arc::from(b) };
        let (lo, hi) = (crate::load::at_or_below(first), crate::load::at_or_above(last));
        writes.push_back((n, bound(first, lo), bound(last, hi)));
        while writes.len() > RECENT_RELEASES {
            writes.pop_front();
        }
        self.recent.store(Arc::new(Released { writes }));
    }
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

/// A write proposed and not applied yet, on one of its keys (FR-029).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pipelined {
    ts: Hlc,
    id: LatchId,
    /// The key is the write's idempotence record.
    record: bool,
    /// What the write leaves under the key once applied: a version (its type and bytes) or none
    /// (a delete); `None` when it only read the key.
    value: Option<Pending>,
}

/// What a pending write leaves under a key: a version, its type and its bytes, or none.
pub type Pending = Option<(u8, Vec<u8>)>;

/// The latches of a range's leader.
#[derive(Clone, Debug, Default)]
pub struct Latches {
    by_key: BTreeMap<Key, (Option<Hlc>, LatchId)>,
    /// The writes proposed and not applied yet by key, in the order of their entries (FR-029).
    pipelined: BTreeMap<Key, Vec<Pipelined>>,
    /// The holders that took their latches with [`Latches::acquire_pipelined`]: only they go on
    /// pipelined writes when they add keys.
    goes_on: BTreeSet<LatchId>,
    /// The writes already answered to their client while their latches are held.
    answered: BTreeSet<LatchId>,
    next: u64,
    /// The writes holding latches, and how many they are for the reads outside the core (005
    /// FR-028): a write is counted when it takes its first latch, before it gets a mark, and a
    /// read outside the core that sees any goes to the core.
    holders: BTreeSet<LatchId>,
    held: Arc<AtomicU32>,
    /// The keys latched, counted for the point reads outside the core, and the slots each write
    /// counted, which its release uncounts.
    keys: Arc<KeySlots>,
    /// The keys latched, in order, for the scans outside the core, and how many writes hold each.
    spans: Arc<SpanKeys>,
    span_counts: BTreeMap<Key, u32>,
    /// How many writes with counted keys were released, raised before their keys leave
    /// [`SpanKeys`].
    released: Arc<AtomicU64>,
    /// The keys each write counted, which its release uncounts.
    counted: BTreeMap<LatchId, Vec<Key>>,
}

impl Latches {
    /// Takes `keys` for a write; if one is held, the holder it has to wait for.
    pub fn acquire(&mut self, keys: &[Vec<u8>]) -> Result<LatchId, LatchId> {
        if let Some(holder) = keys.iter().find_map(|k| self.pipelined_holder(k, false)) {
            return Err(holder);
        }
        self.next += 1;
        let id = LatchId(self.next);
        self.extend(id, keys)?;
        Ok(id)
    }

    /// Takes `keys` for a write without a transaction (FR-029): the pipelined writes of its keys
    /// do not stop it, only an idempotence record among them, which a retry of the same statement
    /// waits for.
    pub fn acquire_pipelined(&mut self, keys: &[Vec<u8>]) -> Result<LatchId, LatchId> {
        if let Some(holder) = keys.iter().find_map(|k| self.pipelined_holder(k, true)) {
            return Err(holder);
        }
        self.next += 1;
        let id = LatchId(self.next);
        self.goes_on.insert(id);
        if let Err(holder) = self.extend(id, keys) {
            self.goes_on.remove(&id);
            return Err(holder);
        }
        Ok(id)
    }

    /// The write that holds one of `keys`, pipelined or not, if any: what a write of them waits
    /// for, as an operator reads it.
    pub fn holder_of(&self, keys: &[Vec<u8>]) -> Option<(LatchId, bool)> {
        let pipelined = keys.iter().find_map(|k| self.pipelined_holder(k, false));
        if let Some(id) = pipelined {
            return Some((id, true));
        }
        keys.iter().find_map(|k| self.by_key.get(&k[..])).map(|(_, h)| (*h, false))
    }

    /// The newest pipelined write of `key` a write has to wait for: any, or with `records_only`
    /// the one whose idempotence record `key` is.
    fn pipelined_holder(&self, key: &[u8], records_only: bool) -> Option<LatchId> {
        let held = self.pipelined.get(key)?;
        held.iter().rev().find(|p| p.record || !records_only).map(|p| p.id)
    }

    /// What the newest pipelined write of `key` leaves under it, if one writes it: the value the
    /// next write of the key is evaluated on (FR-029).
    pub fn pending_value(&self, key: &[u8]) -> Option<&Pending> {
        self.pipelined.get(key)?.iter().rev().find_map(|p| p.value.as_ref())
    }

    /// The write `id`, proposed at `ts`, gives its keys up to the next writes without a
    /// transaction (FR-029): they stay held for the reads and the transactions until it is
    /// applied. `records` are its keys that are idempotence records and `written` what it leaves
    /// under the keys it writes, held too from now on.
    pub fn pipeline(
        &mut self,
        id: LatchId,
        ts: Hlc,
        records: &[Vec<u8>],
        mut written: BTreeMap<Vec<u8>, Pending>,
    ) {
        let mine: Vec<Key> =
            self.by_key.iter().filter(|(_, (_, h))| *h == id).map(|(k, _)| Arc::clone(k)).collect();
        for k in mine {
            self.by_key.remove(&k);
            let value = written.remove(&k[..]);
            let record = records.iter().any(|r| r[..] == k[..]);
            self.pipelined.entry(k).or_default().push(Pipelined { ts, id, record, value });
        }
        let mut added = false;
        for (k, value) in written {
            // A key written and not latched (only a faulty variant leaves one) counts now.
            let k: Key = Arc::from(k);
            added |= self.count_key(id, &k);
            let p = Pipelined { ts, id, record: false, value: Some(value) };
            self.pipelined.entry(k).or_default().push(p);
        }
        if added {
            self.spans.publish(&self.span_counts);
        }
        if self.holders.insert(id) {
            self.held
                .store(u32::try_from(self.holders.len()).unwrap_or(u32::MAX), Ordering::SeqCst);
        }
    }

    /// Adds `keys` to the latches of `id` (the index entries its evaluation found); if one is
    /// held by another write, nothing is taken and that write is returned.
    pub fn extend(&mut self, id: LatchId, keys: &[Vec<u8>]) -> Result<(), LatchId> {
        let records_only = self.goes_on.contains(&id);
        let pipelined =
            keys.iter().filter_map(|k| self.pipelined_holder(k, records_only)).find(|h| *h != id);
        if let Some(holder) = pipelined {
            return Err(holder);
        }
        let other = keys.iter().filter_map(|k| self.by_key.get(&k[..])).find(|(_, h)| *h != id);
        // Faulty variant (005 T017): writes never wait for each other, so two increments of one
        // document read the same version.
        #[cfg(teseladb_bug = "no_latch_inc")]
        let other: Option<&(Option<Hlc>, LatchId)> = other.filter(|_| false);
        if let Some((_, holder)) = other {
            return Err(*holder);
        }
        let ts = self.by_key.values().find(|(_, h)| *h == id).and_then(|(t, _)| *t);
        let mut added = false;
        for k in keys {
            // Counted here, before the write's mark: the simulator runs a core step whole, so
            // only `spec/tla/HlcKeyLatches.tla` (its CountBeforeMark mutant) reaches a read
            // between the count and the mark. A key latched again by the same write is not
            // copied again.
            if let Some(held) = self.by_key.get_mut(&k[..]) {
                *held = (ts, id);
                continue;
            }
            let k: Key = Arc::from(&k[..]);
            self.by_key.insert(Arc::clone(&k), (ts, id));
            added |= self.count_key(id, &k);
        }
        if added {
            self.spans.publish(&self.span_counts);
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
        let pipelined = || self.pipelined.values().flatten().any(|p| p.id == id);
        if self.by_key.values().any(|(_, h)| *h == id) || pipelined() {
            self.answered.insert(id);
        }
    }

    /// Releases a write's latches once it is applied (or abandoned).
    pub fn release(&mut self, id: LatchId) {
        self.by_key.retain(|_, (_, holder)| *holder != id);
        self.pipelined.retain(|_, held| {
            held.retain(|p| p.id != id);
            !held.is_empty()
        });
        self.answered.remove(&id);
        self.goes_on.remove(&id);
        let counted = self.counted.remove(&id).unwrap_or_default();
        if let (Some(first), Some(last)) = (counted.iter().min(), counted.iter().max()) {
            // Its keys before its number: a scan that sees the number finds them.
            let n = self.released.load(Ordering::SeqCst) + 1;
            self.spans.record_release(n, first, last);
            self.released.fetch_add(1, Ordering::SeqCst);
        }
        let mut gone = false;
        for k in &counted {
            self.keys.uncount(KeySlots::slot(k));
            if let Some(n) = self.span_counts.get_mut(&k[..]) {
                *n -= 1;
                if *n == 0 {
                    self.span_counts.remove(&k[..]);
                    gone = true;
                }
            }
        }
        if gone {
            self.spans.publish(&self.span_counts);
        }
        if self.holders.remove(&id) {
            self.held
                .store(u32::try_from(self.holders.len()).unwrap_or(u32::MAX), Ordering::SeqCst);
        }
    }

    /// How many writes hold latches, for the reads outside the core (005 FR-028).
    pub fn held(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.held)
    }

    /// The keys latched, for the point reads outside the core (005 FR-028).
    pub fn keys(&self) -> Arc<KeySlots> {
        Arc::clone(&self.keys)
    }

    /// The keys latched in order, for the scans outside the core (005 FR-028).
    pub fn spans(&self) -> Arc<SpanKeys> {
        Arc::clone(&self.spans)
    }

    /// How many writes with counted keys were released, for the scans outside the core.
    pub fn released(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.released)
    }

    /// Counts `key`, latched by `id`, in its slot and among the keys in order; whether the keys
    /// in order gained it, and have to be published before the write's mark.
    fn count_key(&mut self, id: LatchId, key: &Key) -> bool {
        self.keys.count(KeySlots::slot(key));
        self.counted.entry(id).or_default().push(Arc::clone(key));
        let n = match self.span_counts.get_mut(&key[..]) {
            Some(n) => n,
            None => self.span_counts.entry(Arc::clone(key)).or_insert(0),
        };
        *n += 1;
        *n == 1
    }

    /// Whether `key` is held.
    pub fn is_held(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key) || self.pipelined.contains_key(key)
    }

    /// Whether no latch is held.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty() && self.pipelined.is_empty()
    }

    /// Every held latch of `[start, end)`: the exclusive ones and the pipelined ones.
    fn held_in<'a>(
        &'a self,
        start: &[u8],
        end: &[u8],
    ) -> impl Iterator<Item = (Option<Hlc>, LatchId)> + 'a {
        let bounds = || (Bound::Included(start), Bound::Excluded(end));
        let exclusive = self.by_key.range::<[u8], _>(bounds()).map(|(_, v)| *v);
        let pipelined = self
            .pipelined
            .range::<[u8], _>(bounds())
            .flat_map(|(_, held)| held.iter().map(|p| (Some(p.ts), p.id)));
        // Faulty variant (005 T054): the reads do not wait for the pipelined writes, and see a
        // value before its entry commits, or miss a write confirmed below their mark.
        #[cfg(teseladb_bug = "reads_skip_pipelined")]
        let pipelined = pipelined.filter(|_| false);
        exclusive.chain(pipelined)
    }

    /// What a read of `[start, end)` at `ts` waits for, among the latches `which` names.
    pub fn read_wait(&self, start: &[u8], end: &[u8], ts: Hlc, which: WaitFor) -> ReadWait {
        self.held_in(start, end)
            .find(|(t, _)| match which {
                WaitFor::AtOrBelow => t.is_some_and(|t| t <= ts),
                WaitFor::Every => true,
            })
            .map_or(ReadWait::Free, |(_, id)| ReadWait::On(id))
    }

    /// The mark at which a `local` or `available` read of `[start, end)` that chose its own mark
    /// `ts` reads without waiting: just below the lowest mark of a write in flight there, or `ts`.
    /// A range applies its writes in the order of its log, which is the order of their marks, so
    /// below the lowest one in flight every write is applied. `None` when a write of those keys
    /// was answered before it was applied: the read waits for it, so it sees it (DV-010).
    pub fn own_read_mark(&self, start: &[u8], end: &[u8], ts: Hlc) -> Option<Hlc> {
        if self.held_in(start, end).any(|(_, id)| self.answered.contains(&id)) {
            return None;
        }
        let lowest = self.held_in(start, end).filter_map(|(t, _)| t).filter(|t| *t <= ts).min();
        Some(lowest.map_or(ts, crate::intents::pred))
    }
}
