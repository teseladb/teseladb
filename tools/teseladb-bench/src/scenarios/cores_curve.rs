//! `cores-curve` (012 FR-009, SC-003): YCSB A, B and C against a node of 1, 2, 4, … cores, up to
//! `--max-cores` (16 by default, never more than the machine has). For each workload and number of
//! cores it reports the throughput, and the scaling efficiency: the throughput over `cores` times
//! the throughput with one core. SC-003 asks for 70 % or more up to 16 cores in YCSB C.

use super::ycsb;
use crate::cli::Args;
use crate::client::Target;
use crate::report::Report;

/// The workloads of the curve.
pub const WORKLOADS: [char; 3] = ['a', 'b', 'c'];

/// 1, 2, 4, … up to `max` (and `max` itself when it is not a power of two).
pub fn steps(max: u16) -> Vec<u16> {
    let max = max.max(1);
    let mut v: Vec<u16> =
        std::iter::successors(Some(1u16), |c| c.checked_mul(2)).take_while(|c| *c <= max).collect();
    if v.last() != Some(&max) {
        v.push(max);
    }
    v
}

/// Throughput of `cores` cores over `cores` times that of one.
pub fn efficiency(one: f64, cores: u16, tput: f64) -> f64 {
    if one <= 0.0 { 0.0 } else { tput / (one * f64::from(cores)) }
}

/// Runs the curve.
pub fn run(args: &Args) -> Result<Report, String> {
    let machine = std::thread::available_parallelism().map_or(1, |n| n.get());
    let max = args.max_cores.min(u16::try_from(machine).unwrap_or(u16::MAX));
    let mut report = Report::new("cores-curve");
    for letter in WORKLOADS {
        let mut one = 0.0;
        for cores in steps(max) {
            let target = Target::local(1, cores)?;
            let (out, _) = ycsb::run_on(args, letter, &target)?;
            let tput = out.throughput();
            if cores == 1 {
                one = tput;
            }
            let name = format!("ycsb-{letter}.cores-{cores}");
            report.push(&format!("{name}.throughput"), "op/s", tput, None);
            report.push(&format!("{name}.efficiency"), "ratio", efficiency(one, cores, tput), None);
            report.push(&format!("{name}.errors"), "op", out.errors as f64, None);
        }
    }
    report.notes.push(format!(
        "one node in this process, cores 1 to {max} (machine: {machine}), {} records, {} workers",
        args.records,
        args.concurrency.max(1)
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn steps_double_up_to_the_maximum() {
        assert_eq!(steps(1), [1]);
        assert_eq!(steps(0), [1]);
        assert_eq!(steps(16), [1, 2, 4, 8, 16]);
        assert_eq!(steps(12), [1, 2, 4, 8, 12]);
    }

    #[test]
    fn efficiency_is_relative_to_one_core() {
        assert_eq!(efficiency(100.0, 4, 300.0), 0.75);
        assert_eq!(efficiency(0.0, 4, 300.0), 0.0);
    }

    #[test]
    fn a_short_curve_reports_every_point() {
        let args = Args {
            max_cores: 2,
            records: 50,
            duration: std::time::Duration::from_millis(100),
            warmup: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = run(&args).unwrap();
        let names: Vec<&str> = r.metrics.iter().map(|m| m.name.as_str()).collect();
        for w in WORKLOADS {
            for c in [1, 2] {
                assert!(
                    names.contains(&format!("ycsb-{w}.cores-{c}.efficiency").as_str()),
                    "{names:?}"
                );
            }
        }
    }
}
