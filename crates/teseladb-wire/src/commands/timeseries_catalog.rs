//! The bucket catalog of a time series as `collStats.timeseries` counts it (015 FR-010, T024,
//! T025), measured on a `refnode` 9.0.4 on 02-11-2026: for each meta value the catalog holds the
//! windows it writes to (open), those a lookup or a fetch left behind (displaced, still open
//! until the next insert of the meta value archives them) and those it archived; it reopens an
//! archived window from memory (a fetch) or looks a bucket up in the collection (a query), and
//! closes the windows before a measurement past them.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};

use teseladb_mql::timeseries_write::{BucketSpec, Full};
use teseladb_bson::{RawDoc, Value};

/// The start of a window, the first four bytes of a bucket's `_id`.
type Window = [u8; 4];
/// The hash of a meta value, the next five bytes of a bucket's `_id`.
type Meta = [u8; 5];
/// A window and meta value, the first nine bytes of a bucket's `_id`.
pub type Key = [u8; 9];

/// The bucket counters of a time series on this node since it started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TsStats {
    /// `numBucketInserts`.
    pub bucket_inserts: i64,
    /// `numBucketUpdates`.
    pub bucket_updates: i64,
    /// `numBucketsOpenedDueToMetadata`.
    pub opened_for_meta: i64,
    /// `numBucketsClosedDueToCount`.
    pub closed_by_count: i64,
    /// `numBucketsClosedDueToSize`.
    pub closed_by_size: i64,
    /// `numBucketsClosedDueToSchemaChange`.
    pub closed_by_schema: i64,
    /// `numBucketsClosedDueToTimeForward`.
    pub closed_time_forward: i64,
    /// `numBucketsKeptOpenDueToLargeMeasurements`.
    pub kept_open_large: i64,
    /// `numBucketsArchivedDueToTimeBackward`.
    pub archived_time_backward: i64,
    /// `numBucketsReopened`.
    pub reopened: i64,
    /// `numBucketsFetched`: reopened from the archive.
    pub fetched: i64,
    /// `numBucketsQueried`: reopened from a lookup in the collection.
    pub queried: i64,
    /// `numBucketQueriesFailed`: lookups that found no bucket.
    pub queries_failed: i64,
    /// `numCommits`.
    pub commits: i64,
    /// `numMeasurementsCommitted`.
    pub measurements: i64,
    /// The windows the catalog holds for each meta value.
    series: BTreeMap<Meta, Series>,
    /// The open bucket of each window and meta value: where the next insert there starts.
    open: BTreeMap<Key, u32>,
}

/// The windows of one meta value the catalog holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Series {
    open: BTreeSet<Window>,
    displaced: BTreeSet<Window>,
    archived: BTreeSet<Window>,
}

/// How the catalog finds the bucket of an insert ([`TsStats::route`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A bucket it holds open, or one it fetched from its archive.
    Open,
    /// A new bucket after it closed those of earlier windows.
    New,
    /// It looks one up in the collection; `fresh` for a meta value it never held.
    Query {
        /// The meta value is new to the catalog.
        fresh: bool,
    },
}

/// What one write of measurements did to the catalog so far. RefDb writes the measurements
/// of a batch in time order; a window the write already used closes by time forward when a
/// later one of its meta value is reopened. A failed ordered insert is first tried whole (an
/// `attempt`) and aborted: its lookups and the buckets it closed count, the windows it opened
/// do not, and it leaves none of the windows it went to in the catalog.
#[derive(Debug, Default)]
pub struct Pass {
    touched: BTreeSet<Key>,
    attempt: bool,
    created: BTreeSet<Key>,
    routed: BTreeSet<Key>,
    new_series: BTreeSet<Meta>,
}

impl Pass {
    /// The pass of the first, aborted try of a failed ordered insert.
    pub fn attempt() -> Self {
        Pass { attempt: true, ..Pass::default() }
    }
}

/// The window, meta value and both of bucket `id`.
fn split(id: [u8; 12]) -> (Window, Meta, Key) {
    (
        std::array::from_fn(|i| id[i]),
        std::array::from_fn(|i| id[4 + i]),
        std::array::from_fn(|i| id[i]),
    )
}

/// The key of window `w` of meta value `meta`.
fn key_of(w: Window, meta: Meta) -> Key {
    std::array::from_fn(|i| if i < 4 { w[i] } else { meta[i - 4] })
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The counters of `collStats.timeseries` that this node never moves: memory thresholds,
/// cache pressure, frozen buckets, compression and failed reopenings are RefDb's bucket
/// catalog's own machinery (measured at zero on a `refnode` 9.0.4 on 02-11-2026).
const ALWAYS_ZERO: [&str; 22] = [
    "numBucketDocumentsTooLargeInsert",
    "numBucketDocumentsTooLargeUpdate",
    "numBucketFetchesFailed",
    "numBucketReopeningsFailedDueToCompressionFailure",
    "numBucketReopeningsFailedDueToEraMismatch",
    "numBucketReopeningsFailedDueToHashCollision",
    "numBucketReopeningsFailedDueToMalformedIdField",
    "numBucketReopeningsFailedDueToMarkedClosed",
    "numBucketReopeningsFailedDueToMarkedFrozen",
    "numBucketReopeningsFailedDueToMinMaxCalculation",
    "numBucketReopeningsFailedDueToSchemaGeneration",
    "numBucketReopeningsFailedDueToUncompressedTimeColumn",
    "numBucketReopeningsFailedDueToValidator",
    "numBucketReopeningsFailedDueToWriteConflict",
    "numBucketsArchivedDueToMemoryThreshold",
    "numBucketsClosedDueToCachePressure",
    "numBucketsClosedDueToMemoryThreshold",
    "numBucketsClosedDueToReopening",
    "numBucketsFrozen",
    "numCompressedBucketsConvertedToUnsorted",
    "numDuplicateBucketsReopened",
    "numWaits",
];

impl TsStats {
    /// The counters as `collStats.timeseries` writes them after `bucketsNs`, with `bucketCount`
    /// and `avgBucketSize`, in alphabetical order as a `refrouter` merges them.
    pub fn fields(&self, bucket_count: i64, avg_bucket_size: i64) -> Vec<(&'static str, i64)> {
        let avg = if self.commits > 0 { self.measurements / self.commits } else { 0 };
        let active: usize =
            self.series.values().map(|s| s.open.len() + s.displaced.len() + s.archived.len()).sum();
        let mut out = vec![
            ("avgBucketSize", avg_bucket_size),
            ("avgNumMeasurementsPerCommit", avg),
            ("bucketCount", bucket_count),
            ("numActiveBuckets", count(active)),
            ("numBucketInserts", self.bucket_inserts),
            ("numBucketQueriesFailed", self.queries_failed),
            ("numBucketUpdates", self.bucket_updates),
            ("numBucketsArchivedDueToTimeBackward", self.archived_time_backward),
            ("numBucketsClosedDueToCount", self.closed_by_count),
            ("numBucketsClosedDueToSchemaChange", self.closed_by_schema),
            ("numBucketsClosedDueToSize", self.closed_by_size),
            ("numBucketsClosedDueToTimeForward", self.closed_time_forward),
            ("numBucketsFetched", self.fetched),
            ("numBucketsKeptOpenDueToLargeMeasurements", self.kept_open_large),
            ("numBucketsOpenedDueToMetadata", self.opened_for_meta),
            ("numBucketsQueried", self.queried),
            ("numBucketsReopened", self.reopened),
            ("numCommits", self.commits),
            ("numMeasurementsCommitted", self.measurements),
        ];
        out.extend(ALWAYS_ZERO.iter().map(|k| (*k, 0)));
        out.sort_by_key(|(k, _)| *k);
        out
    }

    /// How the catalog finds the bucket for measurements of the window and meta value of bucket
    /// `id`. The displaced windows of the meta value go to the archive first, and one of them
    /// that the insert goes back to is taken from there at once. A window held open takes the
    /// measurements; an archived one is fetched; past every open window those close and a new
    /// bucket opens; before one, or with none, the catalog looks a bucket up.
    pub fn route(&mut self, id: [u8; 12], pass: &mut Pass) -> Route {
        let (window, meta, key) = split(id);
        let fresh = !self.series.contains_key(&meta);
        if pass.attempt {
            pass.routed.insert(key);
            if fresh {
                pass.new_series.insert(meta);
            }
        }
        let s = self.series.entry(meta).or_default();
        let displaced = std::mem::take(&mut s.displaced);
        self.archived_time_backward += count(displaced.len());
        let retaken = displaced.contains(&window);
        s.archived.extend(displaced.into_iter().filter(|w| *w != window));
        if retaken || s.open.contains(&window) {
            s.open.insert(window);
            return Route::Open;
        }
        if s.archived.remove(&window) {
            self.fetched += 1;
            self.reopened += 1;
            self.leave_open(meta, pass);
            return Route::Open;
        }
        let s = self.series.entry(meta).or_default();
        if s.open.is_empty() || s.open.iter().any(|w| *w > window) {
            return Route::Query { fresh };
        }
        let closed = std::mem::take(&mut s.open);
        self.closed_time_forward +=
            count(closed.iter().filter(|w| !pass.created.contains(&key_of(**w, meta))).count());
        Route::New
    }

    /// The open windows of `meta` as the catalog reopens another one: a window this write used
    /// closes by time forward, the others are displaced.
    fn leave_open(&mut self, meta: Meta, pass: &Pass) {
        let s = self.series.entry(meta).or_default();
        let open = std::mem::take(&mut s.open);
        for w in open {
            let k = key_of(w, meta);
            if !pass.touched.contains(&k) {
                s.displaced.insert(w);
                continue;
            }
            // A window the aborted try opened itself closes uncounted.
            self.closed_time_forward += i64::from(!pass.created.contains(&k));
        }
    }

    /// Counts the lookup of a [`Route::Query`] for bucket `id`: one that `found` a bucket
    /// reopens it; one that found none failed, and the later open windows go to the archive.
    pub fn looked_up(&mut self, id: [u8; 12], found: bool) {
        if found {
            self.queried += 1;
            self.reopened += 1;
            return;
        }
        self.queries_failed += 1;
        self.archive_later(id);
    }

    /// What the bucket a lookup found for bucket `id` does to the other open windows: a `full`
    /// one gives no room, so the later ones go to the archive; one with room takes the insert
    /// and the others are left behind.
    pub fn landed(&mut self, id: [u8; 12], full: bool, pass: &Pass) {
        let (_, meta, _) = split(id);
        match full {
            true => self.archive_later(id),
            false => self.leave_open(meta, pass),
        }
    }

    /// Archives the open windows later than that of bucket `id`.
    fn archive_later(&mut self, id: [u8; 12]) {
        let (window, meta, _) = split(id);
        let s = self.series.entry(meta).or_default();
        let later: Vec<Window> = s.open.range((Excluded(window), Unbounded)).copied().collect();
        for w in later {
            s.open.remove(&w);
            s.archived.insert(w);
            self.archived_time_backward += 1;
        }
    }

    /// Counts a commit of `taken` measurements into bucket `id` (number `n` of its window and
    /// meta value), which `existed` or not; `closing` is why the bucket before it stopped.
    pub fn commit(
        &mut self,
        (id, n): ([u8; 12], u32),
        existed: bool,
        closing: Option<Full>,
        taken: i64,
        pass: &mut Pass,
    ) {
        let (window, meta, key) = split(id);
        self.open.insert(key, n);
        self.series.entry(meta).or_default().open.insert(window);
        pass.touched.insert(key);
        self.commits += 1;
        self.measurements += taken;
        if existed {
            self.bucket_updates += 1;
            return;
        }
        self.bucket_inserts += 1;
        match closing {
            Some(Full::Count) => self.closed_by_count += 1,
            Some(Full::Size) => self.closed_by_size += 1,
            // Counted when the bucket closed ([`TsStats::closed_for_schema`]).
            Some(Full::Schema) | None => {}
        }
    }

    /// Counts what the first commit of an insert that found its bucket by `route` says: a new
    /// bucket after a lookup that found none opens a meta value it never held; an open bucket
    /// that is gone (its measurements deleted) is looked up again, and fails.
    pub fn first_commit(&mut self, route: Route, existed: bool, full: bool) {
        match route {
            Route::Query { fresh } if !existed && !full => self.opened_for_meta += i64::from(fresh),
            Route::Open if !existed && !full => self.queries_failed += 1,
            Route::Open | Route::New | Route::Query { .. } => {}
        }
    }

    /// Counts bucket `n` of window and meta value `key` closed because a measurement has a
    /// field of another type: the next insert starts after it.
    pub fn closed_for_schema(&mut self, key: Key, n: u32) {
        self.closed_by_schema += 1;
        self.open.insert(key, n + 1);
    }

    /// Counts an open bucket an aborted attempt found `full`.
    pub fn closed_in_attempt(&mut self, full: Full) {
        match full {
            Full::Count => self.closed_by_count += 1,
            Full::Size => self.closed_by_size += 1,
            Full::Schema => self.closed_by_schema += 1,
        }
    }

    /// Bucket `id` as an aborted attempt opens it: later groups of the attempt see it open.
    pub fn provisional(&mut self, id: [u8; 12], pass: &mut Pass) {
        let (window, meta, key) = split(id);
        self.series.entry(meta).or_default().open.insert(window);
        pass.touched.insert(key);
        pass.created.insert(key);
    }

    /// The end of an aborted attempt: none of the windows it went to stays in the catalog.
    pub fn abort(&mut self, pass: &Pass) {
        for key in &pass.routed {
            let window: Window = std::array::from_fn(|i| key[i]);
            let meta: Meta = std::array::from_fn(|i| key[4 + i]);
            // Every window the try went to has its meta value's entry ([`TsStats::route`]).
            let s = self.series.entry(meta).or_default();
            s.open.remove(&window);
            s.displaced.remove(&window);
            s.archived.remove(&window);
        }
        // A meta value new to the try held only windows of the try.
        for meta in &pass.new_series {
            self.series.remove(meta);
        }
    }

    /// Bucket `id` after a write into it failed: the catalog drops its window, and the meta
    /// value when it holds no other window.
    pub fn cleared(&mut self, id: [u8; 12]) {
        let (window, meta, _) = split(id);
        // The write routed there first, which gave the meta value its entry.
        let s = self.series.entry(meta).or_default();
        s.open.remove(&window);
        s.displaced.remove(&window);
        if s.open.is_empty() && s.displaced.is_empty() && s.archived.is_empty() {
            self.series.remove(&meta);
        }
    }

    /// The bucket an insert into window and meta value `key` starts at.
    pub fn open_bucket(&self, key: Key) -> u32 {
        self.open.get(&key).copied().unwrap_or(0)
    }
}

/// A bucket of one window and meta value as a lookup sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// Its number in the window and meta value (the last three bytes of its `_id`).
    pub n: u32,
    /// `control.min` and `control.max` of the time field.
    pub min: i64,
    /// See `min`.
    pub max: i64,
    /// The bucket.
    pub bytes: Vec<u8>,
}

impl Stored {
    /// The bucket stored as `bytes`, laid out by `spec`; `None` when it has no `_id` or no times.
    pub fn of(spec: &BucketSpec, bytes: Vec<u8>) -> Option<Stored> {
        let raw = RawDoc::from_bytes(&bytes).ok()?;
        let Some(Value::ObjectId(id)) = raw.get("_id").map(Value::from) else { return None };
        let time = |side: &str| match raw.get(&format!("control.{side}.{}", spec.time_field)) {
            Some(v) => match Value::from(v) {
                Value::DateTime(t) => Some(t),
                _ => None,
            },
            None => None,
        };
        let n = u32::from_be_bytes([0, id[9], id[10], id[11]]);
        Some(Stored { n, min: time("min")?, max: time("max")?, bytes })
    }
}

/// The bucket a lookup for measurements from time `t` lands on among `buckets`, those of one
/// window and meta value: RefDb's query takes the first bucket that starts at or before `t`
/// in the order of the index on the meta value and the time (start, then end), or in `_id`
/// order without a meta field (measured on a `refnode` 9.0.4 on 02-11-2026: after a full bucket
/// and one with room, the lookup lands on the full one; after one with an earlier end, on it).
pub fn lookup(buckets: &[Stored], t: i64, by_index: bool) -> Option<&Stored> {
    let candidates = buckets.iter().filter(|b| b.min <= t);
    match by_index {
        true => candidates.min_by_key(|b| (b.min, b.max, b.n)),
        false => candidates.min_by_key(|b| (b.min, b.n)),
    }
}

/// The number of the next new bucket of a window and meta value holding `buckets`.
pub fn next_free(buckets: &[Stored]) -> u32 {
    buckets.iter().map(|b| b.n + 1).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(window: u8, meta: u8, n: u8) -> [u8; 12] {
        let mut id = [0u8; 12];
        id[3] = window;
        id[8] = meta;
        id[11] = n;
        id
    }

    fn stored(n: u32, min: i64, max: i64) -> Stored {
        Stored { n, min, max, bytes: Vec::new() }
    }

    #[test]
    fn a_lookup_takes_the_first_bucket_in_index_order_that_starts_in_time() {
        let b = [stored(0, 0, 999), stored(1, 0, 1004), stored(2, 0, 1), stored(3, 60, 61)];
        assert_eq!(lookup(&b, 5, true).map(|b| b.n), Some(2));
        assert_eq!(lookup(&b, 5, false).map(|b| b.n), Some(0));
        assert_eq!(lookup(&b[3..], 5, true), None);
        assert_eq!(next_free(&b), 4);
        assert_eq!(next_free(&[]), 0);
    }

    #[test]
    fn an_attempt_leaves_no_window_and_counts_no_close_of_its_own() {
        let mut s = TsStats::default();
        let mut pass = Pass::attempt();
        assert_eq!(s.route(id(0, 1, 0), &mut pass), Route::Query { fresh: true });
        s.looked_up(id(0, 1, 0), false);
        s.provisional(id(0, 1, 0), &mut pass);
        assert_eq!(s.route(id(2, 1, 0), &mut pass), Route::New);
        s.provisional(id(2, 1, 0), &mut pass);
        s.abort(&pass);
        assert_eq!((s.closed_time_forward, s.queries_failed), (0, 1));
        assert!(s.series.is_empty());
        assert_eq!(s.route(id(0, 1, 0), &mut Pass::default()), Route::Query { fresh: true });
    }

    #[test]
    fn a_failed_write_drops_its_window_and_a_full_landing_archives_the_later_ones() {
        let mut s = TsStats::default();
        let mut pass = Pass::default();
        s.commit((id(3, 1, 0), 0), false, None, 1, &mut pass);
        s.commit((id(4, 1, 0), 0), false, None, 1, &mut pass);
        s.landed(id(1, 1, 0), true, &pass);
        assert_eq!(s.archived_time_backward, 2);
        s.cleared(id(1, 1, 0));
        s.closed_in_attempt(Full::Count);
        s.closed_in_attempt(Full::Size);
        s.closed_in_attempt(Full::Schema);
        assert_eq!((s.closed_by_count, s.closed_by_size, s.closed_by_schema), (1, 1, 1));
        s.first_commit(Route::Open, false, false);
        s.first_commit(Route::New, false, false);
        assert_eq!(s.queries_failed, 1);
    }
}
