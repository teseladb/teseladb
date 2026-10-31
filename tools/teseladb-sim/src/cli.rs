//! Command-line arguments (contract `sim-cli.md` of 002).

use teseladb_types::Nanos;

use teseladb_sim::SimConfig;
use teseladb_sim::faults::FaultPlan;

/// Checkers `--check-history` can run on an external history.
pub const HISTORY_CHECKERS: &[&str] = &["linearizable", "durability"];

/// What a run of seeds needs besides the simulation's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunArgs {
    /// Common configuration.
    pub cfg: SimConfig,
    /// First seed.
    pub start: u64,
    /// Number of seeds.
    pub seeds: u64,
    /// Child processes.
    pub jobs: usize,
    /// Child process of another run: it ends with a `CHILD` line to aggregate.
    pub child: bool,
    /// File of the traces.
    pub trace: Option<String>,
    /// File of the histories.
    pub history: Option<String>,
    /// Files carry the seed (and the workload) in their name even for one seed: a child of a run
    /// of several seeds.
    pub suffix_files: bool,
}

/// What to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run seeds.
    Run(RunArgs),
    /// Check an external JSONL history.
    CheckHistory {
        /// File.
        path: String,
        /// Checkers to run.
        checkers: Vec<String>,
    },
    /// Print the faulty variants with their checker, plan and seeds.
    ListVariants,
    /// Print the usage.
    Help,
}

/// Usage.
pub const USAGE: &str = "\
usage: teseladb-sim [--seeds <n> | --seed <s>] [--start <s>] [--jobs <n>] [--plan none|light|default|heavy]
                    [--fault-rate <pct>] [--workload <name>|all] [--nodes 3|5|7] [--cores 1..4]
                    [--steps <n>] [--sim-time <duration>] [--variant <cfg>] [--trace <file>]
                    [--history <file>]
       teseladb-sim --check-history <file> [--checkers linearizable,durability]
       teseladb-sim --list-variants";

fn duration(s: &str) -> Result<Nanos, String> {
    let (num, unit) = s.find(|c: char| !c.is_ascii_digit()).map_or((s, "s"), |i| s.split_at(i));
    let n: u64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    match unit {
        "ms" => Ok(Nanos::from_millis(n)),
        "s" => Ok(Nanos::from_secs(n)),
        "m" => Ok(Nanos::from_secs(n * 60)),
        _ => Err(format!("unknown duration unit: {s}")),
    }
}

/// Parses the arguments (without the program name).
pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|a| a == "--check-history") {
        return check_history(args);
    }
    if args.iter().any(|a| a == "--list-variants") {
        return match args {
            [_] => Ok(Command::ListVariants),
            _ => Err("--list-variants takes no other option".into()),
        };
    }
    let mut cfg = SimConfig { workload: "testkv".into(), ..SimConfig::default() };
    let mut run = RunArgs {
        cfg: cfg.clone(),
        start: 1,
        seeds: 200,
        jobs: 1,
        child: false,
        trace: None,
        history: None,
        suffix_files: false,
    };
    let (mut seed, mut seeds_given) = (None, false);
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("missing value for {a}"));
        let num = |v: &str| v.parse::<u64>().map_err(|_| format!("{a} needs a number"));
        match a {
            "--seeds" => (run.seeds, seeds_given) = (num(val()?)?, true),
            "--start" => run.start = num(val()?)?,
            "--seed" => seed = Some(num(val()?)?),
            "--jobs" => run.jobs = num(val()?)?.max(1) as usize,
            "--plan" => {
                let p = val()?;
                cfg.faults = FaultPlan::parse(p).ok_or_else(|| format!("unknown plan: {p}"))?;
            }
            "--fault-rate" => match num(val()?)? {
                r @ 1..=10_000 => cfg.fault_rate_pct = r as u32,
                _ => return Err("--fault-rate accepts 1 to 10000".into()),
            },
            "--workload" => {
                let w = val()?;
                if w != "all"
                    && teseladb_sim::registry::Registry::builtin().workload(w, 1, 1).is_none()
                {
                    return Err(format!("unknown or not yet available workload: {w}"));
                }
                cfg.workload = w.to_owned();
            }
            "--nodes" => match num(val()?)? {
                n @ (3 | 5 | 7) => cfg.nodes = n as u32,
                _ => return Err("--nodes accepts 3, 5 or 7".into()),
            },
            "--cores" => match num(val()?)? {
                c @ 1..=4 => cfg.cores = c as u16,
                _ => return Err("--cores accepts 1 to 4".into()),
            },
            "--steps" => match num(val()?)? {
                0 => return Err("--steps needs a number above 0".into()),
                s => cfg.steps = s,
            },
            "--sim-time" => cfg.sim_time = duration(val()?)?,
            "--variant" => teseladb_sim::variants::check(val()?)?,
            "--trace" => run.trace = Some(val()?.to_owned()),
            "--history" => run.history = Some(val()?.to_owned()),
            "--child" => run.child = true,
            "--suffix-files" => run.suffix_files = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unknown option: {other}\n{USAGE}")),
        }
    }
    if let Some(s) = seed {
        if seeds_given {
            return Err("--seed and --seeds do not go together".into());
        }
        (run.start, run.seeds) = (s, 1);
    }
    cfg.trace = run.trace.is_some();
    cfg.history = run.history.is_some();
    run.cfg = cfg;
    Ok(Command::Run(run))
}

fn check_history(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter().map(String::as_str);
    let (mut path, mut checkers) = (None, HISTORY_CHECKERS.iter().map(|c| c.to_string()).collect());
    while let Some(a) = it.next() {
        match a {
            "--check-history" => {
                path = Some(it.next().ok_or("--check-history needs a file")?.to_owned());
            }
            "--checkers" => {
                let list = it.next().ok_or("missing value for --checkers")?;
                checkers = list.split(',').map(str::to_owned).collect::<Vec<_>>();
                if let Some(bad) = checkers.iter().find(|c| !HISTORY_CHECKERS.contains(&c.as_str()))
                {
                    return Err(format!(
                        "unknown checker: {bad} ({})",
                        HISTORY_CHECKERS.join(", ")
                    ));
                }
            }
            other => return Err(format!("{other} does not go with --check-history")),
        }
    }
    let path = path.ok_or("--check-history needs a file")?;
    Ok(Command::CheckHistory { path, checkers })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        let args: Vec<String> = s.split_whitespace().map(String::from).collect();
        parse(&args)
    }

    fn run(s: &str) -> RunArgs {
        match p(s) {
            Ok(Command::Run(r)) => r,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn defaults_of_the_contract() {
        let r = run("");
        assert_eq!((r.start, r.seeds, r.jobs, r.child, r.suffix_files), (1, 200, 1, false, false));
        assert_eq!((r.trace, r.history), (None, None));
        let c = r.cfg;
        assert_eq!((c.workload.as_str(), c.nodes, c.cores), ("testkv", 3, 2));
        assert_eq!((c.faults, c.fault_rate_pct, c.steps), (FaultPlan::Default, 100, 200_000));
        assert_eq!((c.sim_time, c.trace, c.history), (Nanos::from_secs(20), false, false));
    }

    #[test]
    fn every_option() {
        let r = run(
            "--seeds 5 --start 7 --jobs 3 --plan heavy --fault-rate 250 --workload all --nodes 5 \
             --cores 4 --steps 900 --sim-time 3s --trace t --history h --child --suffix-files",
        );
        assert_eq!((r.start, r.seeds, r.jobs, r.child, r.suffix_files), (7, 5, 3, true, true));
        assert_eq!((r.trace.as_deref(), r.history.as_deref()), (Some("t"), Some("h")));
        let c = r.cfg;
        assert_eq!(
            (c.faults, c.fault_rate_pct, c.workload.as_str()),
            (FaultPlan::Heavy, 250, "all")
        );
        assert_eq!((c.nodes, c.cores, c.steps, c.sim_time), (5, 4, 900, Nanos::from_secs(3)));
        assert!(c.trace && c.history);
        let one = run("--seed 9 --sim-time 500ms --jobs 0");
        assert_eq!((one.start, one.seeds, one.jobs), (9, 1, 1));
        assert_eq!(one.cfg.sim_time, Nanos::from_millis(500));
        assert_eq!(run("--sim-time 2m").cfg.sim_time, Nanos::from_secs(120));
        assert_eq!(run("--sim-time 7").cfg.sim_time, Nanos::from_secs(7));
        assert_eq!(p("--help"), Ok(Command::Help));
        assert_eq!(p("-h"), Ok(Command::Help));
    }

    #[test]
    fn check_history_takes_only_its_checkers() {
        assert_eq!(
            p("--check-history h.jsonl"),
            Ok(Command::CheckHistory {
                path: "h.jsonl".into(),
                checkers: vec!["linearizable".into(), "durability".into()],
            })
        );
        assert_eq!(
            p("--checkers durability --check-history h"),
            Ok(Command::CheckHistory { path: "h".into(), checkers: vec!["durability".into()] })
        );
        assert!(p("--check-history h --checkers").unwrap_err().contains("missing value"));
    }

    #[test]
    fn errors() {
        for (s, what) in [
            ("--sim-time x", "invalid duration"),
            ("--seeds x", "--seeds needs a number"),
            ("--variant vote_twice", "built without a faulty variant"),
        ] {
            assert!(p(s).unwrap_err().contains(what), "{s}");
        }
    }
}
