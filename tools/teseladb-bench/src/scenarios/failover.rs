//! `failover` scenario (005 FR-021, SC-004): the leader of a range of three replicas is killed
//! 100 times, each time in a new cluster (its own folder and election timeouts); `failover` is the
//! time from the kill until a write through the new leader is confirmed, under 1 s at p99, and
//! `failover.lost_writes` counts the writes confirmed before the kill that the new leader does not
//! read (none). The time is the cluster's logical time, in ticks of 10 ms: it measures the
//! protocol's timeouts (election and heartbeats of `RaftConfig`), not the network nor the
//! processes; the measurement with processes and `SIGKILL` is `kill-leader.sh` (T029).
//! Neither `--duration` nor `--warmup` applies.

use teseladb_range::host::Reply;
use teseladb_types::Histogram;

use super::DataDir;
use super::write::{Cluster, doc};
use crate::cli::Args;
use crate::report::{Report, Target};

/// Kills of the measurement (SC-004).
const KILLS: u64 = 100;

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    run_with(args, KILLS)
}

fn run_with(args: &Args, kills: u64) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "failover")?;
    let mut replaced = Histogram::new();
    let mut lost = 0u64;
    for round in 0..kills {
        let path = dir.path.join(format!("round-{round:03}"));
        let mut c = Cluster::open(&path, round, false)?;
        let old = c.elect()?;
        let id = c.write(old, &doc(0));
        c.wait(old, id)?;
        c.kill(old);
        let killed_at = c.now;
        let new = c.elect()?;
        let id = c.write(new, &doc(1));
        c.wait(new, id)?;
        replaced.record((c.now - killed_at).0.max(1));
        let id = c.get(new, &doc(0));
        let read = c.wait(new, id)?;
        lost += u64::from(!matches!(read, Reply::Value { value: Some(_), .. }));
        drop(c);
        std::fs::remove_dir_all(&path).map_err(super::at(&path))?;
    }
    let mut report = Report::new("failover");
    report.latency("failover", &replaced, None, Some(1e9));
    report.push("failover.kills", "n", kills as f64, None);
    report.push("failover.lost_writes", "writes", lost as f64, Some(Target::AtMost(0.0)));
    report.notes.push(
        "logical time of three replicas in this process, ticks of 10 ms: the protocol's \
         timeouts, without network nor processes"
            .into(),
    );
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reports_every_metric() {
        let data = super::super::write::tests::scratch("failover");
        let args =
            Args { scenario: "failover".into(), data: Some(data.clone()), ..Args::default() };
        let r = run_with(&args, 3).unwrap();
        std::fs::remove_dir_all(&data).unwrap();
        let value = |name: &str| {
            r.metrics.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}")).value
        };
        assert_eq!(value("failover.kills"), 3.0);
        assert_eq!(value("failover.lost_writes"), 0.0);
        // At least an election timeout of 150 ms, well under a second.
        for name in ["failover.p50", "failover.p99", "failover.p999"] {
            assert!((150e6..1e9).contains(&value(name)), "{r}");
        }
        assert!(r.missed().is_empty(), "{r}");
    }
}
