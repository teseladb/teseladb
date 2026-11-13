//! `teseladb-bench`: TeselaDB's benchmarks with measurable targets (contract `bench-cli.md` of 002).
//! Exit codes: 0 without regression, 1 when a p50 or a p99 is more than 10 % worse than the
//! baseline's, 2 for a usage error, a failed scenario, a baseline of another machine or runs that
//! started on a machine too busy to compare them.

#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

mod baseline;
mod cli;
mod client;
mod fingerprint;
mod load;
mod open_loop;
mod report;
mod scenarios;

use std::path::Path;
use std::process::ExitCode;

use baseline::Saved;
use cli::{Args, Format};
use fingerprint::Fingerprint;

/// Exit code of a regression.
const REGRESSION: u8 = 1;
/// Exit code of a usage error, a failed scenario, a baseline of another machine or loads that do
/// not compare.
const ERROR: u8 = 2;

fn read_saved(path: &Path) -> Result<Saved, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Saved::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Compares with the baseline and prints the deltas; Ok(true) when there is no regression.
fn against(base: &Saved, now: &Saved) -> Result<bool, String> {
    let deltas = baseline::compare(base, now)?;
    print!("against the baseline:\n{}", baseline::render(&deltas));
    Ok(!deltas.iter().any(baseline::Delta::regressed))
}

fn run(args: &Args) -> Result<bool, String> {
    let base = args.baseline.as_deref().map(read_saved).transpose()?;
    if let Some(report) = &args.compare {
        let base = base.ok_or("--compare needs --baseline")?;
        return against(&base, &read_saved(report)?);
    }
    let machine = Fingerprint::current();
    // A baseline of another scenario or machine is refused before spending the run.
    if let Some(b) = &base {
        baseline::same_run(b, &args.scenario, &machine)?;
    }
    // How busy the machine was before the run, without the run's own threads (002 T034).
    let load = fingerprint::load_average();
    let mut report = scenarios::run(args)?;
    if let Some(l) = load {
        report.push(baseline::HOST_LOAD, "load", l / machine.cores.max(1) as f64, None);
    }
    report.fingerprint = Some(machine.clone());
    match args.format {
        Format::Text => println!("{report}"),
        Format::Json => println!("{}", report.to_json()),
    }
    for path in [&args.json, &args.save_baseline].into_iter().flatten() {
        write(path, &report.to_json())?;
    }
    match &base {
        Some(b) => against(b, &Saved::of(&report, machine)),
        None => Ok(true),
    }
}

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(ERROR);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(REGRESSION),
        Err(msg) => {
            eprintln!("teseladb-bench: {msg}");
            ExitCode::from(ERROR)
        }
    }
}
