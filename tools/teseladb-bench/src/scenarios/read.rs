//! `read` scenario (005 FR-021, SC-003): a `majority` read of a 100-byte document on the leader of
//! a range of three replicas, from the call to its `RangeHost` until its answer. `read_lease` holds
//! the reads its lease served, under 50 µs at p50; `read.lease_share` says which part of the reads
//! that is (the rest fell back to ReadIndex, a round of heartbeats). Every read enters the leader's
//! read cache, which holds 4 096 intervals: a run of more reads than that measures it full, as a
//! leader under load has it. The cluster is the `write` scenario's: its notes apply.

use std::hint::black_box;
use std::time::Instant;

use teseladb_types::{Histogram, NodeId, Rng};

use super::write::{Cluster, DOCS, OPS_PER_TICK, doc, latency_measured, notes};
use super::{DataDir, keep_going, warm_up};
use crate::cli::Args;
use crate::report::Report;

/// Reads timed at least, whatever `--duration` says.
const MIN_READS: u64 = 1000;

/// Reads a random document through `leader` and returns its latency in nanoseconds and whether
/// its lease served it; `ops` counts the reads, and the logical clock ticks every
/// [`OPS_PER_TICK`] of them, outside the timing.
fn read_next(
    c: &mut Cluster,
    leader: NodeId,
    rng: &mut Rng,
    ops: &mut u64,
) -> Result<(u64, bool), String> {
    let key = doc(rng.range(0, DOCS));
    let t = Instant::now();
    let id = c.get(leader, &key);
    black_box(c.wait(leader, id)?);
    let ns = t.elapsed().as_nanos() as u64;
    let (lease, _) = c.reads_by_lease(leader);
    *ops += 1;
    if ops.is_multiple_of(OPS_PER_TICK) {
        c.tick()?;
    }
    Ok((ns.max(1), lease > 0))
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    run_with(args, MIN_READS)
}

fn run_with(args: &Args, min_reads: u64) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "read")?;
    let mut c = Cluster::open(&dir.path, 0, true)?;
    let leader = c.elect()?;
    // The documents, [`OPS_PER_TICK`] writes in flight at a time (they share their syncs), with
    // the clock moving as in the timed loop: the lease starts with the first round of heartbeats of
    // the term, so a read right after the election would need ReadIndex.
    for first in (0..DOCS).step_by(OPS_PER_TICK as usize) {
        let ids: Vec<_> = (first..first + OPS_PER_TICK).map(|i| c.write(leader, &doc(i))).collect();
        for id in ids {
            c.wait(leader, id)?;
        }
        c.tick()?;
    }
    c.reads_by_lease(leader);
    let mut rng = Rng::new(5);
    let mut ops = 0;
    warm_up(args.warmup, || read_next(&mut c, leader, &mut rng, &mut ops).map(drop))?;
    // [0]: confirmed with ReadIndex; [1]: served by the lease.
    let mut by_lease = [Histogram::new(), Histogram::new()];
    let clock = Instant::now();
    let mut n = 0;
    while keep_going(n, min_reads, clock.elapsed() >= args.duration) {
        let (ns, lease) = read_next(&mut c, leader, &mut rng, &mut ops)?;
        by_lease[usize::from(lease)].record(ns);
        n += 1;
    }
    let mut report = Report::new("read");
    latency_measured(&mut report, "read_lease", &by_lease[1], Some(50_000.0), None);
    report.push("read.lease_share", "ratio", by_lease[1].count() as f64 / n as f64, None);
    report.push("read.count", "n", n as f64, None);
    notes(&mut report);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reports_every_metric() {
        let data = super::super::write::tests::scratch("read");
        let args = Args {
            scenario: "read".into(),
            data: Some(data.clone()),
            duration: std::time::Duration::ZERO,
            warmup: std::time::Duration::from_millis(1),
            ..Args::default()
        };
        let r = run_with(&args, 200).unwrap();
        std::fs::remove_dir_all(&data).unwrap();
        let value = |name: &str| {
            r.metrics.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}")).value
        };
        for name in ["read_lease.p50", "read_lease.p99", "read_lease.p999"] {
            assert!(value(name) > 0.0, "{r}");
        }
        assert_eq!(value("read.count"), 200.0);
        // The leader renews its lease with its heartbeats: every read is served by it.
        assert_eq!(value("read.lease_share"), 1.0, "{r}");
        let targets = r.metrics.iter().filter(|m| m.target.is_some()).count();
        assert_eq!(targets, 1, "{r}");
    }
}
