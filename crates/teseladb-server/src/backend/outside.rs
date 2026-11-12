//! Reads of one key, and scans of a span one range holds, served on the client worker's thread,
//! without the range's core (005 FR-028, H-013, H-011 and H-009 of the native tests). The worker
//! finds the key's range in the routing table, takes the view its core published and reads the
//! key or the span there; a block the shared cache lacks it reads from its file itself and hands
//! to the cache. Whatever the view cannot serve goes to the core as before: a write holding a
//! latch, an intent, a lease not checked for this moment, a file that cannot be read (the core
//! reads it again and reports its error). A scan whose view lacks a write answered before the
//! scan began waits here for the view that holds it (005 FR-034).
//!
//! The reads served here still count for their range's split by load (007 FR-006): the worker
//! tallies them per range and reports each tally to the core in one message, with a sample of
//! their keys, every [`REPORT_OPS`] reads or [`REPORT_AGE`].

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_hlc::Hlc;
use teseladb_io::runtime::CoreInput;
use teseladb_lsm::{BlockNeed, ViewCache};
use teseladb_range::HISTORY_VALUE;
use teseladb_range::command::DedupKey;
use teseladb_range::host::{Mutation, MutationError};
use teseladb_range::mutate::Outcome;
use teseladb_range::read::ReadConcern;
use teseladb_range::view::{Begun, Decline, FastRead, FastWrite, NoChange, Part, RangeView};
use teseladb_stages::Stage;
use teseladb_wire::commands::mutation::Bound;
use teseladb_wire::commands::{Backend, Rows, WriteOp};
use teseladb_types::{GroupId, Nanos};

use super::{ServerBackend, stage_since};
use crate::logic::NodeMsg;

/// A write's mutation decoded the first time the view evaluates it (H-042): most updates reach a
/// document that exists, and the view sends those to the core after reading their keys, without
/// evaluating them (`absent_only`), so decoding the mutation first was work thrown away.
struct LazyBound<'a> {
    bytes: &'a [u8],
    bound: std::cell::OnceCell<Option<Bound>>,
}

impl<'a> LazyBound<'a> {
    fn new(bytes: &'a [u8]) -> LazyBound<'a> {
        LazyBound { bytes, bound: std::cell::OnceCell::new() }
    }

    fn get(&self) -> Result<&Bound, MutationError> {
        self.bound
            .get_or_init(|| Bound::decode(self.bytes).ok())
            .as_ref()
            .ok_or_else(|| MutationError("a malformed mutation".to_owned()))
    }

    /// Whether it was decoded.
    #[cfg(test)]
    fn decoded(&self) -> bool {
        self.bound.get().is_some()
    }
}

impl Mutation for LazyBound<'_> {
    fn eval(&self, current: &[Option<(u8, &[u8])>], ts: Hlc) -> Result<Outcome, MutationError> {
        self.get()?.eval(current, ts)
    }

    fn eval_with(
        &self,
        keys: &[Vec<u8>],
        current: &[Option<(u8, &[u8])>],
        ts: Hlc,
    ) -> Result<Outcome, MutationError> {
        self.get()?.eval_with(keys, current, ts)
    }

    fn reads(&self, outcome: &Outcome) -> Vec<Vec<u8>> {
        self.get().map_or_else(|_| Vec::new(), |b| b.reads(outcome))
    }
}

/// Blocks one read may read from their files before it goes to the core instead.
const MAX_BLOCKS: usize = 8;
/// Blocks one part of a scan reads from their files before it stops at the documents it has
/// and the scan goes on with another part (005 FR-033): a megabyte of blocks at a time.
const MAX_SCAN_BLOCKS: usize = 64;
/// Files a worker keeps open between reads.
const MAX_FILES: usize = 64;
/// The longest a scan waits on its thread for a view that holds the writes answered before it
/// began (005 FR-034); past it the scan goes to the core, and the worker does not wait for that
/// range again for as long.
const VIEW_WAIT: Duration = Duration::from_millis(50);
/// How long the thread sleeps between two looks at the published view while it waits.
const VIEW_LOOK: Duration = Duration::from_micros(200);
/// Reads of a range a worker tallies before it reports them to the core.
pub(super) const REPORT_OPS: u64 = 64;
/// Longest a tally waits before it is reported.
pub(super) const REPORT_AGE: Duration = Duration::from_millis(10);
/// Keys of each report's sample.
const SAMPLES: usize = 8;

/// Reads served outside the cores, as their tallies are reported.
static SERVED: AtomicU64 = AtomicU64::new(0);
/// Blocks the workers read from their files.
static BLOCKS: AtomicU64 = AtomicU64::new(0);

/// Reads the node's client workers served without the cores since the process started, counted
/// as each tally is reported (every [`REPORT_OPS`] reads or [`REPORT_AGE`] of a range).
pub fn served_outside() -> u64 {
    SERVED.load(Ordering::Relaxed)
}

/// Writes answered on a worker's thread because they changed nothing (005 FR-030).
static UNCHANGED: AtomicU64 = AtomicU64::new(0);

/// Writes that changed nothing the node's client workers answered without the cores since the
/// process started (005 FR-030).
pub fn unchanged_outside() -> u64 {
    UNCHANGED.load(Ordering::Relaxed)
}

/// Reads a range's view sent back to its core since the process started.
static DECLINED: AtomicU64 = AtomicU64::new(0);

/// Reads and scans a range's published view sent back to its core: a write in flight on what
/// they read, an intent, a view past its time, a block that could not be read.
pub fn declined_outside() -> u64 {
    DECLINED.load(Ordering::Relaxed)
}

/// Blocks the client workers read from their files for reads outside the cores.
pub fn blocks_read_outside() -> u64 {
    BLOCKS.load(Ordering::Relaxed)
}

/// `found`, counting a read that goes to the core because the routing table has no range for
/// its key.
fn unrouted<T>(found: Option<T>) -> Option<T> {
    if found.is_none() {
        Decline::Unrouted.count();
    }
    found
}

/// `view`, counting a read that goes to the core because the node publishes no view of its
/// range.
fn unviewed<T>(view: Option<T>) -> Option<T> {
    if view.is_none() {
        Decline::NoView.count();
    }
    view
}

/// What `look` gives once `serves` accepts it, looking again every `every` for at most `most`,
/// and whether it had to look again; past `most`, what `look` gave last. `None` as soon as
/// `look` gives nothing.
fn look_until<T>(
    mut look: impl FnMut() -> Option<T>,
    serves: impl Fn(&T) -> bool,
    (most, every): (Duration, Duration),
) -> Option<(T, bool)> {
    let mut seen = look()?;
    if serves(&seen) {
        return Some((seen, false));
    }
    let began = Instant::now();
    while began.elapsed() < most {
        thread::sleep(every);
        seen = look()?;
        if serves(&seen) {
            break;
        }
    }
    Some((seen, true))
}

/// What a worker keeps for its reads outside the cores.
#[derive(Debug, Default)]
pub(super) struct Outside {
    /// The LSM version of each core this worker read last (by core).
    caches: Vec<ViewCache>,
    /// Files it read blocks from, by the path the core's disk names them with.
    files: BTreeMap<String, File>,
    /// Reads served and not reported yet, by core and range.
    tallies: BTreeMap<(usize, GroupId), Tally>,
    /// The routing table as this worker last read it.
    routes: crate::routes::RouteCache,
    /// The range whose view this worker waited for in vain, and until when it does not wait for
    /// it again (005 FR-034).
    gave_up: Option<(GroupId, Instant)>,
}

impl Outside {
    /// Drops the LSM versions and the open files this worker kept (see `release_outside`).
    fn release(&mut self) {
        self.caches.iter_mut().for_each(ViewCache::release);
        self.files.clear();
    }
}

/// Reads of one range not reported yet.
#[derive(Debug)]
struct Tally {
    ops: u64,
    /// A uniform sample of their keys (reservoir).
    keys: Vec<Vec<u8>>,
    since: Instant,
}

/// The bytes of the block `need` names, from its file under `dir`, keeping the file open in
/// `files`; `None` if they cannot be read here.
fn block(files: &mut BTreeMap<String, File>, dir: &Path, need: &BlockNeed) -> Option<Vec<u8>> {
    if !files.contains_key(&need.path) {
        if files.len() >= MAX_FILES {
            files.clear();
        }
        let file = File::open(dir.join(&need.path)).ok()?;
        files.insert(need.path.clone(), file);
    }
    let file = files.get(&need.path)?;
    let mut buf = vec![0; usize::try_from(need.len).ok()?];
    let got = teseladb_io::read_up_to(file, &mut buf, need.offset).ok()?;
    buf.truncate(got);
    BLOCKS.fetch_add(1, Ordering::Relaxed);
    Some(buf)
}

impl ServerBackend {
    /// The view of range `g` its core `core` published and what a scan that begins now has to
    /// see (005 FR-031). While the view lacks a write answered before the scan began, the thread
    /// waits for one that holds it (FR-034); past [`VIEW_WAIT`] it gives the view as it is,
    /// which sends the scan to the core, and for as long again it does not wait for that range:
    /// the next parts of the scan go straight to the core. `None` when the node publishes no
    /// view of the range.
    fn view_for_scan(&mut self, core: usize, g: GroupId) -> Option<(Arc<RangeView>, Begun)> {
        let views = &self.h.views;
        let look = || unviewed(views.get(core).and_then(|v| v.get(g)));
        let mut first = Some(look()?);
        let begun = first.as_ref()?.begin();
        let began = Instant::now();
        let patient = self.outside.gave_up.is_none_or(|(r, until)| r != g || began >= until);
        let most = if patient { VIEW_WAIT } else { Duration::ZERO };
        let (view, waited) =
            look_until(|| first.take().or_else(look), |v| v.holds(&begun), (most, VIEW_LOOK))?;
        if waited && patient {
            stage_since(Stage::ViewWait, began);
        }
        if waited && !view.holds(&begun) {
            self.outside.gave_up = Some((g, Instant::now() + VIEW_WAIT));
        }
        Some((view, begun))
    }

    /// The document under `key` read on this thread (`Some(None)`: none), or `None` when the read
    /// goes to the key's core.
    pub(super) fn read_outside(
        &mut self,
        key: &[u8],
        concern: ReadConcern,
    ) -> Option<Option<Vec<u8>>> {
        let (g, core) = unrouted(self.h.routes.route_of(&mut self.outside.routes, key))?;
        let core = usize::from(core);
        let view = unviewed(self.h.views.get(core).and_then(|v| v.get(g)))?;
        let Some((value, at)) = self.serve(core, &view, key, concern) else {
            DECLINED.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.tally(core, g, key, at);
        Some(value)
    }

    /// The encoded result of `op` answered on this thread when its documents are absent and it
    /// changes nothing (005 FR-030, H-005 of the native tests), at a mark of the node's clock;
    /// `None` when it goes to its range's core.
    pub(super) fn write_outside(&mut self, op: &WriteOp) -> Option<Vec<u8>> {
        let first = op.keys.first()?;
        let (g, core) = unrouted(self.h.routes.route_of(&mut self.outside.routes, first))?;
        let core = usize::from(core);
        let view = unviewed(self.h.views.get(core).and_then(|v| v.get(g)))?;
        // The mutation as it was sent: its result carries that send's number (`attempt`).
        let bound = LazyBound::new(&op.mutation);
        let dedup = op.dedup.map(|k| DedupKey {
            lsid: k.lsid,
            txn_number: k.txn_number,
            stmt_id: k.stmt_id,
        });
        let deadline = Some(self.deadline_of(self.until()));
        let w = NoChange {
            mutation: &bound,
            keys: &op.keys,
            dedup,
            wc: op.wc,
            deadline,
            absent_only: true,
        };
        if self.outside.caches.len() <= core {
            self.outside.caches.resize_with(core + 1, ViewCache::default);
        }
        let origin = self.h.origin;
        let now = || {
            let since = Instant::now().saturating_duration_since(origin);
            Nanos(u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
        };
        for _ in 0..=MAX_BLOCKS {
            let wall = crate::clock::wall_ms();
            match view.no_change(&mut self.outside.caches[core], &w, wall, now) {
                FastWrite::Answered { ts, result } => {
                    // Its mark comes into the node's clock, as a write's does.
                    self.h.clock.0.update(ts);
                    self.committed = self.committed.max(ts);
                    self.tally(core, g, first, Instant::now());
                    UNCHANGED.fetch_add(1, Ordering::Relaxed);
                    return Some(result);
                }
                FastWrite::Need(need) => {
                    let dir = self.h.data_dir.as_deref()?;
                    let bytes = block(&mut self.outside.files, dir, &need)?;
                    view.lsm().supply(&need, &bytes).ok()?;
                }
                FastWrite::Core => return None,
            }
        }
        None
    }

    fn serve(
        &mut self,
        core: usize,
        view: &RangeView,
        key: &[u8],
        concern: ReadConcern,
    ) -> Option<(Option<Vec<u8>>, Instant)> {
        if self.outside.caches.len() <= core {
            self.outside.caches.resize_with(core + 1, ViewCache::default);
        }
        let origin = self.h.origin;
        // The node's monotonic time, as its cores count it; the view asks once its mark is taken,
        // and the read's tally counts it at that same instant.
        let asked = std::cell::Cell::new(None);
        let now = || {
            let at = Instant::now();
            asked.set(Some(at));
            let since = at.saturating_duration_since(origin);
            Nanos(u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
        };
        for _ in 0..=MAX_BLOCKS {
            let wall = crate::clock::wall_ms();
            match view.read(&mut self.outside.caches[core], key, wall, now, concern) {
                // A version that only serves the history is no document, as in the core's reads.
                FastRead::Found { value, .. } => {
                    let value = value.filter(|(t, _)| *t != HISTORY_VALUE).map(|(_, v)| v);
                    return Some((value, asked.get().unwrap_or_else(Instant::now)));
                }
                FastRead::Need(need) => {
                    let dir = self.h.data_dir.as_deref()?;
                    let bytes = block(&mut self.outside.files, dir, &need)?;
                    view.lsm().supply(&need, &bytes).ok()?;
                }
                FastRead::Core => return None,
            }
        }
        None
    }

    /// The documents of `[start, end)` that one range holds, at most `limit`, read on this thread
    /// when its view serves them (005 FR-028, H-011, H-009 and H-003 of the native tests): from
    /// `start` up, or from `end` down when `backward`; with where that range's part of the span
    /// ends (or starts, backward). `None` when the read goes to the core. The
    /// blocks it lacks it reads from their files, at most [`MAX_SCAN_BLOCKS`].
    pub(super) fn scan_outside(
        &mut self,
        (start, end): (&[u8], &[u8]),
        (limit, backward): (usize, bool),
        concern: ReadConcern,
    ) -> Option<(Vec<u8>, Rows)> {
        // Forward, the part of the range of `start`; backward, of the range of the keys just
        // below `end` (H-003). The boundary returned is where the next part starts or ends.
        let (g, core, (lo, hi)) = match backward {
            false => {
                let (g, core, range_end) =
                    unrouted(self.h.routes.span_of(&mut self.outside.routes, start))?;
                let hi = match range_end.is_empty() || end <= range_end {
                    true => end.to_vec(),
                    false => range_end.to_vec(),
                };
                (g, core, (start.to_vec(), hi))
            }
            true => {
                let (g, core, range_start) =
                    unrouted(self.h.routes.span_below(&mut self.outside.routes, end))?;
                (g, core, (range_start.max(start).to_vec(), end.to_vec()))
            }
        };
        let core = usize::from(core);
        let (view, begun) = self.view_for_scan(core, g)?;
        if self.outside.caches.len() <= core {
            self.outside.caches.resize_with(core + 1, ViewCache::default);
        }
        let origin = self.h.origin;
        let asked = std::cell::Cell::new(None);
        let now = || {
            let at = Instant::now();
            asked.set(Some(at));
            let since = at.saturating_duration_since(origin);
            Nanos(u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
        };
        let dir = self.h.data_dir.clone();
        let Outside { caches, files, .. } = &mut self.outside;
        let supply = |need: &BlockNeed| {
            let Some(dir) = dir.as_deref() else { return false };
            block(files, dir, need).is_some_and(|b| view.lsm().supply(need, &b).is_ok())
        };
        let span = (lo.as_slice(), hi.as_slice());
        let part = (Part { docs: limit, backward, blocks: MAX_SCAN_BLOCKS }, &begun);
        let Some(read) = view.scan_begun(&mut caches[core], span, part, now, concern, supply)
        else {
            DECLINED.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.tally(core, g, &lo, asked.get().unwrap_or_else(Instant::now));
        // A part that stopped at its blocks goes on from there (005 FR-033); any other ends at
        // the range's end, or at its number of documents.
        let boundary = read.next.unwrap_or(if backward { lo } else { hi });
        Some((boundary, read.rows.into_iter().map(|(k, (_, v))| (k, v)).collect()))
    }

    /// As [`ServerBackend::scan_outside`] in key order, over at most `examine` documents of the
    /// part of the span in the range of `start`, of which only those `keep` accepts are copied
    /// (010 FR-027); with where the scan goes on. `None` when the read goes to the core.
    pub(super) fn scan_matching_outside(
        &mut self,
        (start, end): (&[u8], &[u8]),
        examine: usize,
        concern: ReadConcern,
        keep: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Option<teseladb_wire::commands::Matching> {
        let (g, core, range_end) =
            unrouted(self.h.routes.span_of(&mut self.outside.routes, start))?;
        let hi = match range_end.is_empty() || end <= range_end {
            true => end.to_vec(),
            false => range_end.to_vec(),
        };
        let core = usize::from(core);
        let (view, begun) = self.view_for_scan(core, g)?;
        if self.outside.caches.len() <= core {
            self.outside.caches.resize_with(core + 1, ViewCache::default);
        }
        let origin = self.h.origin;
        let asked = std::cell::Cell::new(None);
        let now = || {
            let at = Instant::now();
            asked.set(Some(at));
            let since = at.saturating_duration_since(origin);
            Nanos(u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
        };
        let dir = self.h.data_dir.clone();
        let Outside { caches, files, .. } = &mut self.outside;
        let supply = |need: &BlockNeed| {
            let Some(dir) = dir.as_deref() else { return false };
            block(files, dir, need).is_some_and(|b| view.lsm().supply(need, &b).is_ok())
        };
        let span = (start, hi.as_slice());
        let most = (examine, MAX_SCAN_BLOCKS, &begun);
        let cache = &mut caches[core];
        let Some(m) = view.scan_matching_begun(cache, span, most, now, concern, supply, keep)
        else {
            DECLINED.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.tally(core, g, start, asked.get().unwrap_or_else(Instant::now));
        // It stopped at its number of documents, or at the end of this range's part: the span
        // goes on in the next range unless that was its end.
        let next = m.next.or_else(|| (hi.as_slice() < end).then_some(hi));
        Some(teseladb_wire::commands::Matching {
            rows: m.rows.into_iter().map(|(k, (_, v))| (k, v)).collect(),
            seen: m.seen,
            examined: m.examined,
            next,
        })
    }

    /// The live documents of collection `coll` in `[start, end)`, its documents' keys, added up
    /// from the counts each range's view carries, on this thread (H-004 of the native tests,
    /// 005 FR-028); `None` when a range has no view, its counts are estimated or a write holds
    /// a latch, and the count goes to the cores.
    pub(super) fn count_outside(
        &mut self,
        coll: (u32, u32),
        (start, end): (&[u8], &[u8]),
    ) -> Option<u64> {
        let origin = self.h.origin;
        // The monotonic time each view asks for once its mark is taken, which its tally counts
        // too; one wall clock reading for all the marks, as the HLC never goes below its own.
        let asked = std::cell::Cell::new(None);
        let now = || {
            let at = Instant::now();
            asked.set(Some(at));
            Nanos(
                u64::try_from(at.saturating_duration_since(origin).as_nanos()).unwrap_or(u64::MAX),
            )
        };
        let wall = crate::clock::wall_ms();
        let mut total = 0u64;
        let mut from = start.to_vec();
        loop {
            let (g, core, range_end) = self.h.routes.span_of(&mut self.outside.routes, &from)?;
            let range_end = range_end.to_vec();
            let Some(view) = self.h.views.get(usize::from(core)).and_then(|v| v.get(g)) else {
                DECLINED.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            let Some(n) = view.count(coll, (wall, &now), ReadConcern::Local) else {
                DECLINED.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            total = total.saturating_add(n);
            self.tally(usize::from(core), g, &from, asked.get().unwrap_or_else(Instant::now));
            if range_end.is_empty() || range_end.as_slice() >= end {
                return Some(total);
            }
            from = range_end;
        }
    }

    /// Counts a read of `key` served outside core `core` for range `g`'s load.
    fn tally(&mut self, core: usize, g: GroupId, key: &[u8], now: Instant) {
        let r = Backend::random(self);
        let t = self.outside.tallies.entry((core, g)).or_insert_with(|| Tally {
            ops: 0,
            keys: Vec::with_capacity(SAMPLES),
            since: now,
        });
        t.ops += 1;
        if t.keys.len() < SAMPLES {
            t.keys.push(key.to_vec());
        } else if let Some(slot) = usize::try_from(r % t.ops).ok().and_then(|j| t.keys.get_mut(j)) {
            *slot = key.to_vec();
        }
        let due = t.ops >= REPORT_OPS || now.saturating_duration_since(t.since) >= REPORT_AGE;
        if due && let Some(t) = self.outside.tallies.remove(&(core, g)) {
            self.report(core, g, t);
        }
    }

    /// Lets go of what the worker kept for its reads outside the cores once an operation ends:
    /// the LSM versions and the open files. A connection that waits in a client's pool would
    /// otherwise keep files that compactions replaced, and the disk space of a deleted file
    /// that is still open (H-052).
    pub(super) fn release_outside(&mut self) {
        self.outside.release();
    }

    /// Reports every tally not reported yet (when the worker ends).
    pub(super) fn report_outside(&mut self) {
        for ((core, g), t) in std::mem::take(&mut self.outside.tallies) {
            self.report(core, g, t);
        }
    }

    fn report(&self, core: usize, g: GroupId, t: Tally) {
        SERVED.fetch_add(t.ops, Ordering::Relaxed);
        // A full mailbox drops the report: the load is a sample of the range's traffic, and a
        // worker that waited for a busy core would slow the reads this path makes fast.
        if let Some(tx) = self.cores.get(core) {
            let _ = tx.send(CoreInput::App(NodeMsg::OutsideLoad(g, t.ops, t.keys)));
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod release_tests {
    use super::*;
    use teseladb_lsm::{Lsm, LsmConfig};
    use teseladb_raftlog::MemDisk;

    #[test]
    fn a_worker_lets_go_of_its_versions_and_files() {
        // H-052: a connection waiting in a client's pool kept the version of each core it read
        // last and the files it had open, so replaced files stayed on disk.
        let cfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
        let mut disk = MemDisk::new();
        let (lsm, _) = Lsm::recover(&cfg, &mut disk).expect("recovery");
        lsm.publish();
        let shared = lsm.shared();
        let mut outside = Outside::default();
        outside.caches.resize_with(2, ViewCache::default);
        shared.view_cached(&mut outside.caches[1]);
        let file = std::env::current_exe().expect("this test's file");
        let open = File::open(&file).expect("open");
        outside.files.insert(file.display().to_string(), open);
        assert!(outside.caches[1].holds());
        outside.release();
        assert!(outside.caches.iter().all(|c| !c.holds()), "no version held");
        assert!(outside.files.is_empty(), "no file open");
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod wait_tests {
    use super::*;

    const EVERY: Duration = Duration::from_micros(100);
    const LONG: Duration = Duration::from_secs(5);

    #[test]
    fn what_serves_at_once_is_given_without_another_look() {
        let mut looks = 0;
        let look = || {
            looks += 1;
            Some(7)
        };
        let got = look_until(look, |v| *v == 7, (LONG, EVERY));
        assert_eq!((got, looks), (Some((7, false)), 1));
    }

    #[test]
    fn it_looks_again_until_what_it_sees_serves() {
        // 005 FR-034: the third view the core publishes holds what the scan has to see.
        let mut looks = 0;
        let look = || {
            looks += 1;
            Some(looks)
        };
        assert_eq!(look_until(look, |v| *v >= 3, (LONG, EVERY)), Some((3, true)));
    }

    #[test]
    fn past_its_time_it_gives_what_it_saw_last_and_with_no_time_it_does_not_wait() {
        let (began, most) = (Instant::now(), Duration::from_millis(5));
        let mut looks = 0;
        let look = || {
            looks += 1;
            Some(looks)
        };
        let got = look_until(look, |_| false, (most, EVERY));
        assert!(began.elapsed() >= most);
        assert!(looks >= 2, "{looks} looks");
        assert_eq!(got, Some((looks, true)));
        // A worker that gave up on a range a moment ago takes its view as it is.
        let mut looks = 0;
        let look = || {
            looks += 1;
            Some(looks)
        };
        let got = look_until(look, |_| false, (Duration::ZERO, EVERY));
        assert_eq!((got, looks), (Some((1, true)), 1));
    }

    #[test]
    fn a_range_whose_view_goes_ends_the_wait() {
        let mut looks = 0;
        let look = || {
            looks += 1;
            (looks < 2).then_some(looks)
        };
        let got = look_until(look, |_| false, (LONG, EVERY));
        assert_eq!((got, looks), (None, 2));
        assert_eq!(look_until(|| None::<u8>, |_| true, (LONG, EVERY)), None);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod lazy_tests {
    use super::*;

    #[test]
    fn a_mutation_is_decoded_only_when_it_is_evaluated() {
        // H-042: bytes that do not decode fail only once something evaluates them.
        let lazy = LazyBound::new(b"not a mutation");
        assert!(!lazy.decoded(), "nothing asked for it");
        let none = Outcome { puts: vec![], deletes: vec![], change: vec![], result: vec![] };
        assert!(lazy.reads(&none).is_empty());
        assert!(lazy.decoded());
        let ts = Hlc { physical: 1, logical: 0 };
        assert!(lazy.eval(&[], ts).is_err(), "malformed");
        assert!(lazy.eval_with(&[], &[], ts).is_err());
    }
}
