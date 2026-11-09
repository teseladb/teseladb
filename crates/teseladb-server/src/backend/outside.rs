//! Reads of one key, and scans of a span one range holds, served on the client worker's thread,
//! without the range's core (005 FR-028, H-013, H-011 and H-009 of the native tests). The worker
//! finds the key's range in the routing table, takes the view its core published and reads the
//! key or the span there; a block the shared cache lacks it reads from its file itself and hands
//! to the cache. Whatever the view cannot serve goes to the core as before: a write holding a
//! latch, an intent, a lease not checked for this moment, a file that cannot be read (the core
//! reads it again and reports its error).
//!
//! The reads served here still count for their range's split by load (007 FR-006): the worker
//! tallies them per range and reports each tally to the core in one message, with a sample of
//! their keys, every [`REPORT_OPS`] reads or [`REPORT_AGE`].

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use teseladb_io::runtime::CoreInput;
use teseladb_lsm::{BlockNeed, ViewCache};
use teseladb_range::HISTORY_VALUE;
use teseladb_range::read::ReadConcern;
use teseladb_range::view::{FastRead, RangeView};
use teseladb_wire::commands::{Backend, Rows};
use teseladb_types::{GroupId, Nanos};

use super::ServerBackend;
use crate::logic::NodeMsg;

/// Blocks one read may read from their files before it goes to the core instead.
const MAX_BLOCKS: usize = 8;
/// Blocks one scan may read from their files before it goes to the core instead: a scan of a
/// YCSB-E length (up to 100 documents) spans a few blocks of each table it reads.
const MAX_SCAN_BLOCKS: usize = 64;
/// Files a worker keeps open between reads.
const MAX_FILES: usize = 64;
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

/// Blocks the client workers read from their files for reads outside the cores.
pub fn blocks_read_outside() -> u64 {
    BLOCKS.load(Ordering::Relaxed)
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
    /// The document under `key` read on this thread (`Some(None)`: none), or `None` when the read
    /// goes to the key's core.
    pub(super) fn read_outside(
        &mut self,
        key: &[u8],
        concern: ReadConcern,
    ) -> Option<Option<Vec<u8>>> {
        let (g, core) = self.h.routes.route_of(&mut self.outside.routes, key)?;
        let core = usize::from(core);
        let view = self.h.views.get(core)?.get(g)?;
        let (value, at) = self.serve(core, &view, key, concern)?;
        self.tally(core, g, key, at);
        Some(value)
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
                    self.h.routes.span_of(&mut self.outside.routes, start)?;
                let hi = match range_end.is_empty() || end <= range_end {
                    true => end.to_vec(),
                    false => range_end.to_vec(),
                };
                (g, core, (start.to_vec(), hi))
            }
            true => {
                let (g, core, range_start) =
                    self.h.routes.span_below(&mut self.outside.routes, end)?;
                (g, core, (range_start.max(start).to_vec(), end.to_vec()))
            }
        };
        let core = usize::from(core);
        let view = self.h.views.get(core)?.get(g)?;
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
        let mut blocks = 0;
        let supply = |need: &BlockNeed| {
            blocks += 1;
            let Some(dir) = dir.as_deref().filter(|_| blocks <= MAX_SCAN_BLOCKS) else {
                return false;
            };
            block(files, dir, need).is_some_and(|b| view.lsm().supply(need, &b).is_ok())
        };
        let clocks = (crate::clock::wall_ms(), now);
        let span = (lo.as_slice(), hi.as_slice());
        let way = (limit, backward);
        let (_, rows) = view.scan(&mut caches[core], span, way, clocks, concern, supply)?;
        self.tally(core, g, &lo, asked.get().unwrap_or_else(Instant::now));
        let boundary = if backward { lo } else { hi };
        Some((boundary, rows.into_iter().map(|(k, (_, v))| (k, v)).collect()))
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
