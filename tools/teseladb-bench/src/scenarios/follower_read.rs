//! `follower-read` scenario (007 FR-024, SC-004): a document read on a follower of a range of three
//! replicas in this process, at the follower's closed mark, without the leader. `follower-read` is
//! the time of one read on the machine's clock, under 100 µs at p50; `follower-read.staleness` is
//! how far behind the cluster's clock the mark of each read was, never past `maxStalenessSeconds`
//! (90 s, the reference server's smallest). The cluster's time is logical: the replicas run in
//! ticks of 10 ms, and the reads are timed apart from the ticks.

use std::time::Instant;

use teseladb_range::host::Reply;
use teseladb_types::{Histogram, NodeId};

use super::DataDir;
use super::write::{Cluster, doc};
use crate::cli::Args;
use crate::report::{Report, Target};

/// Documents written before the reads.
const DOCS: u64 = 100;
/// Reads measured.
const READS: u64 = 10_000;
/// Ticks of 10 ms before the reads: past the closed mark's lag and the log's mark, so that the
/// follower's mark covers the writes.
const SETTLE_TICKS: u32 = 1_000;
/// p50 of a read (SC-004), in nanoseconds.
const P50_NS: f64 = 100_000.0;
/// Most staleness of a read's mark (the reference server's smallest `maxStalenessSeconds`), in
/// milliseconds.
const MAX_STALENESS_MS: f64 = 90_000.0;

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    run_with(args, READS)
}

fn run_with(args: &Args, reads: u64) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "follower-read")?;
    let mut c = Cluster::open(&dir.path, 1, false)?;
    let leader = c.elect()?;
    for i in 0..DOCS {
        let id = c.write(leader, &doc(i));
        c.wait(leader, id)?;
    }
    for _ in 0..SETTLE_TICKS {
        c.tick()?;
    }
    let follower = (1..=3).map(NodeId).find(|n| *n != leader).expect("three replicas");
    let (mut latency, mut staleness) = (Histogram::new(), 0u64);
    for i in 0..reads {
        let t0 = Instant::now();
        let id = c.read_follower(follower, &doc(i % DOCS));
        let reply = c.wait(follower, id)?;
        latency.record(t0.elapsed().as_nanos() as u64);
        let Reply::Value { ts, value: Some(_) } = reply else {
            return Err(format!("read {i}: no document at the follower's mark: {reply:?}"));
        };
        staleness = staleness.max(c.wall_ms().saturating_sub(ts.physical));
    }
    let mut report = Report::new("follower-read");
    report.latency("follower-read", &latency, Some(P50_NS), None);
    report.push("follower-read.reads", "n", reads as f64, None);
    report.push(
        "follower-read.staleness",
        "ms",
        staleness as f64,
        Some(Target::AtMost(MAX_STALENESS_MS)),
    );
    report.notes.push(
        "three replicas in this process in logical time; each read timed on the machine's clock"
            .into(),
    );
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reads_every_document_at_the_follower() {
        let data = super::super::write::tests::scratch("follower-read");
        let args =
            Args { scenario: "follower-read".into(), data: Some(data.clone()), ..Args::default() };
        let r = run_with(&args, 50).unwrap();
        std::fs::remove_dir_all(&data).unwrap();
        let value = |name: &str| {
            r.metrics.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}")).value
        };
        assert_eq!(value("follower-read.reads"), 50.0);
        // The mark trails the clock by the closed mark's lag, and by the log's period at most.
        let staleness = value("follower-read.staleness");
        assert!((3_000.0..=9_000.0).contains(&staleness), "{staleness}");
    }
}
