//! `prefetch` scenario (009 FR-024): the walk of an intersection over the entries of an index,
//! each entry checked against the set of local identifiers of another index, without prefetch
//! and with the set prefetched [`AHEAD`] entries on. The two run on the same entries in one
//! process; the scan of blocks the LSM keeps in memory is measured by the `lsm` scenario on the
//! commits before and after the prefetch.

use std::hint::black_box;

use teseladb_index::docset::Held;
use teseladb_keys::prefetch::AHEAD;

use super::per_op;
use crate::cli::Args;
use crate::report::Report;

/// Documents of the held set: large enough that its containers leave the CPU's caches.
const HELD: u32 = 4_000_000;
/// Entries of the streamed scan, each walk.
const WALKED: usize = 4096;

/// A simple generator, so that the entries are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let mut rng = Rng(0x0009_F024_0000_0001);
    // Every third document of the range is in the held set.
    let pks: Vec<[u8; 4]> = (0..HELD).map(u32::to_be_bytes).collect();
    let held = Held::of(pks.iter().zip(0u32..).filter(|(_, id)| id % 3 == 0).map(|(pk, id)| {
        let pk: &[u8] = pk;
        (pk, Some(id))
    }));
    let walked: Vec<(&[u8], Option<u32>)> = (0..WALKED)
        .map(|_| {
            let id = u32::try_from(rng.next() % u64::from(HELD)).unwrap_or(0);
            let pk: &[u8] = &pks[id as usize];
            (pk, Some(id))
        })
        .collect();
    let mut report = Report::new("prefetch");
    let plain = per_op(args.warmup, args.duration, || {
        black_box(held.may_hold_each_ahead(black_box(&walked), 0));
    });
    report.latency("prefetch.intersection_walk_4096.without", &plain, None, None);
    let ahead = per_op(args.warmup, args.duration, || {
        black_box(held.may_hold_each_ahead(black_box(&walked), AHEAD));
    });
    report.latency("prefetch.intersection_walk_4096.ahead", &ahead, None, None);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reports_both_walks() {
        let args = Args {
            scenario: "prefetch".into(),
            duration: std::time::Duration::from_millis(20),
            warmup: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = run(&args).unwrap();
        for name in [
            "prefetch.intersection_walk_4096.without.p50",
            "prefetch.intersection_walk_4096.ahead.p50",
        ] {
            assert!(r.metrics.iter().any(|m| m.name == name), "{name}");
        }
    }
}
