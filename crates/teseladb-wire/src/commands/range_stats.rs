//! The statistics of the ranges a compaction rewrote (010 FR-029): the storage of a core marks
//! each range whose keys a compaction of its LSM rewrote ([`teseladb_plan::stats::mark_compacted`]),
//! and the node's keeper reads, every [`STATS_EVERY_SECS`], the entries of the indexes of each
//! collection the range holds and publishes the range's statistics for the planner: its documents
//! and, for each field of an index the planner scans, the values of the field as the index keeps
//! them.
//!
//! The fields come from the ordered indexes the planner may scan, without collation (their keys
//! hold sort keys, not values) and without hash buckets; the index of `_id` is left out, since
//! its entries are the documents themselves and reading them all is the cost the statistics
//! exist to avoid. A field two indexes share is read from the first one.

use std::time::{Duration, Instant};

use teseladb_bson::Value;
use teseladb_keys::space::KeyBuilder;
use teseladb_meta::catalog::CollectionEntry;
use teseladb_plan::stats;
use teseladb_range::read::ReadConcern;
use teseladb_stages::{self as stages, Stage};

use super::Backend;
use super::plan_exec::{self, PlanIndex};
use super::query::documents_of;
use crate::error::CommandError;

/// How often the node's keeper recomputes the statistics of the ranges compactions rewrote: a
/// range gets them at most once in that time, however many compactions touch it.
pub const STATS_EVERY_SECS: u64 = 10;

/// The share of its shard's time the passes take at most: after a pass of `d`, the next one waits
/// `d · (1 − share) / share`, and never less than [`STATS_EVERY_SECS`]. A pass reads every entry
/// of the indexes of each range it publishes; in a range a load keeps compacting, passes that
/// followed each other 10 s apart kept a shard reading all the time.
pub const STATS_SHARE_PERCENT: u32 = 20;

/// The wait before the next pass after one that took `took`: `every`, or longer for a pass so
/// long that the passes would take more than [`STATS_SHARE_PERCENT`] of the time.
#[must_use]
pub fn next_pass_after(every: Duration, took: Duration) -> Duration {
    let rest = took.saturating_mul(100 - STATS_SHARE_PERCENT) / STATS_SHARE_PERCENT;
    every.max(rest)
}

/// Documents read per call when the backend keeps no count of a collection's documents.
const COUNT_CHUNK: usize = 1024;

/// A key interval; an empty end is no end.
type Span = (Vec<u8>, Vec<u8>);

/// Recomputes and publishes the statistics of each range compactions rewrote since the last pass,
/// and forgets those of the ranges no longer in the node's routing table. A range whose
/// statistics cannot be read now (no leader for a range) waits for the next pass. Gives the
/// ranges it published.
pub fn pass<B: Backend + ?Sized>(backend: &mut B) -> Result<usize, CommandError> {
    pass_pausing(backend, &mut || {})
}

/// [`pass`], calling `pause` between the steps of its reads and builds, where the caller may
/// give the CPU to other work (023 FR-012): a range of hundreds of thousands of index entries
/// is seconds of reads and comparisons. Each pass, read and build is timed in the stages `stats_pass`,
/// `stats_read` and `stats_build`.
pub fn pass_pausing<B: Backend + ?Sized>(
    backend: &mut B,
    pause: &mut dyn FnMut(),
) -> Result<usize, CommandError> {
    let started = Instant::now();
    let out = ranges(backend, pause);
    if matches!(out, Ok(n) if n > 0) {
        stages::record(Stage::StatsPass, nanos_since(started));
    }
    out
}

fn nanos_since(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn ranges<B: Backend + ?Sized>(
    backend: &mut B,
    pause: &mut dyn FnMut(),
) -> Result<usize, CommandError> {
    // The routing table first: a pass that cannot read it leaves the ranges marked.
    let live = backend.route_entries()?;
    let due = stats::take_compacted();
    if due.is_empty() {
        return Ok(0);
    }
    if !live.is_empty() {
        stats::retain_ranges(|r| live.iter().any(|e| e.group.0 == r));
    }
    let collections = backend.collections()?;
    let mut published = 0;
    for (range, start, end) in due {
        // A range that left the node since it was marked has nothing to publish.
        if !live.is_empty() && !live.iter().any(|e| e.group.0 == range) {
            continue;
        }
        let span = (start, end);
        let mut read = Ok(());
        for (_, coll) in collections.iter().filter(|(_, c)| holds(&span, c)) {
            read = refresh(backend, coll, (range, &span), pause);
            if read.is_err() {
                break;
            }
        }
        match read {
            Ok(()) => published += 1,
            Err(_) => stats::mark_compacted(range, &span.0, &span.1),
        }
        backend.end_operation();
    }
    Ok(published)
}

/// Whether range `span` holds keys of collection `c` (its documents or its indexes).
fn holds(span: &Span, c: &CollectionEntry) -> bool {
    let first = KeyBuilder::collection_start(c.tenant, c.id).as_bytes().to_vec();
    let past = KeyBuilder::collection_end(c.tenant, c.id).as_bytes().to_vec();
    (span.1.is_empty() || first < span.1) && span.0 < past
}

/// `[a0, a1)` cut to `[b0, b1)`; empty ends are no ends.
fn cut((a0, a1): (&[u8], &[u8]), (b0, b1): (&[u8], &[u8])) -> Span {
    let start = a0.max(b0).to_vec();
    let end = match (a1.is_empty(), b1.is_empty()) {
        (true, _) => b1.to_vec(),
        (_, true) => a1.to_vec(),
        _ => a1.min(b1).to_vec(),
    };
    (start, end)
}

/// Publishes the statistics of collection `coll` in range `range` over `span`.
fn refresh<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    (range, span): (u64, &Span),
    pause: &mut dyn FnMut(),
) -> Result<(), CommandError> {
    if !plan_exec::plannable_kind(coll)? {
        return Ok(());
    }
    let started = Instant::now();
    let (first, past) = documents_of(coll);
    let docs = cut((&first, &past), (&span.0, &span.1));
    let documents = match backend.doc_tally((coll.tenant, coll.id), (&docs.0, &docs.1))? {
        Some((n, _, _)) => n,
        None => counted(backend, &docs, pause)?,
    };
    let indexes = plan_exec::plan_indexes(backend, coll)?;
    let mut fields: Vec<(String, Vec<Value>)> = Vec::new();
    for index in indexes.iter().filter(|i| i.gives_statistics()) {
        let (first, past) = index.span((coll.tenant, coll.id));
        let within = cut((&first, &past), (&span.0, &span.1));
        // The positions of the fields no index before this one gave, each with its values. The
        // entries are read a pull at a time and only those values are kept (023 FR-012): reading
        // every entry of the range first held them all, with their keys, at once.
        let taken: Vec<(usize, &str)> = index
            .paths()
            .enumerate()
            .filter(|(_, p)| !fields.iter().any(|(f, _)| f == p))
            .collect();
        let mut columns: Vec<Vec<Value>> = vec![Vec::new(); taken.len()];
        let mut keep = |pulled: Vec<Vec<Value>>| {
            for mut values in pulled {
                for (column, (i, _)) in columns.iter_mut().zip(&taken) {
                    if let Some(v) = values.get_mut(*i) {
                        column.push(std::mem::replace(v, Value::Null));
                    }
                }
            }
        };
        if !plan_exec::index_values(backend, coll, index, &within, pause, &mut keep)? {
            continue;
        }
        pause();
        let names = taken.iter().map(|(_, p)| (*p).to_owned());
        fields.extend(names.zip(columns));
    }
    stages::record(Stage::StatsRead, nanos_since(started));
    let started = Instant::now();
    let prefix = KeyBuilder::collection_start(coll.tenant, coll.id).as_bytes().to_vec();
    stats::recompute_range_pausing(&prefix, (range, span.clone()), documents, fields, pause);
    stages::record(Stage::StatsBuild, nanos_since(started));
    Ok(())
}

/// The documents of `span`, counted by reading them, with a pause after each chunk.
fn counted<B: Backend + ?Sized>(
    backend: &mut B,
    span: &Span,
    pause: &mut dyn FnMut(),
) -> Result<u64, CommandError> {
    let mut from = span.0.clone();
    let mut n = 0u64;
    loop {
        let rows = backend.scan(&from, &span.1, COUNT_CHUNK, ReadConcern::Local)?;
        n += rows.iter().filter(|(_, v)| teseladb_keys::stored::raw(v).is_ok()).count() as u64;
        pause();
        match rows.last() {
            Some((k, _)) if rows.len() == COUNT_CHUNK => from = [k.as_slice(), &[0]].concat(),
            _ => return Ok(n),
        }
    }
}

impl PlanIndex {
    /// Whether the statistics read its fields' values from its entries.
    fn gives_statistics(&self) -> bool {
        self.meta.plannable
            && !self.is_id_index()
            && self.entry.buckets == 0
            && self.meta.key.iter().all(|(_, d)| d.abs() == 1)
            && !self.has_collation()
    }

    /// Its fields' paths, in its order.
    fn paths(&self) -> impl Iterator<Item = &str> {
        self.meta.key.iter().map(|(p, _)| p.as_str())
    }
}
