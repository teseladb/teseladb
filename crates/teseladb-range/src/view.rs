//! The view of each range this node leads, for reads served on a connection's thread without the
//! core (005 FR-028, H-013 of the native tests). The core publishes, after each batch, the term,
//! the time until which the lease and the clock rule hold (checked by the core for that whole
//! span), the GC threshold and the LSM's published version; the count of writes holding latches
//! and the bound of the reads served outside are atomics shared with the core.
//!
//! A read outside the core takes its mark from the node's clock, raises the bound to it, then
//! reads only if no write holds a latch it covers: either it sees the write, or the write sees its
//! bound and takes a mark above it (`spec/tla/HlcLatches.tla`, which fails without either step).
//! A point read looks at the writes of its own key, in the slots the latches count
//! (`spec/tla/HlcKeyLatches.tla`); a scan at every write of the range. Any condition it cannot
//! check goes back to the core, which serves the read as before.
//!
//! The time the view holds until is checked after the mark is taken: a thread stopped between
//! the two would otherwise read at a mark from after the lease, where another leader, or the
//! half of a split whose read cache starts 250 ms above the clock, may write below it. Checked
//! after, the mark is at most the clock's at `valid_until`, at most [`VIEW_SPAN`] past the
//! publication.
//!
//! [`VIEW_SPAN`]: crate::host::VIEW_SPAN

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use teseladb_hlc::{Hlc, Next};
use teseladb_lsm::{
    BlockNeed, Kind, Published, SharedBackScan, SharedLsm, SharedRead, SharedScan, SharedStep,
    ViewCache,
};
use teseladb_multiraft::NodeClock;
use teseladb_types::{GroupId, Nanos};

use crate::doc_count::{Coll, Counts};
use crate::latch::KeySlots;
use crate::read::ReadConcern;
use crate::sm::split_value;
use crate::tscache::word;

/// The view of one range.
pub struct RangeView {
    /// The range.
    pub g: GroupId,
    /// The term the core leads it in.
    pub term: u64,
    start: Vec<u8>,
    end: Vec<u8>,
    /// Monotonic time until which the core checked the lease and the clock rule.
    valid_until: Nanos,
    /// Reads below it may miss versions the compaction dropped (FR-013).
    gc_floor: Hlc,
    held: Arc<AtomicU32>,
    keys: Arc<KeySlots>,
    outside: Arc<AtomicU64>,
    /// The live documents of each collection when the core published the view, if exact (007
    /// FR-039).
    counts: Option<Arc<Counts>>,
    lsm: SharedLsm,
    clock: NodeClock,
}

impl std::fmt::Debug for RangeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeView").field("g", &self.g).field("term", &self.term).finish()
    }
}

/// What a read outside the core gives.
#[derive(Debug)]
pub enum FastRead {
    /// The visible version at the read's mark (its value type and value).
    Found {
        /// The read's mark.
        ts: Hlc,
        /// The visible version.
        value: Option<(u8, Vec<u8>)>,
    },
    /// A block to read from its file and give to [`SharedLsm::supply`]; then the read runs
    /// again.
    Need(BlockNeed),
    /// The read goes to the core.
    Core,
}

/// The parts of a view the core fills.
pub(crate) struct ViewParts {
    pub(crate) g: GroupId,
    pub(crate) term: u64,
    pub(crate) range: (Vec<u8>, Vec<u8>),
    pub(crate) valid_until: Nanos,
    pub(crate) gc_floor: Hlc,
    pub(crate) held: Arc<AtomicU32>,
    pub(crate) keys: Arc<KeySlots>,
    pub(crate) outside: Arc<AtomicU64>,
    pub(crate) counts: Option<Arc<Counts>>,
}

impl RangeView {
    pub(crate) fn new(p: ViewParts, lsm: SharedLsm, clock: NodeClock) -> RangeView {
        RangeView {
            g: p.g,
            term: p.term,
            start: p.range.0,
            end: p.range.1,
            valid_until: p.valid_until,
            gc_floor: p.gc_floor,
            held: p.held,
            keys: p.keys,
            outside: p.outside,
            counts: p.counts,
            lsm,
            clock,
        }
    }

    /// The block cache a missed block goes to.
    pub fn lsm(&self) -> &SharedLsm {
        &self.lsm
    }

    /// The mark of a read outside the core of `key`, or of the whole range with `None`, with the
    /// wall clock at `wall_ms`; `now` gives the monotonic time once it is taken. `None` when the
    /// read goes to the core.
    fn mark(&self, key: Option<&[u8]>, wall_ms: u64, now: impl FnOnce() -> Nanos) -> Option<Hlc> {
        let Next::Mark(ts) = self.clock.0.now(wall_ms) else { return None };
        if ts < self.gc_floor {
            return None;
        }
        // The bound first, then the latches: a write counted after this check takes a mark
        // above the bound.
        self.outside.fetch_max(word(ts), Ordering::SeqCst);
        // Faulty variant (005 T049): a read outside the core that does not look at the writes
        // holding latches, which may then take a mark below it.
        #[cfg(not(teseladb_bug = "outside_ignores_latches"))]
        let writing = match key {
            Some(key) => self.keys.maybe_held(key),
            None => self.held.load(Ordering::SeqCst) != 0,
        };
        #[cfg(teseladb_bug = "outside_ignores_latches")]
        let writing = {
            let _ignored = (&self.held, &self.keys, key);
            false
        };
        (!writing && now() < self.valid_until).then_some(ts)
    }

    /// Reads `key` at a mark of the node's clock, with the wall clock at `wall_ms`, if this
    /// thread can serve it; `now` gives the monotonic time once the mark is taken.
    pub fn read(
        &self,
        cache: &mut ViewCache,
        key: &[u8],
        wall_ms: u64,
        now: impl FnOnce() -> Nanos,
        concern: ReadConcern,
    ) -> FastRead {
        let servable = matches!(concern, ReadConcern::Local | ReadConcern::Majority)
            && self.start.as_slice() <= key
            && key < self.end.as_slice();
        if !servable {
            return FastRead::Core;
        }
        let Some(ts) = self.mark(Some(key), wall_ms, now) else { return FastRead::Core };
        let view = self.lsm.view_cached(cache);
        match view.get(&self.lsm, key, ts) {
            SharedRead::Hit(f) if f.kind == Kind::Intent => FastRead::Core,
            SharedRead::Hit(f) => {
                FastRead::Found { ts, value: split_value(&f.value).map(|(t, v)| (t, v.to_vec())) }
            }
            SharedRead::NotFound => FastRead::Found { ts, value: None },
            SharedRead::Need(need) => FastRead::Need(need),
            SharedRead::Failed(_) => FastRead::Core,
        }
    }
}

impl RangeView {
    /// The documents of `[start, end)` with their value types, at most `limit` (more than none),
    /// in key order (from the last key down when `backward`, H-003), read at one mark of the node's clock as [`RangeView::read`] reads a key,
    /// with the wall clock and the monotonic time of `clocks`; with the mark. A block the shared cache lacks goes to
    /// `supply`, which reads it, gives it to [`SharedLsm::supply`](teseladb_lsm::SharedLsm) and
    /// says whether it could; then the scan goes on. `None` when the scan goes to the core: a
    /// span past the range, an intent, a block that could not be read, a failed read, and
    /// whatever [`RangeView::read`] sends there. Versions that only serve the history are no
    /// documents, as in the core's scans.
    pub fn scan(
        &self,
        cache: &mut ViewCache,
        (start, end): (&[u8], &[u8]),
        (limit, backward): (usize, bool),
        (wall_ms, now): (u64, impl FnOnce() -> Nanos),
        concern: ReadConcern,
        mut supply: impl FnMut(&BlockNeed) -> bool,
    ) -> Option<(Hlc, Vec<crate::push::Row>)> {
        let servable = matches!(concern, ReadConcern::Local | ReadConcern::Majority)
            && limit > 0
            && self.start.as_slice() <= start
            && start <= end
            && end <= self.end.as_slice();
        if !servable {
            return None;
        }
        let ts = self.mark(None, wall_ms, now)?;
        let view = self.lsm.view_cached(cache);
        let mut scan = match backward {
            false => Either::Forward(view.scan(start, end, ts)),
            true => Either::Backward(view.scan_back(start, end, ts)),
        };
        let mut rows = Vec::new();
        while rows.len() < limit {
            match scan.next(&self.lsm) {
                SharedStep::Item(i) if i.kind == Kind::Intent => return None,
                SharedStep::Item(i) => {
                    if let Some((t, v)) = split_value(&i.value)
                        && t != crate::HISTORY_VALUE
                    {
                        rows.push((i.key, (t, v.to_vec())));
                    }
                }
                SharedStep::Done => break,
                SharedStep::Need(need) if supply(&need) => {}
                SharedStep::Need(_) | SharedStep::Failed(_) => return None,
            }
        }
        Some((ts, rows))
    }
}

impl RangeView {
    /// The live documents of collection `coll` in the range, read as [`RangeView::read`] reads a
    /// key (the counts are those of the state the view's LSM version holds), with the wall clock
    /// and the monotonic time of `clocks` (H-004 of the native tests). `None` when the counts
    /// are estimated or the read goes to the core.
    pub fn count(
        &self,
        coll: Coll,
        (wall_ms, now): (u64, impl FnOnce() -> Nanos),
        concern: ReadConcern,
    ) -> Option<u64> {
        let counts = self.counts.as_ref()?;
        if !matches!(concern, ReadConcern::Local | ReadConcern::Majority) {
            return None;
        }
        self.mark(None, wall_ms, now)?;
        Some(counts.docs.get(&coll).copied().unwrap_or(0))
    }
}

/// A scan of the shared LSM in either direction.
enum Either {
    Forward(SharedScan),
    Backward(SharedBackScan),
}

impl Either {
    fn next(&mut self, lsm: &SharedLsm) -> SharedStep {
        match self {
            Either::Forward(s) => s.next(lsm),
            Either::Backward(s) => s.next(lsm),
        }
    }
}

/// The views of the ranges a core leads, published by the core and read on any thread.
#[derive(Clone)]
pub struct RangeViews {
    cell: Arc<Published<BTreeMap<GroupId, Arc<RangeView>>>>,
}

impl std::fmt::Debug for RangeViews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeViews").finish()
    }
}

impl Default for RangeViews {
    fn default() -> RangeViews {
        RangeViews { cell: Arc::new(Published::new(Arc::new(BTreeMap::new()))) }
    }
}

impl RangeViews {
    /// The view of range `g`, if this core leads it and published one.
    pub fn get(&self, g: GroupId) -> Option<Arc<RangeView>> {
        self.cell.load().get(&g).cloned()
    }

    pub(crate) fn publish(&self, views: BTreeMap<GroupId, Arc<RangeView>>) {
        self.cell.store(Arc::new(views));
    }
}
