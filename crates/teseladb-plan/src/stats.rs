//! Statistics of a range and the cost of a plan from them (010 FR-029): the number of documents,
//! and for each indexed field its number of values, of distinct values and an equal-depth
//! histogram. The planner estimates the keys and documents each plan reads, picks the cheapest,
//! and only runs a trial among the plans whose estimates are within twice the best one's.
//!
//! The statistics are built from the values of each field in their index order
//! ([`FieldStats::build`]), as the compaction of a range reads them, and merged across ranges
//! ([`RangeStats::merge`]).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};

use teseladb_bson::Value;

use crate::logical::{Bound, compare_values};
use crate::rules::{Access, IndexMeta, IndexScan};

/// The buckets of a histogram, at most.
pub const HISTOGRAM_BUCKETS: usize = 64;

/// One bucket of a histogram: the values after the previous bucket's upper value, up to its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Bucket {
    /// The greatest value of the bucket.
    pub upper: Value,
    /// Values in the bucket, its upper value included.
    pub count: u64,
    /// Values equal to the upper value.
    pub upper_count: u64,
    /// Distinct values in the bucket.
    pub distinct: u64,
}

/// What the statistics know of one field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldStats {
    /// Values of the field (a document with an array counts each element).
    pub count: u64,
    /// Distinct values.
    pub distinct: u64,
    /// Equal-depth buckets in the reference server's order of values.
    pub buckets: Vec<Bucket>,
}

/// The statistics of a range.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RangeStats {
    /// Documents of the range.
    pub documents: u64,
    /// The statistics of each indexed field, by path.
    pub fields: BTreeMap<String, FieldStats>,
}

impl FieldStats {
    /// The statistics of `values` (in any order), in at most [`HISTOGRAM_BUCKETS`] buckets.
    pub fn build(mut values: Vec<Value>) -> FieldStats {
        values.sort_by(compare_values);
        let count = values.len() as u64;
        // Runs of equal values: a bucket never splits one.
        let mut runs: Vec<(Value, u64)> = Vec::new();
        for v in values {
            match runs.last_mut() {
                Some((last, n)) if compare_values(last, &v) == Ordering::Equal => *n += 1,
                _ => runs.push((v, 1)),
            }
        }
        let distinct = runs.len() as u64;
        let depth = count.div_ceil(HISTOGRAM_BUCKETS as u64).max(1);
        let mut buckets: Vec<Bucket> = Vec::new();
        let mut open: Option<Bucket> = None;
        for (v, n) in runs {
            let b = open.get_or_insert(Bucket {
                upper: Value::Null,
                count: 0,
                upper_count: 0,
                distinct: 0,
            });
            b.upper = v;
            b.count += n;
            b.upper_count = n;
            b.distinct += 1;
            if b.count >= depth {
                buckets.extend(open.take());
            }
        }
        buckets.extend(open);
        FieldStats { count, distinct, buckets }
    }

    /// The values of the field estimated within `bounds` (intervals that do not overlap).
    pub fn estimate(&self, bounds: &[Bound]) -> f64 {
        bounds.iter().map(|b| self.within(b)).sum::<f64>().min(self.count as f64)
    }

    fn within(&self, b: &Bound) -> f64 {
        let mut total = 0.0;
        let mut lower: Option<&Value> = None;
        for bucket in &self.buckets {
            // The values strictly between the previous bucket's upper value and this one's.
            let inner = (bucket.count - bucket.upper_count) as f64;
            if b.is_point() {
                match compare_values(&b.low, &bucket.upper) {
                    Ordering::Equal => return bucket.upper_count as f64,
                    Ordering::Less => {
                        let above = lower.is_none_or(|l| compare_values(&b.low, l).is_gt());
                        let kinds = bucket.distinct.saturating_sub(1).max(1) as f64;
                        return if above { inner / kinds } else { 0.0 };
                    }
                    Ordering::Greater => {}
                }
            } else {
                let disjoint = lower.is_some_and(|l| compare_values(&b.high, l).is_le())
                    || compare_values(&b.low, &bucket.upper).is_ge();
                let start = match lower {
                    Some(l) => compare_values(&b.low, l).is_le(),
                    None => matches!(b.low, Value::MinKey),
                };
                let end = compare_values(&b.high, &bucket.upper).is_ge();
                total += match (disjoint, start && end) {
                    (true, _) => 0.0,
                    (false, true) => inner,
                    (false, false) => inner / 2.0,
                };
                if in_bound(b, &bucket.upper) {
                    total += bucket.upper_count as f64;
                }
            }
            lower = Some(&bucket.upper);
        }
        total
    }
}

/// Whether `v` is inside interval `b`.
fn in_bound(b: &Bound, v: &Value) -> bool {
    let lo = compare_values(v, &b.low);
    let hi = compare_values(v, &b.high);
    (lo.is_gt() || (lo.is_eq() && b.low_inclusive))
        && (hi.is_lt() || (hi.is_eq() && b.high_inclusive))
}

/// Relative costs of the cost model: reading a key of an index, reading a document by its key
/// (a fetch), and reading a document in a collection scan.
const KEY_COST: f64 = 0.25;
const FETCH_COST: f64 = 1.0;
const SCAN_COST: f64 = 0.5;
/// The cost of sorting `n` documents in memory, per comparison.
const SORT_COST: f64 = 0.05;

/// Two plans whose costs are within this factor of each other are tried against each other.
pub const TRIAL_FACTOR: f64 = 2.0;

impl RangeStats {
    /// The statistics of two ranges together: documents and values add up, histograms join.
    pub fn merge(&self, other: &RangeStats) -> RangeStats {
        let mut fields = self.fields.clone();
        for (path, f) in &other.fields {
            let joined = match fields.get(path) {
                Some(mine) => join(mine, f),
                None => f.clone(),
            };
            fields.insert(path.clone(), joined);
        }
        RangeStats { documents: self.documents + other.documents, fields }
    }

    /// The fraction of documents whose `path` lies within `bounds`, when the field has
    /// statistics.
    pub fn fraction(&self, path: &str, bounds: &[Bound]) -> Option<f64> {
        let f = self.fields.get(path)?;
        if f.count == 0 {
            return Some(0.0);
        }
        Some((f.estimate(bounds) / f.count as f64).clamp(0.0, 1.0))
    }

    /// The estimated cost of `access`, `None` when a field it scans has no statistics.
    pub fn cost(&self, access: &Access, indexes: &[IndexMeta], sorted: bool) -> Option<f64> {
        let docs = self.documents as f64;
        let sort = |rows: f64, gives_order: bool| {
            if sorted && !gives_order { rows * rows.max(2.0).log2() * SORT_COST } else { 0.0 }
        };
        Some(match access {
            Access::Collscan => docs * SCAN_COST + sort(docs, false),
            Access::Index(scan) => {
                let keys = self.keys(scan, indexes)?;
                keys * (KEY_COST + FETCH_COST) + sort(keys, scan.sorts)
            }
            Access::Or(scans) => {
                let mut keys = 0.0;
                for s in scans {
                    keys += self.keys(s, indexes)?;
                }
                let keys = keys.min(docs);
                keys * (KEY_COST + FETCH_COST) + sort(keys, scans.iter().all(|s| s.sorts))
            }
            Access::AndHash { scans, .. } => {
                // The documents found are those every scan holds: with the fields independent,
                // the share of each scan multiplied (009 FR-023). Each key read joins a set of
                // local identifiers, which costs a key; only the documents found are fetched.
                let mut keys = 0.0;
                let mut found = docs;
                for s in scans {
                    let k = self.keys(s, indexes)?;
                    keys += k;
                    found *= if docs > 0.0 { (k / docs).min(1.0) } else { 0.0 };
                }
                keys * KEY_COST + found * FETCH_COST + sort(found, false)
            }
        })
    }

    /// The index keys `scan` reads: the documents of the leading field's intervals, narrowed by
    /// each next field while the fields before it are points.
    fn keys(&self, scan: &IndexScan, indexes: &[IndexMeta]) -> Option<f64> {
        let multikey = indexes.get(scan.index).is_some_and(|i| i.multikey);
        let mut rows = self.documents as f64;
        for (path, bounds) in &scan.bounds {
            let f = self.fraction(path, bounds)?;
            rows *= f;
            if !bounds.iter().all(Bound::is_point) {
                break;
            }
        }
        // A multikey index holds a key per element: count the field's values, not documents.
        if multikey
            && let Some((path, _)) = scan.bounds.first()
            && let Some(f) = self.fields.get(path)
            && self.documents > 0
        {
            rows *= (f.count as f64 / self.documents as f64).max(1.0);
        }
        Some(rows)
    }
}

/// Two histograms of one field as one: the buckets of both in order (statistics are rebuilt
/// whole at the next compaction).
fn join(a: &FieldStats, b: &FieldStats) -> FieldStats {
    let mut buckets: Vec<Bucket> = a.buckets.iter().chain(&b.buckets).cloned().collect();
    buckets.sort_by(|x, y| compare_values(&x.upper, &y.upper));
    let mut merged: Vec<Bucket> = Vec::with_capacity(buckets.len());
    for bk in buckets {
        match merged.last_mut() {
            Some(last) if compare_values(&last.upper, &bk.upper).is_eq() => {
                last.count += bk.count;
                last.upper_count += bk.upper_count;
                last.distinct = last.distinct.max(bk.distinct);
            }
            _ => merged.push(bk),
        }
    }
    FieldStats { count: a.count + b.count, distinct: a.distinct.max(b.distinct), buckets: merged }
}

/// What the planner does with its plans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The plan at this position wins without a trial.
    Pick(usize),
    /// The plans at these positions (in their order) run a trial: their estimates are within
    /// [`TRIAL_FACTOR`] of the best one's, or some field had no statistics.
    Trial(Vec<usize>),
}

/// The choice among `plans` by their estimated costs.
pub fn choose(plans: &[Access], stats: &RangeStats, indexes: &[IndexMeta], sorted: bool) -> Choice {
    let costs: Option<Vec<f64>> = plans.iter().map(|p| stats.cost(p, indexes, sorted)).collect();
    let Some(costs) = costs else { return Choice::Trial((0..plans.len()).collect()) };
    let Some(best) = costs.iter().copied().min_by(f64::total_cmp) else {
        return Choice::Trial(Vec::new());
    };
    // A plan as cheap as the best, or within twice its cost, goes to the trial.
    let close: Vec<usize> =
        (0..plans.len()).filter(|&i| costs[i] <= best || costs[i] < best * TRIAL_FACTOR).collect();
    match close.as_slice() {
        [one] => Choice::Pick(*one),
        _ => Choice::Trial(close),
    }
}

/// The intersection of index scans among `intersections` (009 FR-023) that the statistics find
/// cheaper than every plan of `plans` by more than [`TRIAL_FACTOR`]: the planner takes it without
/// a trial. `None` when no intersection is that much cheaper or a field has no statistics.
pub fn cheaper_intersection(
    plans: &[Access],
    intersections: Vec<Access>,
    stats: &RangeStats,
    indexes: &[IndexMeta],
    sorted: bool,
) -> Option<Access> {
    let mut best = f64::INFINITY;
    for p in plans {
        best = best.min(stats.cost(p, indexes, sorted)?);
    }
    let mut chosen: Option<(f64, Access)> = None;
    for a in intersections {
        let cost = stats.cost(&a, indexes, sorted)?;
        if cost * TRIAL_FACTOR < best && chosen.as_ref().is_none_or(|(c, _)| cost < *c) {
            chosen = Some((cost, a));
        }
    }
    chosen.map(|(_, a)| a)
}

/// The statistics of a collection: each range's, as its last compaction left them, and their
/// merge with the epoch it was made in.
#[derive(Default)]
struct Published {
    ranges: BTreeMap<u64, RangeStats>,
    merged: Option<(u64, Arc<RangeStats>)>,
}

/// The statistics of the node's collections, by the `01 · tenant · collection` prefix of their
/// keys.
fn registry() -> &'static Mutex<HashMap<Vec<u8>, Published>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Vec<u8>, Published>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The epochs of statistics: each publication takes the next one, so a program ordered by
/// statistics is kept only while they hold (010 FR-031). 0 is the default estimate's.
static EPOCH: AtomicU64 = AtomicU64::new(1);

fn locked() -> std::sync::MutexGuard<'static, HashMap<Vec<u8>, Published>> {
    // A panic while holding the lock leaves whole entries behind: the map stays usable.
    registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The hook the storage calls when it compacts range `range` of `collection` (the
/// `01 · tenant · collection` prefix of its keys): the range holds `documents` documents and,
/// for each indexed field, the values the compaction read in index order. The range's
/// statistics are rebuilt from them and replace the ones it published before (010 FR-029).
pub fn recompute_at_compaction(
    collection: &[u8],
    range: u64,
    documents: u64,
    fields: Vec<(String, Vec<Value>)>,
) {
    let fields =
        fields.into_iter().map(|(path, values)| (path, FieldStats::build(values))).collect();
    publish_range(collection, range, RangeStats { documents, fields });
}

/// Range `range` of `collection` now has statistics `stats`.
pub fn publish_range(collection: &[u8], range: u64, stats: RangeStats) {
    let mut map = locked();
    let entry = map.entry(collection.to_vec()).or_default();
    entry.ranges.insert(range, stats);
    entry.merged = None;
}

/// Range `range` of `collection` is gone from this node (moved, merged or dropped).
pub fn forget_range(collection: &[u8], range: u64) {
    let mut map = locked();
    if let Some(entry) = map.get_mut(collection) {
        entry.ranges.remove(&range);
        entry.merged = None;
        if entry.ranges.is_empty() {
            map.remove(collection);
        }
    }
}

/// The statistics of `collection`'s ranges merged, with their epoch; `None` before any
/// compaction published some.
pub fn collection_stats(collection: &[u8]) -> Option<(u64, Arc<RangeStats>)> {
    let mut map = locked();
    let entry = map.get_mut(collection)?;
    if let Some(m) = &entry.merged {
        return Some(m.clone());
    }
    let merged = entry.ranges.values().fold(RangeStats::default(), |acc, r| acc.merge(r));
    let m = (EPOCH.fetch_add(1, AtomicOrdering::Relaxed), Arc::new(merged));
    entry.merged = Some(m.clone());
    Some(m)
}
