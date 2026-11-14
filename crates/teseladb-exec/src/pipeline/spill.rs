//! Stages that write to disk what passes their memory limit (010 FR-010, T024): with
//! `allowDiskUse`, a `$sort` over more bytes than its limit sorts its input in runs of at most
//! the limit, writes each run to the node's temporary store ([`SpillStore`], behind the
//! [`super::Env`]) and merges the runs reading them back a piece at a time. Equal keys keep
//! their input order, as in memory: the runs are cut in input order and the merge takes the
//! earlier run first on a tie. `$group` sorts its documents by key the same way and builds one
//! group at a time; `$bucketAuto` sorts by its `groupBy` values and `$setWindowFields` by
//! partition and `sortBy` ([`external_sort`]).

use std::cmp::Ordering;
use std::fmt::Debug;

use teseladb_mql::{MqlError, codes};
use teseladb_bson::{Collator, Document, RawDoc, Value};

use super::errors::{self, document_size};
use super::sort::SortSpec;

std::thread_local! {
    /// The runs this thread wrote, for what a command reports (`usedDisk` of `$queryStats`).
    static WRITTEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The runs stages on this thread have written so far; a command compares it before and after
/// it runs.
pub fn runs_written() -> u64 {
    WRITTEN.with(std::cell::Cell::get)
}

/// The most bytes a merge reads from a run at a time.
const READ_BYTES: usize = 64 * 1024;

/// The fewest bytes a merge reads from a run at a time, when many runs share the limit.
const MIN_READ_BYTES: usize = 1024;

/// The node's temporary store for what stages write to disk; sans-I/O, the server gives one
/// over its files.
pub trait SpillStore: Debug {
    /// Writes `bytes` as a new run; its number.
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError>;
    /// Up to `len` bytes of run `run` from `offset` (fewer at its end).
    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError>;
    /// Drops run `run`.
    fn remove(&self, run: u64);

    /// Writes `bytes` as a new run that a stage may still drop without having used it: what a
    /// store reports counts it only after [`SpillStore::count`]. By default, as
    /// [`SpillStore::write`], for stores that report nothing.
    fn write_unseen(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        self.write(bytes)
    }

    /// Counts as written the runs of `stats`, written before with [`SpillStore::write_unseen`].
    fn count(&self, _stats: SpillStats) {}
}

/// A run being read back: its documents a piece at a time, from the pieces of the store that
/// hold it one after the other.
struct Reader<'a> {
    store: &'a dyn SpillStore,
    /// The pieces of the run, and the place of the next one to read.
    pieces: &'a [u64],
    next: usize,
    run: u64,
    offset: u64,
    buf: Vec<u8>,
    at: usize,
    done: bool,
    /// The bytes it reads at a time.
    step: usize,
}

impl<'a> Reader<'a> {
    /// A reader of the pieces `run`, `step` bytes at a time.
    fn of(store: &'a dyn SpillStore, pieces: &'a [u64], step: usize) -> Reader<'a> {
        let (run, done) = match pieces.first() {
            Some(&id) => (id, false),
            None => (0, true),
        };
        Reader { store, pieces, next: 1, run, offset: 0, buf: Vec::new(), at: 0, done, step }
    }

    /// The next document of the run, if any.
    fn next_doc(&mut self) -> Result<Option<Document>, MqlError> {
        loop {
            let left = &self.buf[self.at..];
            if let Some(len) = left.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) {
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                if left.len() >= len {
                    let doc = RawDoc::from_bytes(&left[..len]).map_err(|e| {
                        errors::error(codes::INTERNAL_ERROR, format!("a spilled run: {e}"))
                    })?;
                    self.at += len;
                    return Ok(Some(Document::from(doc)));
                }
            }
            if self.done {
                if !left.is_empty() {
                    return Err(errors::error(codes::INTERNAL_ERROR, "a spilled run is cut"));
                }
                let Some(&run) = self.pieces.get(self.next) else { return Ok(None) };
                (self.run, self.offset, self.done, self.next) = (run, 0, false, self.next + 1);
            }
            let more = self.store.read(self.run, self.offset, self.step)?;
            self.done = more.len() < self.step;
            self.offset += u64::try_from(more.len()).unwrap_or(u64::MAX);
            self.buf.drain(..self.at);
            self.at = 0;
            self.buf.extend_from_slice(&more);
        }
    }
}

/// The runs of a sort, each the pieces of the store that hold it in order; it removes them when
/// it goes, also on an error.
struct Runs<'a> {
    store: &'a dyn SpillStore,
    ids: Vec<Vec<u64>>,
}

impl Drop for Runs<'_> {
    fn drop(&mut self) {
        for id in self.ids.iter().flatten() {
            self.store.remove(*id);
        }
    }
}

/// Writes `docs`, sorted by `cmp` over their keys `keys`, as one run; with `unseen`, as a run
/// the store does not count yet, whose figures it adds there.
fn write_run<K>(
    docs: Vec<Document>,
    keys: &dyn Fn(&Document) -> Result<K, MqlError>,
    cmp: &dyn Fn(&K, &K) -> Result<Ordering, MqlError>,
    (store, unseen): (&dyn SpillStore, Option<&mut SpillStats>),
) -> Result<u64, MqlError> {
    let records = u64::try_from(docs.len()).unwrap_or(u64::MAX);
    let mut keyed = Vec::with_capacity(docs.len());
    for d in docs {
        keyed.push((keys(&d)?, d));
    }
    crate::expr::try_sort_by(&mut keyed, |(a, _), (b, _)| cmp(a, b))?;
    let mut bytes = Vec::new();
    for (_, d) in keyed {
        let b = d.to_bytes().map_err(|e| errors::error(codes::INTERNAL_ERROR, e.to_string()))?;
        bytes.extend_from_slice(&b);
    }
    let Some(unseen) = unseen else {
        let id = store.write(&bytes)?;
        WRITTEN.with(|w| w.set(w.get() + 1));
        return Ok(id);
    };
    let id = store.write_unseen(&bytes)?;
    unseen.spills += 1;
    unseen.records += records;
    unseen.bytes += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok(id)
}

/// How a [`Sorter`] reads the key of a document.
pub(crate) type KeyOf<'s, K> = Box<dyn Fn(&Document) -> Result<K, MqlError> + 's>;

/// How a [`Sorter`] orders two keys.
pub(crate) type OrderOf<'s, K> = Box<dyn Fn(&K, &K) -> Result<Ordering, MqlError> + 's>;

/// An external sort that takes its input a document at a time, as the stage before it gives
/// it (010 FR-028): the documents wait in memory until the next one would pass `limit` bytes;
/// then those waiting go to the store as one sorted run. [`Sorter::finish`] merges the runs,
/// equal keys in input order. A provisional sort ([`Sorter::provisional`]) writes runs the
/// store does not count until [`Sorter::confirm`]; dropped before, it leaves no trace in what
/// the stage reports (010 T066).
pub(crate) struct Sorter<'s, K> {
    keys: KeyOf<'s, K>,
    cmp: OrderOf<'s, K>,
    limit: usize,
    runs: Runs<'s>,
    run: Vec<Document>,
    held: usize,
    /// What the runs written so far add up to, while the sort is provisional.
    unseen: Option<SpillStats>,
    /// The bytes of the largest document taken.
    largest: usize,
}

impl<'s, K> Sorter<'s, K> {
    /// A sort by `keys` in the order of `cmp`, in runs of at most `limit` bytes in `store`.
    pub(crate) fn new(
        keys: KeyOf<'s, K>,
        cmp: OrderOf<'s, K>,
        (limit, store): (usize, &'s dyn SpillStore),
    ) -> Sorter<'s, K> {
        Sorter {
            keys,
            cmp,
            limit,
            runs: Runs { store, ids: Vec::new() },
            run: Vec::new(),
            held: 0,
            unseen: None,
            largest: 0,
        }
    }

    /// The sort, writing runs the store counts only after [`Sorter::confirm`].
    pub(crate) fn provisional(mut self) -> Sorter<'s, K> {
        self.unseen = Some(SpillStats::default());
        self
    }

    /// Counts the runs written so far and every later one as written.
    pub(crate) fn confirm(&mut self) {
        if let Some(stats) = self.unseen.take() {
            WRITTEN.with(|w| w.set(w.get() + stats.spills));
            self.runs.store.count(stats);
        }
    }

    /// Takes document `d`, after those before it; the documents waiting go to the store first
    /// when `d` would take them past the limit.
    pub(crate) fn push(&mut self, d: Document) -> Result<(), MqlError> {
        self.push_within(d, self.limit)
    }

    /// [`Sorter::push`] with the documents waiting in memory kept within `budget` bytes, at
    /// most the limit, when the stage holds other things under the same limit.
    pub(crate) fn push_within(&mut self, d: Document, budget: usize) -> Result<(), MqlError> {
        let size = document_size(&d);
        if self.held + size > budget.min(self.limit) && !self.run.is_empty() {
            self.write_waiting()?;
        }
        self.held += size;
        self.largest = self.largest.max(size);
        self.run.push(d);
        Ok(())
    }

    /// Writes the documents waiting as a run.
    fn write_waiting(&mut self) -> Result<(), MqlError> {
        let run = std::mem::take(&mut self.run);
        let id = write_run(run, &self.keys, &self.cmp, (self.runs.store, self.unseen.as_mut()))?;
        self.runs.ids.push(vec![id]);
        self.held = 0;
        Ok(())
    }

    /// Gives `emit` every document it took, in order, through the store.
    pub(crate) fn finish(
        mut self,
        mut emit: impl FnMut(K, Document) -> Result<(), MqlError>,
    ) -> Result<(), MqlError> {
        if !self.run.is_empty() {
            self.write_waiting()?;
        }
        self.confirm();
        // What a run open in a merge holds: its next document, a document read in part and
        // what it reads at a time.
        let slot = 2 * self.largest + MIN_READ_BYTES;
        let fan_in = (self.limit / 2 / slot).max(2);
        while self.runs.ids.len() > fan_in {
            self.merge_pass(fan_in)?;
        }
        let runs = std::mem::take(&mut self.runs.ids);
        let merged = self.merge(&runs, &mut emit);
        self.runs.ids = runs;
        merged
    }

    /// Merges the runs in groups of `fan_in` consecutive ones, each into one run, so that a
    /// merge never holds more runs open than the limit allows.
    fn merge_pass(&mut self, fan_in: usize) -> Result<(), MqlError> {
        let store = self.runs.store;
        let mut pending: std::collections::VecDeque<Vec<u64>> =
            std::mem::take(&mut self.runs.ids).into();
        let mut merged = Vec::new();
        while !pending.is_empty() {
            let group: Vec<Vec<u64>> = pending.drain(..fan_in.min(pending.len())).collect();
            let mut pieces = Vec::new();
            if let Err(e) = self.merge_into_pieces(&group, &mut pieces) {
                // The sort keeps every piece written, to remove them when it goes.
                merged.push(pieces);
                merged.extend(group);
                merged.extend(pending);
                self.runs.ids = merged;
                return Err(e);
            }
            for id in group.iter().flatten() {
                store.remove(*id);
            }
            merged.push(pieces);
        }
        self.runs.ids = merged;
        Ok(())
    }

    /// Merges `runs` into `pieces` of at most a quarter of the limit, written in order. What a
    /// store reports leaves them out: they hold documents the stage already wrote.
    fn merge_into_pieces(&self, runs: &[Vec<u64>], pieces: &mut Vec<u64>) -> Result<(), MqlError> {
        let store = self.runs.store;
        let piece = (self.limit / 4).max(1);
        let mut bytes = Vec::new();
        let mut flush = |bytes: &mut Vec<u8>| -> Result<(), MqlError> {
            pieces.push(store.write_unseen(bytes)?);
            bytes.clear();
            Ok(())
        };
        self.merge(runs, &mut |_, d: Document| {
            let b =
                d.to_bytes().map_err(|e| errors::error(codes::INTERNAL_ERROR, e.to_string()))?;
            if !bytes.is_empty() && bytes.len() + b.len() > piece {
                flush(&mut bytes)?;
            }
            bytes.extend_from_slice(&b);
            Ok(())
        })?;
        if !bytes.is_empty() {
            flush(&mut bytes)?;
        }
        Ok(())
    }

    /// Gives `emit` the documents of `runs` in order, the earlier run first on a tie.
    fn merge(
        &self,
        runs: &[Vec<u64>],
        emit: &mut dyn FnMut(K, Document) -> Result<(), MqlError>,
    ) -> Result<(), MqlError> {
        let store = self.runs.store;
        // The reads of all the runs together stay within the limit, down to a floor.
        let step = (self.limit / 2 / runs.len().max(1)).clamp(MIN_READ_BYTES, READ_BYTES);
        let mut readers: Vec<Reader<'_>> =
            runs.iter().map(|run| Reader::of(store, run, step)).collect();
        let keys = &self.keys;
        let next = |r: &mut Reader<'_>| -> Result<Option<(K, Document)>, MqlError> {
            r.next_doc()?.map(|d| Ok((keys(&d)?, d))).transpose()
        };
        // The next document of each run that has one, with its run, in the order of the runs.
        let mut heads: Vec<(usize, K, Document)> = Vec::with_capacity(readers.len());
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some((k, d)) = next(r)? {
                heads.push((i, k, d));
            }
        }
        while let Some(pos) = smallest(&heads, &self.cmp)? {
            let (run, k, d) = heads.remove(pos);
            if let Some((nk, nd)) = next(&mut readers[run])? {
                heads.insert(pos, (run, nk, nd));
            }
            emit(k, d)?;
        }
        Ok(())
    }
}

/// Gives `emit` the documents of `docs` in the order `cmp` sets over their keys `keys`, equal
/// keys in input order, through runs of at most `limit` bytes in `store`.
pub(crate) fn external_sort<K>(
    docs: Vec<Document>,
    keys: impl Fn(&Document) -> Result<K, MqlError>,
    cmp: impl Fn(&K, &K) -> Result<Ordering, MqlError>,
    (limit, store): (usize, &dyn SpillStore),
    emit: impl FnMut(K, Document) -> Result<(), MqlError>,
) -> Result<(), MqlError> {
    let mut sorter = Sorter::new(Box::new(keys), Box::new(cmp), (limit, store));
    for d in docs {
        sorter.push(d)?;
    }
    sorter.finish(emit)
}

/// `docs` sorted by `spec` through runs of at most `limit` bytes in `store`, strings in
/// collation `col`.
pub fn sort_runs(
    spec: &SortSpec,
    docs: Vec<Document>,
    col: Option<&dyn Collator>,
    limit: usize,
    store: &dyn SpillStore,
) -> Result<Vec<Document>, MqlError> {
    let mut out = Vec::new();
    external_sort(
        docs,
        |d| spec.key(d, col),
        |a: &Vec<Value>, b| spec.compare(a, b, col),
        (limit, store),
        |_, d| {
            out.push(d);
            Ok(())
        },
    )?;
    Ok(out)
}

/// The place in `heads` of the document that comes first; the earlier run on a tie.
fn smallest<K>(
    heads: &[(usize, K, Document)],
    cmp: &dyn Fn(&K, &K) -> Result<Ordering, MqlError>,
) -> Result<Option<usize>, MqlError> {
    let mut best: Option<(usize, &K)> = None;
    for (i, (_, k, _)) in heads.iter().enumerate() {
        best = match best {
            Some((_, b)) if cmp(k, b)? != Ordering::Less => best,
            _ => Some((i, k)),
        };
    }
    Ok(best.map(|(i, _)| i))
}

/// The field that carries a document's sort key through a run (a name that starts with a
/// control character, as [`super::meta::MARK`]).
const SORT_KEY: &str = "\u{1}key";

/// `d` carrying sort key `key` through a run.
pub(crate) fn with_key(mut d: Document, key: Value) -> Document {
    d.push(SORT_KEY, key);
    d
}

/// The key a document carried through a run.
pub(crate) fn key_of(d: &Document) -> Value {
    d.get(SORT_KEY).cloned().unwrap_or(Value::Null)
}

/// The document as it was before [`with_key`].
pub(crate) fn without_key(mut d: Document) -> Document {
    d.remove(SORT_KEY);
    d
}

/// What a store was given: the runs, their documents and their bytes (the `spills`,
/// `spilledRecords` and `spilledBytes` of an explain).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpillStats {
    /// The runs written.
    pub spills: u64,
    /// The documents in them.
    pub records: u64,
    /// Their bytes.
    pub bytes: u64,
}

impl SpillStats {
    /// What was given after `earlier`.
    pub fn minus(self, earlier: SpillStats) -> SpillStats {
        SpillStats {
            spills: self.spills.saturating_sub(earlier.spills),
            records: self.records.saturating_sub(earlier.records),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// A store that counts what it passes on to another.
#[derive(Debug)]
pub struct Counting<S> {
    inner: S,
    spills: std::sync::atomic::AtomicU64,
    records: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
}

impl<S> Counting<S> {
    /// Counts what goes to `inner`.
    pub fn new(inner: S) -> Counting<S> {
        Counting {
            inner,
            spills: Default::default(),
            records: Default::default(),
            bytes: Default::default(),
        }
    }

    /// What it was given so far.
    pub fn stats(&self) -> SpillStats {
        use std::sync::atomic::Ordering::Relaxed;
        SpillStats {
            spills: self.spills.load(Relaxed),
            records: self.records.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
        }
    }
}

impl<S: std::ops::Deref<Target = T> + Debug, T: SpillStore + ?Sized> SpillStore for Counting<S> {
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        use std::sync::atomic::Ordering::Relaxed;
        let id = self.inner.write(bytes)?;
        let mut at = 0usize;
        let mut records = 0u64;
        while let Some(b) = bytes.get(at..at + 4) {
            records += 1;
            let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            at += usize::try_from(len).unwrap_or(usize::MAX).max(4);
        }
        self.spills.fetch_add(1, Relaxed);
        self.records.fetch_add(records, Relaxed);
        self.bytes.fetch_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX), Relaxed);
        Ok(id)
    }

    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
        self.inner.read(run, offset, len)
    }

    fn remove(&self, run: u64) {
        self.inner.remove(run);
    }

    fn write_unseen(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        self.inner.write_unseen(bytes)
    }

    fn count(&self, stats: SpillStats) {
        use std::sync::atomic::Ordering::Relaxed;
        self.spills.fetch_add(stats.spills, Relaxed);
        self.records.fetch_add(stats.records, Relaxed);
        self.bytes.fetch_add(stats.bytes, Relaxed);
        self.inner.count(stats);
    }
}

/// A store in memory, for nodes without a data directory and for tests.
#[derive(Debug, Default)]
pub struct MemorySpill {
    runs: std::sync::Mutex<(std::collections::BTreeMap<u64, Vec<u8>>, u64)>,
}

impl MemorySpill {
    fn runs(&self) -> std::sync::MutexGuard<'_, (std::collections::BTreeMap<u64, Vec<u8>>, u64)> {
        self.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The runs it holds.
    pub fn len(&self) -> usize {
        self.runs().0.len()
    }

    /// Whether it holds no run.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many runs it was given.
    pub fn written(&self) -> u64 {
        self.runs().1
    }
}

impl SpillStore for MemorySpill {
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        let mut g = self.runs();
        let id = g.1;
        g.1 += 1;
        g.0.insert(id, bytes.to_vec());
        Ok(id)
    }

    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
        let g = self.runs();
        let Some(b) = g.0.get(&run) else {
            return Err(errors::error(codes::INTERNAL_ERROR, format!("no spilled run {run}")));
        };
        let from = usize::try_from(offset).unwrap_or(usize::MAX).min(b.len());
        Ok(b[from..b.len().min(from.saturating_add(len))].to_vec())
    }

    fn remove(&self, run: u64) {
        self.runs().0.remove(&run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> SortSpec {
        let d = teseladb_bson::extjson::parse(text).expect("a sort");
        let holder = Document::new().with("s", d).to_bytes().expect("bytes");
        let raw = RawDoc::from_bytes(&holder).expect("a document");
        SortSpec::parse(raw.get("s").expect("the sort")).expect("a sort")
    }

    #[test]
    fn runs_merge_in_order_keeping_ties_in_input_order() {
        let docs: Vec<Document> = (0..500)
            .map(|i: i32| Document::new().with("k", (i * 7919) % 50).with("i", i))
            .collect();
        let s = spec(r#"{"k":1}"#);
        let expected = s.sort(docs.clone(), None).expect("sorted");
        let store = MemorySpill::default();
        let out = sort_runs(&s, docs, None, 500, &store).expect("sorted");
        assert_eq!(out, expected);
        assert!(store.written() > 10);
        assert!(store.is_empty());
    }

    #[test]
    fn nothing_writes_no_run_and_a_document_past_the_limit_is_a_run_of_its_own() {
        let store = MemorySpill::default();
        let s = spec(r#"{"k":1}"#);
        assert_eq!(sort_runs(&s, Vec::new(), None, 10, &store).expect("sorted"), []);
        assert_eq!(store.written(), 0);
        let docs: Vec<Document> =
            (0..3).map(|i: i32| Document::new().with("k", -i).with("s", "x".repeat(50))).collect();
        let counted = Counting::new(&store);
        let out = sort_runs(&s, docs.clone(), None, 1, &counted).expect("sorted");
        assert_eq!(out, docs.into_iter().rev().collect::<Vec<_>>());
        // Three runs, merged two at a time in a pass the figures leave out.
        assert_eq!((counted.stats().spills, counted.stats().records), (3, 3));
        assert!(store.written() > 3, "{}", store.written());
        let counted = Counting::new(&store);
        let more = vec![Document::new().with("k", 1), Document::new().with("k", 0)];
        sort_runs(&s, more, None, 1, &counted).expect("sorted");
        let stats = counted.stats();
        assert_eq!((stats.spills, stats.records), (2, 2));
        assert_eq!(stats.minus(SpillStats { spills: 1, records: 5, bytes: 0 }).records, 0);
        assert!(runs_written() >= 5);
        assert!(store.is_empty());
    }

    #[test]
    fn many_runs_merge_in_passes_and_keep_ties_in_input_order() {
        let docs: Vec<Document> = (0..2_000)
            .map(|i: i32| Document::new().with("k", (i * 7919) % 13).with("i", i))
            .collect();
        let s = spec(r#"{"k":-1}"#);
        let expected = s.sort(docs.clone(), None).expect("sorted");
        let store = MemorySpill::default();
        // Runs of a few documents and room in a merge for two of them at a time.
        let out = sort_runs(&s, docs, None, 200, &store).expect("sorted");
        assert_eq!(out, expected);
        let counted = Counting::new(&store);
        sort_runs(&s, expected.clone(), None, 200, &counted).expect("sorted");
        assert!(store.written() > 2 * counted.stats().spills, "the passes write runs too");
        assert!(store.is_empty());
    }

    #[test]
    fn a_provisional_sort_counts_its_runs_only_once_confirmed() {
        let s = spec(r#"{"k":1}"#);
        let store = MemorySpill::default();
        let counted = Counting::new(&store);
        let sorter = |store| {
            let (by, cmp) = (s.clone(), s.clone());
            Sorter::new(
                Box::new(move |d: &Document| by.key(d, None)),
                Box::new(move |a: &Vec<Value>, b: &Vec<Value>| cmp.compare(a, b, None)),
                (40, store),
            )
            .provisional()
        };
        let before = runs_written();
        let mut dropped = sorter(&counted);
        for i in 0..5 {
            dropped.push_within(Document::new().with("k", i), 20).expect("taken");
        }
        drop(dropped);
        assert_eq!((counted.stats(), runs_written()), (SpillStats::default(), before));
        assert!(store.is_empty() && store.written() >= 4, "{}", store.written());
        let mut kept = sorter(&counted);
        for i in (0..5).rev() {
            kept.push_within(Document::new().with("k", i), 20).expect("taken");
        }
        kept.confirm();
        let mut out = Vec::new();
        kept.finish(|_, d| {
            out.push(d);
            Ok(())
        })
        .expect("merged");
        let ks: Vec<Value> = out.iter().filter_map(|d| d.get("k").cloned()).collect();
        assert_eq!(ks, (0..5).map(Value::Int32).collect::<Vec<_>>());
        let stats = counted.stats();
        assert_eq!(stats.records, 5, "{stats:?}");
        assert!(stats.spills >= 4 && runs_written() >= before + stats.spills, "{stats:?}");
        assert!(store.is_empty());
    }

    #[test]
    fn a_failing_store_fails_the_sort_and_leaves_nothing() {
        #[derive(Debug, Default)]
        struct Lossy(MemorySpill);
        impl SpillStore for Lossy {
            fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
                self.0.write(&bytes[..bytes.len() - 1])
            }
            fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
                self.0.read(run, offset, len)
            }
            fn remove(&self, run: u64) {
                self.0.remove(run);
            }
        }
        let docs: Vec<Document> = (0..20).map(|i: i32| Document::new().with("k", i)).collect();
        let store = Lossy::default();
        let e = sort_runs(&spec(r#"{"k":-1}"#), docs, None, 100, &store).expect_err("cut");
        assert_eq!(e.message, "a spilled run is cut");
        assert!(store.0.is_empty());
        assert!(store.0.read(99, 0, 1).is_err());
    }
}
