//! What every command reads or writes of the node without the lock of its sessions (H-013 of the
//! native tests: a dozen takes of that lock per command made the workers wait for each other).
//!
//! The counters every command adds to are split in stripes: each worker thread writes its own
//! stripe, and `serverStatus` and `$collStats` add the stripes to the node's counters when they
//! read them. The counts stay exact: a reader takes every stripe's lock. The technique is that of
//! striped counters (Java's `LongAdder`, Linux's per-CPU counters); RefDb keeps these metrics
//! in atomic counters.
//!
//! The server parameters every command reads are kept here too, as atomics: they are their only
//! copy, which `setParameter` and `getParameter` write and read.
//!
//! Two flags say when a command has to look under the lock: a `failCommand` failpoint turned on,
//! and transactions whose intents wait to be resolved. Each is raised under the lock where the
//! state is set, and lowered only there too, so a lowered flag is never wrong; one left raised
//! costs a take of the lock.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use super::status::{CommandCount, StatusCounters};

/// Stripes: more than the worker threads of a node usually are.
const STRIPES: usize = 16;

/// What the commands of one node read and write without its lock.
#[derive(Debug)]
pub struct Hot {
    /// The command counters, by stripe.
    pub stats: CommandStats,
    /// A `failCommand` failpoint may be on.
    pub fail_command: AtomicBool,
    /// Coordinators may wait in `txn_resolving`.
    pub txn_resolving: AtomicBool,
    /// `internalQueryPlannerEnableHashIntersection`.
    pub hash_intersection: AtomicBool,
    /// `internalQueryMaxSingleExpressionMemoryUsageBytes`.
    pub expression_memory: AtomicI64,
    /// Whether server-side JavaScript runs: `false` with `--noscripting` or
    /// `security.javascriptEnabled: false` (016 FR-009).
    pub javascript_enabled: AtomicBool,
    /// Milliseconds up to which an operation that ended leaves nothing to the profiler or to the
    /// slow query log: `slowms` while no database is profiled or has a filter, `-1` otherwise.
    pub profiler_quiet_ms: AtomicI64,
}

impl Default for Hot {
    fn default() -> Hot {
        let expression = teseladb_exec::expr::optimize::EXPRESSION_MEMORY_DEFAULT;
        Hot {
            stats: CommandStats::default(),
            fail_command: AtomicBool::new(false),
            txn_resolving: AtomicBool::new(false),
            hash_intersection: AtomicBool::new(false),
            expression_memory: AtomicI64::new(i64::try_from(expression).unwrap_or(i64::MAX)),
            javascript_enabled: AtomicBool::new(true),
            profiler_quiet_ms: AtomicI64::new(super::profile::Profiler::default().quiet_ms()),
        }
    }
}

/// The command counters of one node, by stripe.
#[derive(Debug)]
pub struct CommandStats {
    stripes: Box<[Mutex<StatusCounters>]>,
}

impl Default for CommandStats {
    fn default() -> CommandStats {
        CommandStats { stripes: (0..STRIPES).map(|_| Mutex::default()).collect() }
    }
}

static NEXT_STRIPE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's stripe, given out in turn.
    static STRIPE: usize = NEXT_STRIPE.fetch_add(1, Ordering::Relaxed) % STRIPES;
    /// The counters this thread last wrote to: the node's address and a weak reference, which
    /// fails once that node is gone (another may come to the same address).
    static HELD: std::cell::RefCell<Option<(usize, Weak<Hot>)>> =
        const { std::cell::RefCell::new(None) };
}

impl CommandStats {
    /// This thread's stripe.
    pub fn stripe(&self) -> MutexGuard<'_, StatusCounters> {
        let i = STRIPE.with(|s| *s);
        self.stripes[i].lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds every stripe to `status`.
    pub fn add_to(&self, status: &mut StatusCounters) {
        for s in &self.stripes {
            status.absorb(&s.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }
}

/// What the node of `shared` keeps outside its lock, without that lock after the first command
/// of a thread.
pub fn of(shared: &super::Shared) -> Arc<Hot> {
    let key = std::ptr::from_ref(shared) as usize;
    let held = HELD
        .with(|h| h.borrow().as_ref().filter(|(k, _)| *k == key).and_then(|(_, w)| w.upgrade()));
    if let Some(s) = held {
        return s;
    }
    let hot = super::lock(shared).hot.clone();
    HELD.with(|h| *h.borrow_mut() = Some((key, Arc::downgrade(&hot))));
    hot
}

impl StatusCounters {
    /// Adds the counters a stripe takes (those of [`StatusCounters::command`],
    /// [`StatusCounters::collection_op`] and [`StatusCounters::api_version`], and the commands
    /// killed by their time limit or failed by a duplicate key).
    pub fn absorb(&mut self, o: &StatusCounters) {
        for (l, ol) in self.latency.iter_mut().zip(&o.latency) {
            l.micros = l.micros.saturating_add(ol.micros);
            l.ops += ol.ops;
        }
        self.unknown += o.unknown;
        self.killed_max_time.0 += o.killed_max_time.0;
        self.killed_max_time.1 += o.killed_max_time.1;
        self.duplicate_inserts += o.duplicate_inserts;
        for (name, c) in &o.commands {
            add_command(self.commands.entry(name.clone()).or_default(), c);
        }
        for (ns, c) in &o.collections {
            let mine = self.collections.entry(ns.clone()).or_default();
            for (l, ol) in mine.latency.iter_mut().zip(&c.latency) {
                l.micros = l.micros.saturating_add(ol.micros);
                l.ops += ol.ops;
            }
            for (h, oh) in mine.histograms.iter_mut().zip(&c.histograms) {
                for (bucket, n) in oh {
                    *h.entry(*bucket).or_default() += n;
                }
            }
            mine.collection_scans += c.collection_scans;
        }
        for (app, versions) in &o.api_versions {
            self.api_versions.entry(app.clone()).or_default().extend(versions.iter().cloned());
        }
    }
}

fn add_command(c: &mut CommandCount, o: &CommandCount) {
    c.total += o.total;
    c.failed += o.failed;
    c.array_filters += o.array_filters;
    c.pipeline += o.pipeline;
    c.validator += o.validator;
    c.json_schema += o.json_schema;
    for (level, n) in &o.levels {
        *c.levels.entry(level.clone()).or_default() += n;
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_bson::Document;

    #[test]
    fn the_stripes_add_up_to_what_one_set_of_counters_counts() {
        let stats = CommandStats::default();
        let mut one = StatusCounters::default();
        let cmd = Document::new().with("find", "c");
        // Commands written to several stripes, and the same to one set of counters.
        for (i, s) in stats.stripes.iter().enumerate().take(5) {
            let mut g = s.lock().unwrap();
            for c in [&mut *g, &mut one] {
                c.command("find", &cmd, 10 + i as u64, i % 2 == 0, true);
                c.command("nope", &cmd, 1, true, false);
                c.collection_op("db.c", "find", 10 + i as u64);
                c.api_version(Some("app"), Some("1"));
                c.killed_max_time.0 += 1;
                c.duplicate_inserts += 2;
            }
        }
        let mut sum = StatusCounters::default();
        stats.add_to(&mut sum);
        assert_eq!(sum, one);
    }

    #[test]
    fn a_thread_finds_the_counters_of_each_node_it_serves() {
        let (a, b) = (super::super::Shared::default(), super::super::Shared::default());
        assert!(Arc::ptr_eq(&of(&a), &super::super::lock(&a).hot));
        assert!(Arc::ptr_eq(&of(&b), &super::super::lock(&b).hot));
        assert!(Arc::ptr_eq(&of(&a), &super::super::lock(&a).hot));
        // A node gone and another one: the new one's counters.
        drop(a);
        let c = super::super::Shared::default();
        assert!(Arc::ptr_eq(&of(&c), &super::super::lock(&c).hot));
    }
}
