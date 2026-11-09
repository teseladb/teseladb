//! The read cache of a range's leader (FR-004, FR-005, research R3): key intervals with the
//! highest mark a read of them was served at, over a minimum. A write takes a mark above the cache
//! of its keys, so a read at t never sees a version appear later at or below t. The minimum starts
//! 250 ms (the most two clocks differ, D-004) above the node's clock when the replica wins the
//! term: the previous leader's lease ended before that election, its reads at its wall clock are
//! below the minimum, and those above its own minimum are above a clock its lease's voters carried
//! here (the core raised its clock to its minimum before its term's first messages). When the cache is full, the
//! intervals with the lowest mark go and that mark joins the minimum; that only raises later
//! marks.
//!
//! The answers come from the upper envelope of the intervals: disjoint segments, ordered by start
//! key, each with the highest mark of the reads that cover it. An evicted read's mark is at or
//! below the minimum, and every answer is at least the minimum, so a segment at or below it
//! changes no answer: such segments are swept out in one pass once the map holds more than
//! [`SWEEP_FACTOR`] segments per interval allowed. Touching segments never share a mark, so the
//! segments above the minimum change value only at an end of an interval held: at most 2n − 1
//! of them for n intervals, and a sweep leaves at most twice the capacity. Of the intervals
//! themselves only their marks are kept, in a heap, to know which go first. A read and a lookup
//! cost O(log n) plus the segments they cover; a sweep, amortized, O(1) per read. A query over
//! an empty interval (`start >= end`) holds no key and answers the minimum.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_hlc::{Hlc, LOGICAL_BITS};

/// The most two nodes' clocks differ (D-004), in milliseconds.
pub const LEASE_MARGIN_MS: u64 = 250;

/// Segments per interval of capacity the map may hold before the segments at or below the minimum
/// are swept out; above 2, so that a sweep frees room for several reads.
pub const SWEEP_FACTOR: usize = 4;

/// The read cache.
#[derive(Clone, Debug)]
pub struct TsCache {
    floor: Hlc,
    /// Disjoint segments: start → (end, mark); those at or below `floor` wait for a sweep.
    segments: BTreeMap<Vec<u8>, (Vec<u8>, Hlc)>,
    /// The mark of each interval held, lowest first.
    marks: BinaryHeap<Reverse<Hlc>>,
    /// The highest mark recorded: a read above it is on a mark no segment has.
    top: Hlc,
    capacity: usize,
    /// The bound of the reads served outside the core (005 FR-028), as a [`word`]: a read on a
    /// connection's thread raises it to its mark before it reads, and every write goes above it
    /// as above the cache.
    outside: Arc<AtomicU64>,
}

/// A mark as one ordered word: its physical part above its logical part. A logical part wider
/// than a clock's marks (an end-of-millisecond bound) rounds up, which only raises later writes.
pub fn word(h: Hlc) -> u64 {
    let logical = u64::from(h.logical).min((1 << LOGICAL_BITS) - 1);
    h.physical.saturating_mul(1 << LOGICAL_BITS) | logical
}

/// The mark of a [`word`].
pub fn unword(w: u64) -> Hlc {
    let logical = u32::try_from(w & ((1 << LOGICAL_BITS) - 1)).unwrap_or(u32::MAX);
    Hlc { physical: w >> LOGICAL_BITS, logical }
}

impl TsCache {
    /// A cache with minimum `floor` holding at most `capacity` intervals.
    pub fn new(floor: Hlc, capacity: usize) -> TsCache {
        TsCache {
            floor,
            segments: BTreeMap::new(),
            marks: BinaryHeap::new(),
            top: floor,
            capacity: capacity.max(1),
            outside: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The cache of a leader whose term began, as far as its host saw, at `lease` (FR-005).
    pub fn for_lease(lease: Hlc, capacity: usize) -> TsCache {
        TsCache::new(Hlc { physical: lease.physical + LEASE_MARGIN_MS, logical: 0 }, capacity)
    }

    /// The minimum.
    pub fn floor(&self) -> Hlc {
        self.floor
    }

    /// Intervals held.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// Segments of the envelope held (its memory): at most [`SWEEP_FACTOR`] times the capacity.
    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /// Whether it holds no interval.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// A read of `[start, end)` served at `ts`.
    pub fn record(&mut self, start: &[u8], end: &[u8], ts: Hlc) {
        if ts <= self.floor {
            return;
        }
        // Every segment's mark is one recorded before: none has a mark above all of them.
        let new_mark = ts > self.top;
        self.top = self.top.max(ts);
        self.marks.push(Reverse(ts));
        if start < end {
            self.raise(start, end, ts, new_mark);
        }
        if self.marks.len() > self.capacity {
            // The lowest mark goes, with every interval that has it; it joins the minimum.
            let Reverse(gone) = self.marks.pop().expect("more than the capacity");
            while self.marks.peek().is_some_and(|&Reverse(m)| m == gone) {
                self.marks.pop();
            }
            self.floor = gone;
        }
        if self.segments.len() > SWEEP_FACTOR * self.capacity {
            let floor = self.floor;
            self.segments.retain(|_, (_, m)| *m > floor);
        }
    }

    /// The bound of the reads served outside the core, which they raise (005 FR-028).
    pub fn outside(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.outside)
    }

    /// The minimum, with the reads served outside the core: no write goes at or below it.
    fn base(&self) -> Hlc {
        self.floor.max(unword(self.outside.load(Ordering::SeqCst)))
    }

    /// The highest mark a read of any key in `[start, end)` was served at.
    pub fn max_over(&self, start: &[u8], end: &[u8]) -> Hlc {
        if start >= end {
            return self.base();
        }
        let before = self
            .segments
            .range::<[u8], _>(bounds(Unbounded, Included(start)))
            .next_back()
            .filter(|(_, (e, _))| e.as_slice() > start)
            .map(|(_, (_, m))| *m);
        let inside = self
            .segments
            .range::<[u8], _>(bounds(Excluded(start), Excluded(end)))
            .map(|(_, (_, m))| *m);
        before.into_iter().chain(inside).fold(self.base(), Hlc::max)
    }

    /// The highest read of any of `keys`: a write of them gets a mark above it (with `succ`,
    /// FR-025, when the node's clock is behind; `MultiRaftCore::propose_at`).
    pub fn floor_of(&self, keys: &[Vec<u8>]) -> Hlc {
        keys.iter().map(|k| self.at(k)).fold(self.base(), Hlc::max)
    }

    /// The mark of the segment holding `key`, or the minimum. `[key, key·00)` holds only `key`:
    /// a segment overlaps it when it starts at or before `key` and ends after it.
    fn at(&self, key: &[u8]) -> Hlc {
        self.segments
            .range::<[u8], _>(bounds(Unbounded, Included(key)))
            .next_back()
            .filter(|(_, (e, _))| e.as_slice() > key)
            .map_or(self.floor, |(_, (_, m))| *m)
    }

    /// Raises every key of `[start, end)` to at least `ts`, keeping the segments disjoint and
    /// touching ones on different marks; `new_mark`: no segment has mark `ts` yet.
    fn raise(&mut self, start: &[u8], end: &[u8], ts: Hlc, new_mark: bool) {
        // The last segment starting before `end`: if it ends at or before `start`, so do all the
        // earlier ones, and nothing overlaps.
        let last = self
            .segments
            .range::<[u8], _>(bounds(Unbounded, Excluded(end)))
            .next_back()
            .map(|(s, (e, m))| (s.as_slice(), e.as_slice(), *m));
        if last.is_none_or(|(_, e, _)| e <= start) {
            // Nothing there yet (a point read of a key not read before): one segment, merged with
            // a touching neighbour on the same mark (there is none on a new mark).
            let left = last
                .filter(|&(_, e, m)| !new_mark && e == start && m == ts)
                .map(|(s, _, _)| s.to_vec());
            let right = if new_mark {
                None
            } else {
                self.segments.get(end).filter(|(_, m)| *m == ts).map(|(e, _)| e.clone())
            };
            if right.is_some() {
                self.segments.remove(end);
            }
            let e = right.unwrap_or_else(|| end.to_vec());
            match left {
                Some(s) => self.segments.insert(s, (e, ts)),
                None => self.segments.insert(start.to_vec(), (e, ts)),
            };
            return;
        }
        // The region rebuilt: from the segment holding `start` to the one holding the last key
        // before `end`, so nothing outside it changes.
        let lo = self
            .segments
            .range::<[u8], _>(bounds(Unbounded, Included(start)))
            .next_back()
            .filter(|(_, (e, _))| e.as_slice() > start)
            .map_or_else(|| start.to_vec(), |(s, _)| s.clone());
        let old: Vec<(Vec<u8>, Vec<u8>, Hlc)> = self
            .segments
            .range::<[u8], _>(bounds(Included(lo.as_slice()), Excluded(end)))
            .map(|(s, (e, m))| (s.clone(), e.clone(), *m))
            .collect();
        let hi = old.last().map_or(end, |(_, e, _)| e.as_slice().max(end)).to_vec();
        let mut pieces: Vec<(Vec<u8>, Vec<u8>, Hlc)> = Vec::with_capacity(old.len() * 2 + 1);
        let mut at = lo.clone();
        for (s, e, m) in &old {
            if at < *s {
                // A gap (the minimum): inside `[start, end)` it takes `ts`.
                push_piece(&mut pieces, at.clone(), s.clone(), ts);
            }
            // The part of the segment before `start`, inside, and after `end`.
            let (a, b) = (s.as_slice().max(start), e.as_slice().min(end));
            if s.as_slice() < a {
                push_piece(&mut pieces, s.clone(), a.to_vec(), *m);
            }
            push_piece(&mut pieces, a.to_vec(), b.to_vec(), (*m).max(ts));
            if b < e.as_slice() {
                push_piece(&mut pieces, b.to_vec(), e.clone(), *m);
            }
            at = e.clone();
        }
        if at.as_slice() < end {
            push_piece(&mut pieces, at, end.to_vec(), ts);
        }
        for (s, _, _) in &old {
            self.segments.remove(s);
        }
        // Merges with the neighbours on the same mark. The region has a piece: the last segment
        // starting before `end` ends after `start`, so it is in `old` and overlaps `[start, end)`.
        let (first_start, _, first_mark) = pieces.first_mut().expect("a piece of the region");
        if let Some((s, (e, m))) =
            self.segments.range::<[u8], _>(bounds(Unbounded, Excluded(lo.as_slice()))).next_back()
            && *e == *first_start
            && *m == *first_mark
        {
            let s = s.clone();
            self.segments.remove(&s);
            *first_start = s;
        }
        let (_, last_end, last_mark) = pieces.last_mut().expect("a piece of the region");
        if let Some((m, e)) = self.segments.get(hi.as_slice()).map(|(e, m)| (*m, e.clone()))
            && m == *last_mark
        {
            self.segments.remove(hi.as_slice());
            *last_end = e;
        }
        for (s, e, m) in pieces {
            self.segments.insert(s, (e, m));
        }
    }
}

/// Appends the non-empty `[s, e)` at `m`, merged with the previous piece when it is on the same
/// mark: the pieces of a region are built left to right without gaps, so the previous one ends
/// at `s`.
fn push_piece(pieces: &mut Vec<(Vec<u8>, Vec<u8>, Hlc)>, s: Vec<u8>, e: Vec<u8>, m: Hlc) {
    if let Some((_, pe, pm)) = pieces.last_mut()
        && *pm == m
    {
        *pe = e;
        return;
    }
    pieces.push((s, e, m));
}

/// Key bounds for a range query over byte-string keys.
fn bounds<'a>(lo: Bound<&'a [u8]>, hi: Bound<&'a [u8]>) -> (Bound<&'a [u8]>, Bound<&'a [u8]>) {
    (lo, hi)
}
