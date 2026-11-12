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
//! (`spec/tla/HlcKeyLatches.tla`); when a write counts its slot, which other keys share, it reads
//! and then looks as a scan of that one key does. A scan, after it reads, looks at the writes of
//! the keys it read, absent ones included, in the latched keys in order, and at whether any write
//! was released while it read; a count at every write of the range. Any condition it cannot check
//! goes back to the core, which serves the read as before.
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

use crate::ack::{WriteConcern, met_without_entry};
use crate::command::DedupKey;
use crate::doc_count::{Coll, Counts};
use crate::host::Mutation;
use crate::latch::{KeySlots, SpanKeys};
use crate::read::ReadConcern;
use crate::sm::{dedup_key, split_value};
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
    /// The mark of the last entry the range had applied when the core published the view, which
    /// the LSM's version published just before holds (FR-031).
    applied: Hlc,
    /// Writes answered to their client and not applied yet, which a scan has to see (FR-031).
    early: Arc<AtomicU32>,
    held: Arc<AtomicU32>,
    keys: Arc<KeySlots>,
    spans: Arc<SpanKeys>,
    released: Arc<AtomicU64>,
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

/// The writes a read outside the core looks at before it reads.
#[derive(Clone, Copy, Debug)]
enum Watch<'a> {
    /// Those of these keys.
    Keys(&'a [&'a [u8]]),
    /// None before the read: the caller checks after it.
    Later,
    /// Every write of the range.
    Range,
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
    pub(crate) spans: Arc<SpanKeys>,
    pub(crate) released: Arc<AtomicU64>,
    pub(crate) outside: Arc<AtomicU64>,
    pub(crate) counts: Option<Arc<Counts>>,
    pub(crate) applied: Hlc,
    pub(crate) early: Arc<AtomicU32>,
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
            applied: p.applied,
            early: p.early,
            held: p.held,
            keys: p.keys,
            spans: p.spans,
            released: p.released,
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

    /// The mark of a read outside the core of what `watch` names, with the wall clock at
    /// `wall_ms`; `now` gives the monotonic time once it is taken. `None` when the read goes to
    /// the core.
    fn mark(&self, watch: Watch<'_>, wall_ms: u64, now: impl FnOnce() -> Nanos) -> Option<Hlc> {
        let Next::Mark(ts) = self.clock.0.now(wall_ms) else {
            Decline::Expired.count();
            return None;
        };
        if ts < self.gc_floor {
            Decline::Other.count();
            return None;
        }
        // The bound first, then the latches: a write counted after this check takes a mark
        // above the bound.
        self.outside.fetch_max(word(ts), Ordering::SeqCst);
        // Faulty variant (005 T049): a read outside the core that does not look at the writes
        // holding latches, which may then take a mark below it.
        #[cfg(not(teseladb_bug = "outside_ignores_latches"))]
        let writing = match watch {
            Watch::Keys(keys) => keys.iter().any(|k| self.keys.maybe_held(k)),
            // A scan looks once it has read (`RangeView::scan`).
            Watch::Later => false,
            Watch::Range => self.held.load(Ordering::SeqCst) != 0,
        };
        #[cfg(teseladb_bug = "outside_ignores_latches")]
        let writing = {
            let watched = match watch {
                Watch::Keys(keys) => keys.len(),
                Watch::Later => 0,
                Watch::Range => 0,
            };
            let _ignored = (&self.held, &self.keys, &self.spans, watched);
            false
        };
        if writing {
            Decline::Writing.count();
            return None;
        }
        if now() >= self.valid_until {
            Decline::Expired.count();
            return None;
        }
        Some(ts)
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
            Decline::Other.count();
            return FastRead::Core;
        }
        let Some(ts) = self.mark(Watch::Later, wall_ms, now) else { return FastRead::Core };
        // After the bound, as a scan of the one key: when a write counts the key's slot, which
        // other keys share, the read goes on and looks after it at the latched keys themselves.
        let released = self.released.load(Ordering::SeqCst);
        let shared_slot = self.slot_counted(key);
        let view = self.lsm.view_cached(cache);
        let found = match view.get(&self.lsm, key, ts) {
            SharedRead::Hit(f) if f.kind == Kind::Intent => {
                Decline::Intent.count();
                return FastRead::Core;
            }
            SharedRead::Hit(f) => split_value(&f.value).map(|(t, v)| (t, v.to_vec())),
            SharedRead::NotFound => None,
            SharedRead::Need(need) => return FastRead::Need(need),
            SharedRead::Failed(_) => {
                Decline::Other.count();
                return FastRead::Core;
            }
        };
        if shared_slot && self.written_while_read(&(key.to_vec(), [key, &[0]].concat()), released) {
            Decline::Writing.count();
            return FastRead::Core;
        }
        FastRead::Found { ts, value: found }
    }

    /// Whether a write counts the slot of `key`.
    fn slot_counted(&self, key: &[u8]) -> bool {
        // Faulty variant (005 T049): a read outside the core that does not look at the writes.
        #[cfg(teseladb_bug = "outside_ignores_latches")]
        {
            let _ignored = key;
            false
        }
        #[cfg(not(teseladb_bug = "outside_ignores_latches"))]
        {
            self.keys.maybe_held(key)
        }
    }
}

/// Why reads outside the core went back to it, since the process started, by [`Decline`].
static DECLINES: [AtomicU64; DECLINE_REASONS] = [const { AtomicU64::new(0) }; DECLINE_REASONS];
const DECLINE_REASONS: usize = 4;

/// Why a read outside the core went back to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decline {
    /// A write in flight holds a key a point read reads, or was released while it read.
    Writing,
    /// The view's time ran out, or the clock gives no mark yet.
    Expired,
    /// It met an intent.
    Intent,
    /// Anything else: a span past the range, a mark below the GC threshold, a failed read.
    Other,
}

impl Decline {
    /// Every reason, in the order of [`decline_counts`].
    pub const ALL: [Decline; DECLINE_REASONS] =
        [Decline::Writing, Decline::Expired, Decline::Intent, Decline::Other];

    /// Its label in the metrics.
    pub fn label(self) -> &'static str {
        match self {
            Decline::Writing => "writing",
            Decline::Expired => "expired",
            Decline::Intent => "intent",
            Decline::Other => "other",
        }
    }

    fn count(self) {
        DECLINES[self as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// How many reads outside the core went back to it for each reason of [`Decline::ALL`].
pub fn decline_counts() -> [u64; DECLINE_REASONS] {
    std::array::from_fn(|i| DECLINES[i].load(Ordering::Relaxed))
}

/// What [`RangeView::scan_matching`] read.
#[derive(Debug)]
pub struct Matched {
    /// The mark it read at.
    pub ts: Hlc,
    /// The documents its filter accepted, in key order.
    pub rows: Vec<crate::push::Row>,
    /// For each of them, how many documents the scan had examined when it took it.
    pub seen: Vec<usize>,
    /// Documents examined.
    pub examined: usize,
    /// Where the scan goes on when it stopped at its number of documents; `None` at the span's
    /// end.
    pub next: Option<Vec<u8>>,
}

impl RangeView {
    /// The documents of `[start, end)` with their value types, at most `limit` (more than none),
    /// in key order (from the last key down when `backward`, H-003), with the mark they are read
    /// at: the mark of the last entry the range had applied when the view was published (FR-031).
    /// Every write of the range takes a mark above the entries before it and they are applied in
    /// the log's order, so no write in flight is at or below that mark: the scan looks at no
    /// latch and no write sends it to the core. `now` gives the monotonic time, for the view's
    /// validity. A block the shared cache lacks goes to `supply`, which reads it, gives it to
    /// [`SharedLsm::supply`](teseladb_lsm::SharedLsm) and says whether it could; then the scan
    /// goes on. `None` when the scan goes to the core: a span past the range, an expired view, a
    /// write answered to its client before it is applied (DV-010), a mark below the GC
    /// threshold, an intent, a block that could not be read or a failed read. Versions that only
    /// serve the history are no documents, as in the core's scans.
    pub fn scan(
        &self,
        cache: &mut ViewCache,
        (start, end): (&[u8], &[u8]),
        (limit, backward): (usize, bool),
        now: impl FnOnce() -> Nanos,
        concern: ReadConcern,
        mut supply: impl FnMut(&BlockNeed) -> bool,
    ) -> Option<(Hlc, Vec<crate::push::Row>)> {
        let ts = self.applied_for((start, end), limit, now, concern)?;
        let view = self.lsm.view_cached(cache);
        let mut scan = match backward {
            false => Either::Forward(view.scan(start, end, ts)),
            true => Either::Backward(view.scan_back(start, end, ts)),
        };
        let mut rows = Vec::new();
        while rows.len() < limit {
            match scan.next(&self.lsm) {
                SharedStep::Item(i) if i.kind == Kind::Intent => {
                    Decline::Intent.count();
                    return None;
                }
                SharedStep::Item(i) => {
                    if let Some((t, v)) = split_value(&i.value)
                        && t != crate::HISTORY_VALUE
                    {
                        rows.push((i.key, (t, v.to_vec())));
                    }
                }
                SharedStep::Done => break,
                SharedStep::Need(need) if supply(&need) => {}
                SharedStep::Need(_) | SharedStep::Failed(_) => {
                    Decline::Other.count();
                    return None;
                }
            }
        }
        Some((ts, rows))
    }

    /// The mark a scan of `[start, end)` reads at (FR-031), or `None` when it goes to the core:
    /// a span past the range, no limit, a concern the view does not serve, a write answered
    /// before it is applied, a mark below the GC threshold or an expired view.
    fn applied_for(
        &self,
        (start, end): (&[u8], &[u8]),
        limit: usize,
        now: impl FnOnce() -> Nanos,
        concern: ReadConcern,
    ) -> Option<Hlc> {
        let servable = matches!(concern, ReadConcern::Local | ReadConcern::Majority)
            && limit > 0
            && self.start.as_slice() <= start
            && start <= end
            && end <= self.end.as_slice();
        if !servable {
            Decline::Other.count();
            return None;
        }
        // A write its client already has an answer for is not in the view yet.
        if self.early.load(Ordering::SeqCst) != 0 {
            Decline::Writing.count();
            return None;
        }
        let ts = self.applied;
        if ts < self.gc_floor {
            Decline::Other.count();
            return None;
        }
        if now() >= self.valid_until {
            Decline::Expired.count();
            return None;
        }
        Some(ts)
    }

    /// As [`RangeView::scan`] in key order, over at most `examine` documents of which it copies
    /// only those `keep` accepts (010 FR-027): a collection scan with a filter evaluates it on
    /// the stored bytes and leaves the rest where they are.
    pub fn scan_matching(
        &self,
        cache: &mut ViewCache,
        (start, end): (&[u8], &[u8]),
        examine: usize,
        now: impl FnOnce() -> Nanos,
        concern: ReadConcern,
        mut supply: impl FnMut(&BlockNeed) -> bool,
        mut keep: impl FnMut(&[u8]) -> bool,
    ) -> Option<Matched> {
        let ts = self.applied_for((start, end), examine, now, concern)?;
        let view = self.lsm.view_cached(cache);
        let mut scan = view.scan(start, end, ts);
        let mut out = Matched { ts, rows: Vec::new(), seen: Vec::new(), examined: 0, next: None };
        while out.examined < examine {
            match scan.next(&self.lsm) {
                SharedStep::Item(i) if i.kind == Kind::Intent => {
                    Decline::Intent.count();
                    return None;
                }
                SharedStep::Item(i) => {
                    let Some((t, v)) = split_value(&i.value) else { continue };
                    if t == crate::HISTORY_VALUE {
                        continue;
                    }
                    out.examined += 1;
                    let kept = keep(v);
                    if out.examined == examine {
                        out.next = Some([i.key.as_slice(), &[0]].concat());
                    }
                    if kept {
                        out.seen.push(out.examined);
                        out.rows.push((i.key, (t, v.to_vec())));
                    }
                }
                SharedStep::Done => break,
                SharedStep::Need(need) if supply(&need) => {}
                SharedStep::Need(_) | SharedStep::Failed(_) => {
                    Decline::Other.count();
                    return None;
                }
            }
        }
        Some(out)
    }

    /// Whether a write holds a key of `read` or a write with a key of it was released since the
    /// count was `released` (any write, when the core no longer keeps all of those released
    /// since): then the scan goes to the core. Looking at every release of the range sent a scan
    /// of a written collection to the core almost always (H-051 of the native tests).
    fn written_while_read(&self, (start, end): &(Vec<u8>, Vec<u8>), released: u64) -> bool {
        // Faulty variant (005 T049): a read outside the core that does not look at the writes.
        #[cfg(teseladb_bug = "outside_ignores_latches")]
        {
            let _ignored = (start, end, released);
            false
        }
        #[cfg(not(teseladb_bug = "outside_ignores_latches"))]
        {
            if self.spans.any_in(start, end) {
                return true;
            }
            let now = self.released.load(Ordering::SeqCst);
            now != released
                && self.spans.released().any_in((released, now), (start, end)).unwrap_or(true)
        }
    }
}

/// What a mutation answered outside the core gives (005 FR-030).
#[derive(Debug)]
pub enum FastWrite {
    /// It changes nothing: its result, at the mark it was evaluated at.
    Answered {
        /// The mark.
        ts: Hlc,
        /// The mutation's result.
        result: Vec<u8>,
    },
    /// A block to read from its file and give to [`SharedLsm::supply`]; then it runs again.
    Need(BlockNeed),
    /// The mutation goes to the core.
    Core,
}

/// A mutation to answer outside the core if it changes nothing (005 FR-030).
pub struct NoChange<'a> {
    /// The mutation.
    pub mutation: &'a dyn Mutation,
    /// Its keys, which it reads.
    pub keys: &'a [Vec<u8>],
    /// Its statement, for a retryable write.
    pub dedup: Option<DedupKey>,
    /// Its write concern.
    pub wc: WriteConcern,
    /// The cluster time past which it is not evaluated (009 FR-006).
    pub deadline: Option<u64>,
    /// It goes to the core as soon as one of its keys has a version, without being evaluated: a
    /// delete or an update without upsert changes nothing only when its documents are absent,
    /// and evaluating one that changes something here would only repeat the core's work.
    pub absent_only: bool,
}

/// The mark idempotence records are read at: they are never rewritten, so the newest is the one.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

impl RangeView {
    /// Answers `w` on this thread if it changes nothing, as the core answers it without an entry
    /// (005 FR-026, FR-030): at a mark taken as [`RangeView::read`] takes one for its keys and
    /// its idempotence record, with no record of its statement, no intent on its keys, an
    /// evaluation without versions, deletes or other keys to read, and a mark within its
    /// deadline. Anything else goes to the core, which answers as before.
    pub fn no_change(
        &self,
        cache: &mut ViewCache,
        w: &NoChange<'_>,
        wall_ms: u64,
        now: impl FnOnce() -> Nanos,
    ) -> FastWrite {
        let inside = |k: &Vec<u8>| self.start.as_slice() <= k.as_slice() && k < &self.end;
        if !met_without_entry(w.wc) || w.keys.is_empty() || !w.keys.iter().all(inside) {
            return FastWrite::Core;
        }
        let record = w.dedup.map(|k| dedup_key(self.g, k.lsid, k.txn_number, k.stmt_id));
        let watched: Vec<&[u8]> =
            w.keys.iter().map(Vec::as_slice).chain(record.as_deref()).collect();
        let Some(ts) = self.mark(Watch::Keys(&watched), wall_ms, now) else {
            return FastWrite::Core;
        };
        if w.deadline.is_some_and(|d| ts.physical > d) {
            return FastWrite::Core;
        }
        let view = self.lsm.view_cached(cache);
        // A key that may have a version goes to the core without reading a block (H-042): an
        // update or a delete of a document that exists, the usual one, cost a read here only to
        // learn it, and the filters of the LSM's files rule out an absent key on their own.
        if w.absent_only && w.keys.iter().any(|k| view.may_exist(k)) {
            return FastWrite::Core;
        }
        if let Some(r) = &record {
            match view.get(&self.lsm, r, NEWEST) {
                SharedRead::NotFound => {}
                SharedRead::Need(need) => return FastWrite::Need(need),
                // A stored result goes back through the core, which answers it.
                SharedRead::Hit(_) | SharedRead::Failed(_) => return FastWrite::Core,
            }
        }
        let mut current = Vec::with_capacity(w.keys.len());
        for k in w.keys {
            match view.get(&self.lsm, k, ts) {
                SharedRead::Hit(f) if f.kind == Kind::Intent => return FastWrite::Core,
                SharedRead::Hit(f) => {
                    let version = split_value(&f.value).map(|(t, v)| (t, v.to_vec()));
                    if w.absent_only && version.is_some() {
                        return FastWrite::Core;
                    }
                    current.push(version);
                }
                SharedRead::NotFound => current.push(None),
                SharedRead::Need(need) => return FastWrite::Need(need),
                SharedRead::Failed(_) => return FastWrite::Core,
            }
        }
        let current: Vec<Option<(u8, &[u8])>> =
            current.iter().map(|c| c.as_ref().map(|(t, v)| (*t, v.as_slice()))).collect();
        match w.mutation.eval_with(w.keys, &current, ts) {
            Ok(o)
                if o.puts.is_empty()
                    && o.deletes.is_empty()
                    && w.mutation.reads(&o).iter().all(|k| w.keys.contains(k)) =>
            {
                FastWrite::Answered { ts, result: o.result }
            }
            _ => FastWrite::Core,
        }
    }

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
        self.mark(Watch::Range, wall_ms, now)?;
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
    /// The transaction record mark of each range of the core, led or not (008 FR-007, H-040).
    marks: Arc<Published<BTreeMap<GroupId, u64>>>,
}

impl std::fmt::Debug for RangeViews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeViews").finish()
    }
}

impl Default for RangeViews {
    fn default() -> RangeViews {
        RangeViews {
            cell: Arc::new(Published::new(Arc::new(BTreeMap::new()))),
            marks: Arc::new(Published::new(Arc::new(BTreeMap::new()))),
        }
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

    /// The index of the last entry of range `g` that may have changed its transaction records,
    /// as the core published it at its last poll; `None` if the core holds no such range.
    pub fn txn_mark(&self, g: GroupId) -> Option<u64> {
        self.marks.load().get(&g).copied()
    }

    /// The published marks, to compare before publishing again.
    pub(crate) fn txn_marks(&self) -> Arc<BTreeMap<GroupId, u64>> {
        self.marks.load()
    }

    pub(crate) fn publish_txn_marks(&self, marks: BTreeMap<GroupId, u64>) {
        self.marks.store(Arc::new(marks));
    }
}
