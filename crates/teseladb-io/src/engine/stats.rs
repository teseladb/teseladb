//! What the engine of a node counts, read from outside its shards (023 FR-028, FR-030, FR-037;
//! `contracts/metrics.md`): its metrics for `/metrics`, under `teseladb_engine_` with the label
//! `shard`, the memory its allocator holds (`teseladb_memory_allocated_bytes`), and the
//! allocations of the process, which the tests of the hot path count per operation.
//!
//! An [`EngineProbe`] reaches every shard of the running engine; each shard reads its own
//! registry and its allocator's statistics, and the probe puts them together. The engine's own
//! metrics server is not started: the node's administration port serves them.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc;

use teseladb::engine::core::memory;
use teseladb::engine::core::metrics::impl_::{DataType, MetricValue, MetricValueU};
use teseladb::engine::core::metrics::impl_fns::get_values;

use super::post::{Gate, ShardPost};

/// The prefix of the engine's metrics in `/metrics`.
pub const PREFIX: &str = "teseladb_engine_";

/// The groups of the engine's metrics `/metrics` shows (`contracts/metrics.md`).
const GROUPS: [&str; 5] = ["reactor_", "scheduler_", "smp_", "memory_", "io_queue_"];

/// The kind of a family, as Prometheus' text names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Only grows.
    Counter,
    /// Goes up and down.
    Gauge,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

/// One family of the engine's metrics with the series of one or more shards.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    /// Its name without [`PREFIX`].
    pub name: String,
    /// Its description.
    pub help: String,
    /// Its kind.
    pub kind: Kind,
    /// Its series: the labels in Prometheus' text (`shard="0",class="log"`) and the value.
    pub series: Vec<(String, f64)>,
}

/// The allocator's counters of one shard (FR-030): what `memory::stats()` gives on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShardMemory {
    /// Allocations made on the shard.
    pub mallocs: u64,
    /// Frees of the shard's memory.
    pub frees: u64,
    /// Frees of other shards' memory made on this shard.
    pub cross_cpu_frees: u64,
    /// Allocations that failed.
    pub failed: u64,
    /// Frees of memory no shard owns made by this shard (D-181).
    pub foreign_frees: u64,
    /// Bytes of the shard's pool handed out.
    pub allocated: u64,
    /// Bytes of the shard's pool.
    pub total: u64,
}

impl ShardMemory {
    /// The calling shard's counters.
    fn here() -> ShardMemory {
        let s = memory::stats();
        ShardMemory {
            mallocs: s.mallocs(),
            frees: s.frees(),
            cross_cpu_frees: s.cross_cpu_frees(),
            failed: s.failed_allocations(),
            foreign_frees: s.foreign_frees().saturating_add(s.foreign_cross_frees()),
            allocated: s.allocated_memory() as u64,
            total: s.total_memory() as u64,
        }
    }
}

/// Allocations made so far on the calling shard (on another thread, those of its bucket of the
/// engine's counters for threads that are not shards): what a test of an allocation-free path
/// counts around it, run on a shard.
pub fn allocations_here() -> u64 {
    let s = memory::stats();
    s.mallocs().saturating_add(s.foreign_mallocs())
}

/// The value of a series as one number: a histogram gives its count of samples.
fn number(v: &MetricValue) -> f64 {
    match &v.u {
        MetricValueU::Double(d) => *d,
        MetricValueU::Histogram(h) => h.sample_count as f64,
    }
}

/// Labels in Prometheus' text, with `shard` always present and first.
fn labels_text<'a>(shard: u32, labels: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = format!("shard=\"{shard}\"");
    for (k, v) in labels.filter(|(k, _)| *k != "shard") {
        let v = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let _ = write!(out, ",{k}=\"{v}\"");
    }
    out
}

/// The engine's families of the groups in [`GROUPS`] on the calling shard (`shard`), and the
/// allocator's counts the engine does not register as metrics (`memory_foreign_frees`).
fn families_here(shard: u32) -> Vec<Family> {
    let mut out = Vec::new();
    let values = get_values();
    if let Some(copy) = values.get_local()
        && let Some(meta) = copy.metadata.as_deref()
    {
        for (fm, vals) in meta.iter().zip(copy.values.iter()) {
            let name = fm.mf.name.as_str();
            if !GROUPS.iter().any(|g| name.starts_with(g)) {
                continue;
            }
            // A histogram is published as the count of its samples (`reactor_stalls_total`).
            let (name, kind) = match fm.mf.type_ {
                DataType::Gauge => (name.to_owned(), Kind::Gauge),
                DataType::Counter | DataType::RealCounter => (name.to_owned(), Kind::Counter),
                DataType::Histogram | DataType::Summary => (format!("{name}_total"), Kind::Counter),
            };
            let series = fm
                .metrics
                .iter()
                .zip(vals.iter())
                .map(|(m, v)| {
                    let labels = m.labels().iter().map(|(k, v)| (k.as_str(), v.as_str()));
                    (labels_text(shard, labels), number(v))
                })
                .collect();
            out.push(Family { name, help: fm.mf.d.str().to_string(), kind, series });
        }
    }
    let m = ShardMemory::here();
    out.push(Family {
        name: "memory_foreign_frees".to_owned(),
        help: "Frees of memory no shard owns made on the shard (D-181).".to_owned(),
        kind: Kind::Counter,
        series: vec![(labels_text(shard, std::iter::empty()), m.foreign_frees as f64)],
    });
    out
}

/// Families of several shards merged by name, in the order they first appear.
pub fn merge(per_shard: Vec<Vec<Family>>) -> Vec<Family> {
    let mut out: Vec<Family> = Vec::new();
    for f in per_shard.into_iter().flatten() {
        match out.iter_mut().find(|o| o.name == f.name) {
            Some(o) => o.series.extend(f.series),
            None => out.push(f),
        }
    }
    out
}

/// `families` in Prometheus' text format, each name under [`PREFIX`].
pub fn render(families: &[Family], out: &mut String) {
    for f in families {
        let name = format!("{PREFIX}{}", f.name);
        let help = f.help.replace('\\', "\\\\").replace('\n', "\\n");
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {}", f.kind.as_str());
        for (labels, v) in &f.series {
            let _ = writeln!(out, "{name}{{{labels}}} {v}");
        }
    }
}

/// A way to every shard of the running engine of the process, from threads that are not shards
/// (the administration port, the tests).
#[derive(Clone)]
pub struct EngineProbe {
    gate: Arc<Gate>,
    shards: u16,
}

impl std::fmt::Debug for EngineProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineProbe").field("shards", &self.shards).finish()
    }
}

impl EngineProbe {
    /// The probe of the engine `gate` guards, with `shards` shards.
    pub(crate) fn new(gate: Arc<Gate>, shards: u16) -> EngineProbe {
        EngineProbe { gate, shards }
    }

    /// `f(shard)` on every shard, in shard order; the shards that cannot run it any more (the
    /// engine stopped) are left out.
    pub fn on_each<T: Send + 'static>(
        &self,
        f: impl Fn(u32) -> T + Clone + Send + 'static,
    ) -> Vec<T> {
        let mut waits = Vec::with_capacity(usize::from(self.shards));
        for shard in 0..u32::from(self.shards) {
            let (tx, rx) = mpsc::sync_channel(1);
            let f = f.clone();
            if ShardPost::new(shard, self.gate.clone()).post(move || drop(tx.send(f(shard)))) {
                waits.push(rx);
            }
        }
        waits.into_iter().filter_map(|rx| rx.recv().ok()).collect()
    }

    /// The engine's metrics in Prometheus' text format (`contracts/metrics.md`).
    pub fn metrics(&self) -> String {
        let mut out = String::new();
        render(&merge(self.on_each(families_here)), &mut out);
        out
    }

    /// Each shard's allocator counters, in shard order.
    pub fn memory(&self) -> Vec<ShardMemory> {
        self.on_each(|_| ShardMemory::here())
    }

    /// Bytes the engine's allocator holds for the shards (`teseladb_memory_allocated_bytes`).
    pub fn allocated_bytes(&self) -> u64 {
        self.memory().iter().map(|m| m.allocated).sum()
    }

    /// Allocations made so far by the shards and by the calling thread (FR-030): the count the
    /// tests of the hot path take per operation. The calling thread's are those its bucket of
    /// the engine's counters holds for threads that are not shards.
    pub fn allocations(&self) -> u64 {
        let shards: u64 = self.memory().iter().map(|m| m.mallocs).sum();
        shards.saturating_add(memory::stats().foreign_mallocs())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn family(name: &str, kind: Kind, series: &[(&str, f64)]) -> Family {
        Family {
            name: name.to_owned(),
            help: format!("{name} help"),
            kind,
            series: series.iter().map(|(l, v)| ((*l).to_owned(), *v)).collect(),
        }
    }

    #[test]
    fn the_shards_families_merge_by_name_in_order() {
        let a = vec![
            family("reactor_polls", Kind::Counter, &[("shard=\"0\"", 10.0)]),
            family("memory_free_memory", Kind::Gauge, &[("shard=\"0\"", 5.0)]),
        ];
        let b = vec![
            family("memory_free_memory", Kind::Gauge, &[("shard=\"1\"", 6.0)]),
            family("reactor_polls", Kind::Counter, &[("shard=\"1\"", 11.0)]),
        ];
        let m = merge(vec![a, b]);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "reactor_polls");
        assert_eq!(m[0].series, vec![("shard=\"0\"".into(), 10.0), ("shard=\"1\"".into(), 11.0)]);
        assert_eq!(m[1].series.len(), 2);
    }

    #[test]
    fn the_families_render_once_with_the_prefix_and_every_shard() {
        let mut out = String::new();
        let f = family(
            "memory_foreign_frees",
            Kind::Counter,
            &[("shard=\"0\"", 3.0), ("shard=\"1\"", 0.0)],
        );
        render(
            &[f, family("io_queue_delay", Kind::Gauge, &[("shard=\"0\",class=\"log\"", 0.5)])],
            &mut out,
        );
        assert_eq!(
            out,
            "# HELP teseladb_engine_memory_foreign_frees memory_foreign_frees help\n\
             # TYPE teseladb_engine_memory_foreign_frees counter\n\
             teseladb_engine_memory_foreign_frees{shard=\"0\"} 3\n\
             teseladb_engine_memory_foreign_frees{shard=\"1\"} 0\n\
             # HELP teseladb_engine_io_queue_delay io_queue_delay help\n\
             # TYPE teseladb_engine_io_queue_delay gauge\n\
             teseladb_engine_io_queue_delay{shard=\"0\",class=\"log\"} 0.5\n"
        );
    }

    #[test]
    fn labels_put_the_shard_first_and_escape_their_values() {
        let labels = [("class", "log"), ("shard", "7"), ("group", "a\"b\\c")];
        assert_eq!(
            labels_text(2, labels.into_iter()),
            "shard=\"2\",class=\"log\",group=\"a\\\"b\\\\c\""
        );
        assert_eq!(labels_text(0, std::iter::empty()), "shard=\"0\"");
    }
}
