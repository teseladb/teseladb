//! The documents of an index scan's entries read together (010 FR-027): a key whose range
//! published a view on this node is read here, outside the cores (005 FR-028); the others go by
//! range, in batches of at most [`BATCH`] keys, all at once to the range's core, which reads them
//! as the leader or sends the batch on to the leader on another node, with the residual filter
//! when the plan has one that may travel. A batch that does not come back, and each key a leader
//! did not answer, is read on its own with every rule of a single read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use teseladb_io::engine::answer as mpsc;

use teseladb_io::engine::CoreInput;
use teseladb_range::read::ReadConcern;
use teseladb_types::GroupId;
use teseladb_wire::commands::Fetched;
use teseladb_wire::error::CommandError;

use super::ServerBackend;
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::pushed::{Fallback, note_fallback};

/// The most keys of one batch.
const BATCH: usize = 128;
/// The fewest keys of one range read with one scan of their span ([`ServerBackend::fetch_run`]).
const RUN_MIN: usize = 4;
/// The documents such a scan may read for each key it looks for: once a span has given more
/// than this many documents a key, its keys are read one by one.
const RUN_SPARSE: usize = 3;
/// The most documents of the first part of such a scan, which tells whether the keys lie
/// together before much is read; each part after it doubles, up to [`RUN_PART`].
const RUN_PROBE: usize = 32;
/// The most documents of one part of such a scan.
const RUN_PART: usize = super::CORE_SCAN_PART;

/// The fewest keys of a batch whose ranges are read each on the shard of its core, all at once
/// ([`ServerBackend::fetch_spread`]): below it, posting the parts costs more than reading them
/// one after the other on this thread.
pub(super) const SPREAD_MIN: usize = 64;

/// Batches of keys sent to the core of their range.
static BATCHES: AtomicU64 = AtomicU64::new(0);
/// Keys those batches carried.
static KEYS_SENT: AtomicU64 = AtomicU64::new(0);
/// Keys sent with the residual filter.
static KEYS_FILTERED: AtomicU64 = AtomicU64::new(0);
/// Keys read on this node outside the cores.
static KEYS_HERE: AtomicU64 = AtomicU64::new(0);
/// Keys among those read with a scan of their span.
static KEYS_IN_RUNS: AtomicU64 = AtomicU64::new(0);

/// Keys among those read here that another shard read, at once with the other shards' (010
/// FR-027).
pub(super) static KEYS_SPREAD: AtomicU64 = AtomicU64::new(0);

/// Document keys of index scans read on the shard of their range's core while the other
/// ranges' were read on theirs, since the process started (010 FR-027).
pub fn lookup_keys_spread() -> u64 {
    KEYS_SPREAD.load(Ordering::Relaxed)
}

/// Batches of document keys of index scans this node sent to their range since the process
/// started (010 FR-027).
pub fn lookup_batches_sent() -> u64 {
    BATCHES.load(Ordering::Relaxed)
}

/// Document keys of index scans those batches carried since the process started.
pub fn lookup_keys_sent() -> u64 {
    KEYS_SENT.load(Ordering::Relaxed)
}

/// Document keys of index scans sent with the residual filter for the leader to evaluate, since
/// the process started.
pub fn lookup_keys_filtered() -> u64 {
    KEYS_FILTERED.load(Ordering::Relaxed)
}

/// Document keys of index scans read on this node outside the cores since the process started.
pub fn lookup_keys_here() -> u64 {
    KEYS_HERE.load(Ordering::Relaxed)
}

/// Document keys among those read with one scan forward of their span, since the process
/// started.
pub fn lookup_keys_in_runs() -> u64 {
    KEYS_IN_RUNS.load(Ordering::Relaxed)
}

/// A batch sent: the places of its keys and its answer to come.
type Waiting = (Vec<usize>, mpsc::Receiver<Result<DataReply, teseladb_range::host::RangeError>>);

impl ServerBackend {
    /// Reads the keys of `keys` that lie together with one scan forward of their span, on this
    /// thread from the views of their ranges, and leaves what it read in `found`: the keys are
    /// taken in their order range by range (a collection spread over partitions has a client's
    /// documents together in each one), and each range's keys, when they are [`RUN_MIN`] or
    /// more, go to [`ServerBackend::fetch_run`]. What it leaves empty is read key by key.
    fn fetch_runs(
        &mut self,
        keys: &[Vec<u8>],
        concern: ReadConcern,
        found: &mut [Option<Fetched>],
    ) {
        if keys.len() < RUN_MIN {
            return;
        }
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_unstable_by(|a, b| keys[*a].cmp(&keys[*b]));
        let mut at = 0;
        while at < order.len() {
            let Some(range_end) = self.range_end_of(&keys[order[at]]) else {
                at += 1;
                continue;
            };
            let in_range = |k: &[u8]| range_end.is_empty() || k < range_end.as_slice();
            let mut to = at + 1;
            while to < order.len() && in_range(keys[order[to]].as_slice()) {
                to += 1;
            }
            if to - at >= RUN_MIN {
                self.fetch_run(keys, &order[at..to], concern, found);
            }
            at = to;
        }
    }

    /// Reads the stored documents of the keys at `run`, which ascend and are of one range, with
    /// a scan forward from the first to the last, and leaves them in `found`. A point read of
    /// each cost ten times a scan's document (002 runs p51 and p52: 3.5 µs of CPU against
    /// 0.35). It leaves `found` as it was, for the keys to be read one by one, when a part of
    /// their span goes to its core or the span holds more than [`RUN_SPARSE`] documents a key,
    /// which its first part of [`RUN_PROBE`] documents already tells.
    fn fetch_run(
        &mut self,
        keys: &[Vec<u8>],
        run: &[usize],
        concern: ReadConcern,
        found: &mut [Option<Fetched>],
    ) {
        let key_at = |i: usize| keys[run[i]].as_slice();
        let end = [key_at(run.len() - 1), &[0]].concat();
        let mut from = key_at(0).to_vec();
        let mut docs: Vec<Option<Vec<u8>>> = Vec::with_capacity(run.len());
        let (mut want, mut read) = (RUN_PROBE.min(run.len() * RUN_SPARSE), 0usize);
        while docs.len() < run.len() && from.as_slice() < end.as_slice() {
            let Some((boundary, rows)) = self.scan_outside((&from, &end), (want, false), concern)
            else {
                return;
            };
            read += rows.len();
            from = super::next_from(boundary, &rows, want);
            want = (want * 2).min(RUN_PART);
            for (key, doc) in rows {
                while docs.len() < run.len() && key_at(docs.len()) < key.as_slice() {
                    docs.push(None);
                }
                if docs.len() < run.len() && key_at(docs.len()) == key.as_slice() {
                    // A key asked for twice takes the document twice.
                    while docs.len() + 1 < run.len() && key_at(docs.len() + 1) == key.as_slice() {
                        docs.push(Some(doc.clone()));
                    }
                    docs.push(Some(doc));
                }
            }
            // The span holds much else between the keys: they are read one by one.
            if read > RUN_SPARSE * (docs.len() + 1) {
                return;
            }
        }
        // The keys past the last document of the span hold nothing.
        docs.resize(run.len(), None);
        let n = u64::try_from(run.len()).unwrap_or(u64::MAX);
        KEYS_HERE.fetch_add(n, Ordering::Relaxed);
        KEYS_IN_RUNS.fetch_add(n, Ordering::Relaxed);
        for (place, doc) in run.iter().zip(docs) {
            found[*place] = Some(doc.map_or(Fetched::Missing, Fetched::Doc));
        }
    }

    /// What this thread reads of `keys` from the views of their ranges, in their places: those
    /// that lie together with one scan forward of their span, the others one by one; `None`
    /// for a key whose read goes to its range's core.
    pub(super) fn fetch_here(
        &mut self,
        keys: &[Vec<u8>],
        concern: ReadConcern,
    ) -> Vec<Option<Fetched>> {
        let mut found: Vec<Option<Fetched>> = vec![None; keys.len()];
        self.fetch_runs(keys, concern, &mut found);
        for (i, key) in keys.iter().enumerate() {
            if found[i].is_some() {
                continue;
            }
            // A long batch of point reads gives the shard back once its quota is used up.
            teseladb_io::engine::wait::yield_if_asked();
            if let Some(value) = self.read_outside(key, concern) {
                KEYS_HERE.fetch_add(1, Ordering::Relaxed);
                found[i] = Some(value.map_or(Fetched::Missing, Fetched::Doc));
            }
        }
        found
    }

    /// [`teseladb_wire::commands::Backend::fetch_documents`]: `None` for a read at a command's
    /// mark or with a read preference, which read key by key.
    pub(super) fn fetch_together(
        &mut self,
        keys: &[Vec<u8>],
        filter: Option<&[u8]>,
        concern: ReadConcern,
    ) -> Result<Option<Vec<Fetched>>, CommandError> {
        let primary = self.read_pref.as_ref().is_none_or(|p| p.mode == super::Mode::Primary);
        if self.read_mark.is_some() || !primary {
            return Ok(None);
        }
        let mut found: Vec<Option<Fetched>> = vec![None; keys.len()];
        // The keys of several cores are read on their shards at once; any other batch here,
        // those that lie together in a range with one scan forward of their span.
        if !self.fetch_spread(keys, concern, &mut found) {
            self.fetch_runs(keys, concern, &mut found);
        }
        let mut by_range: Vec<((GroupId, u16), Vec<usize>)> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if found[i].is_some() {
                continue;
            }
            // A long batch of point reads gives the shard back once its quota is used up.
            teseladb_io::engine::wait::yield_if_asked();
            if let Some(value) = self.read_outside(key, concern) {
                KEYS_HERE.fetch_add(1, Ordering::Relaxed);
                found[i] = Some(value.map_or(Fetched::Missing, Fetched::Doc));
                continue;
            }
            let Some(e) = self.h.routes.lookup(key) else { continue };
            match by_range.iter_mut().find(|(r, _)| *r == (e.group, e.core)) {
                Some((_, places)) => places.push(i),
                None => by_range.push(((e.group, e.core), vec![i])),
            }
        }
        let waiting: Vec<Waiting> = by_range
            .iter()
            .flat_map(|(range, places)| places.chunks(BATCH).map(move |c| (*range, c)))
            .filter_map(|(range, places)| self.send_batch(range, (keys, places), filter, concern))
            .collect();
        let until = self.until();
        for (places, answer) in waiting {
            let wait = until.saturating_duration_since(Instant::now());
            match answer.recv_timeout(wait) {
                Ok(Ok(DataReply::Remote(RemoteReply::Looked(got))))
                    if got.len() == places.len() =>
                {
                    // A leader that could not evaluate the filter sent the documents to decide here.
                    let undecided = got.iter().any(|f| matches!(f, Some(Fetched::Doc(_))));
                    if filter.is_some() && undecided {
                        note_fallback(Fallback::Lookup);
                    }
                    for (i, f) in places.into_iter().zip(got) {
                        found[i] = f;
                    }
                }
                _ => note_fallback(Fallback::Lookup),
            }
        }
        // The documents go on without their tables (003 FR-021), as from a point read.
        let document = |key: &[u8], doc| teseladb_keys::stored::document_value(key, doc);
        keys.iter()
            .zip(found)
            .map(|(key, f)| match f {
                Some(Fetched::Doc(doc)) => Ok(Fetched::Doc(document(key, doc))),
                Some(Fetched::Kept(doc)) => Ok(Fetched::Kept(document(key, doc))),
                Some(f) => Ok(f),
                None => {
                    let doc = self.point_on_leader(key, concern)?;
                    Ok(doc.map_or(Fetched::Missing, |doc| Fetched::Doc(document(key, doc))))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// Sends the keys of `places` to the core of `range`, with `filter`; `None` when the core
    /// does not take it.
    fn send_batch(
        &self,
        (group, core): (GroupId, u16),
        (keys, places): (&[Vec<u8>], &[usize]),
        filter: Option<&[u8]>,
        concern: ReadConcern,
    ) -> Option<Waiting> {
        let req = RemoteReq::Lookup {
            keys: places.iter().map(|&i| keys[i].clone()).collect(),
            concern,
            filter: filter.map(<[u8]>::to_vec),
        };
        let (tx, rx) = mpsc::channel();
        let msg = NodeMsg::Data(DataReq::Remote(group, req), tx);
        let sent =
            self.cores.get(usize::from(core)).is_some_and(|c| c.send(CoreInput::App(msg)).is_ok());
        if !sent {
            note_fallback(Fallback::Lookup);
            return None;
        }
        BATCHES.fetch_add(1, Ordering::Relaxed);
        KEYS_SENT.fetch_add(places.len() as u64, Ordering::Relaxed);
        if filter.is_some() {
            KEYS_FILTERED.fetch_add(places.len() as u64, Ordering::Relaxed);
        }
        Some((places.to_vec(), rx))
    }
}
