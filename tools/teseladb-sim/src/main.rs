//! `teseladb-sim`: the simulator's CLI (contract `sim-cli.md` of 002). The tool's only I/O lives
//! here: trace and history files, and child processes with `--jobs`.

#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

mod cli;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, ExitCode, Stdio};
use std::time::Instant;

use teseladb_sim::checkers::{durability, linearizable};
use teseladb_sim::history::History;
use teseladb_sim::trace::short_hex;
use teseladb_sim::{SeedResult, SimConfig, Verdict, run_seed};

use crate::cli::{Command, RunArgs, USAGE};

/// Exit code of a usage or file error.
const USAGE_EXIT: u8 = 2;

/// Directory of the traces of the seeds that fail when no `--trace` was asked for.
const ARTIFACTS: &str = ".sim-artifacts";

/// The file of a seed: `base`, or with the workload and the seed in its name when there are several.
fn seed_file(base: &str, workload: &str, seed: u64, workloads: usize, suffix: bool) -> PathBuf {
    match (workloads > 1, suffix) {
        (true, _) => PathBuf::from(format!("{base}.{workload}.{seed}")),
        (false, true) => PathBuf::from(format!("{base}.{seed}")),
        (false, false) => PathBuf::from(base),
    }
}

/// Writes `text` to `path`, creating its directory; the error names the file.
fn write_text(path: &Path, text: &str) -> io::Result<()> {
    let at =
        |what: &Path, e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", what.display()));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return Err(at(dir, e));
    }
    match std::fs::write(path, text) {
        Ok(()) => Ok(()),
        Err(e) => Err(at(path, e)),
    }
}

fn lines_text(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// Where a seed's report goes and how its files are named.
struct Out<'a> {
    run: &'a RunArgs,
    workloads: usize,
    artifacts: &'a Path,
}

impl Out<'_> {
    fn file(&self, base: &str, r: &SeedResult) -> PathBuf {
        let suffix = self.run.suffix_files || self.run.seeds > 1;
        seed_file(base, &r.workload, r.seed, self.workloads, suffix)
    }

    /// Writes the files of `r` and returns its line and whether it passed; `traced` reruns the
    /// seed with its trace (the run is deterministic: it is the same run) when it failed and no
    /// `--trace` was asked for.
    fn report(
        &self,
        r: &SeedResult,
        traced: impl FnOnce() -> SeedResult,
    ) -> io::Result<(String, bool)> {
        if let Some(base) = &self.run.trace {
            write_text(
                &self.file(base, r),
                &lines_text(r.trace_lines.as_deref().unwrap_or_default()),
            )?;
        }
        if let (Some(base), Some(h)) = (&self.run.history, &r.history) {
            write_text(&self.file(base, r), h)?;
        }
        let Verdict::Fail(v) = &r.verdict else { return Ok((ok_line(r), true)) };
        let path = match &self.run.trace {
            Some(base) => self.file(base, r),
            None => {
                let path = self.artifacts.join(r.seed.to_string()).join("trace.log");
                write_text(
                    &path,
                    &lines_text(traced().trace_lines.as_deref().unwrap_or_default()),
                )?;
                path
            }
        };
        let line = format!(
            "FAIL seed={} workload={} step={} checker={} detail={} trace={}",
            r.seed,
            r.workload,
            r.events,
            v.checker,
            v.detail.replace('\n', " "),
            path.display()
        );
        Ok((line, false))
    }
}

fn ok_line(r: &SeedResult) -> String {
    let lost = if r.lost.is_empty() {
        "-".to_owned()
    } else {
        r.lost.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
    };
    format!(
        "ok seed={} workload={} events={} lost={lost} trace={}",
        r.seed,
        r.workload,
        r.events,
        short_hex(&r.trace_hash)
    )
}

/// Totals of a run: seeds that passed, that failed, and events.
type Totals = (u64, u64, u64);

/// Runs the seeds here, printing a line per seed.
fn run_here(out: &Out<'_>, cfg: &SimConfig) -> io::Result<Totals> {
    let (mut pass, mut fail, mut events) = (0, 0, 0);
    for seed in out.run.start..out.run.start + out.run.seeds {
        let r = run_seed(cfg, seed);
        let (line, ok) =
            out.report(&r, || run_seed(&SimConfig { trace: true, ..cfg.clone() }, seed))?;
        println!("{line}");
        if ok {
            pass += 1
        } else {
            fail += 1
        }
        events += r.events;
    }
    Ok((pass, fail, events))
}

/// The arguments of a child that runs `seeds` seeds from `start` of `cfg`'s workload.
fn child_args(run: &RunArgs, cfg: &SimConfig, start: u64, seeds: u64) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--child".into(),
        "--suffix-files".into(),
        "--start".into(),
        start.to_string(),
        "--seeds".into(),
        seeds.to_string(),
        "--workload".into(),
        cfg.workload.clone(),
        "--nodes".into(),
        cfg.nodes.to_string(),
        "--cores".into(),
        cfg.cores.to_string(),
        "--sim-time".into(),
        format!("{}ms", cfg.sim_time.as_millis()),
        "--plan".into(),
        cfg.faults.name().into(),
        "--fault-rate".into(),
        cfg.fault_rate_pct.to_string(),
        "--steps".into(),
        cfg.steps.to_string(),
    ];
    for (flag, file) in [("--trace", &run.trace), ("--history", &run.history)] {
        if let Some(f) = file {
            a.extend([flag.to_owned(), f.clone()]);
        }
    }
    a
}

/// Adds a child's `CHILD pass= fail= events=` line to `t`; any other line is printed.
fn take_child_line(line: &str, t: &mut Totals) {
    let Some(rest) = line.strip_prefix("CHILD ") else {
        println!("{line}");
        return;
    };
    let f: Vec<u64> = rest.split(' ').filter_map(|x| x.split_once('=')?.1.parse().ok()).collect();
    if let [p, fl, e] = f[..] {
        *t = (t.0 + p, t.1 + fl, t.2 + e);
    }
}

/// Spreads the seeds over `jobs` children, each with a consecutive range, and passes their lines
/// on in seed order.
fn run_jobs(run: &RunArgs, cfg: &SimConfig) -> io::Result<Totals> {
    let exe = std::env::current_exe()?;
    let jobs = (run.jobs as u64).min(run.seeds).max(1);
    let mut children = Vec::new();
    let mut next = run.start;
    for j in 0..jobs {
        let n = run.seeds / jobs + u64::from(j < run.seeds % jobs);
        let child =
            Proc::new(&exe).args(child_args(run, cfg, next, n)).stdout(Stdio::piped()).spawn()?;
        children.push(child);
        next += n;
    }
    let mut totals = (0, 0, 0);
    for c in children {
        let out = c.wait_with_output()?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            take_child_line(line, &mut totals);
        }
        if !matches!(out.status.code(), Some(0 | 1)) {
            return Err(io::Error::other(format!("a child process ended with {}", out.status)));
        }
    }
    Ok(totals)
}

fn run_seeds(run: &RunArgs) -> ExitCode {
    let t0 = Instant::now();
    let names: Vec<&str> = match run.cfg.workload.as_str() {
        "all" => teseladb_sim::registry::Registry::builtin().workload_names(),
        w => vec![w],
    };
    let out = Out { run, workloads: names.len(), artifacts: Path::new(ARTIFACTS) };
    let (mut pass, mut fail, mut events) = (0, 0, 0);
    for name in &names {
        let cfg = SimConfig { workload: (*name).to_owned(), ..run.cfg.clone() };
        let result =
            if run.jobs > 1 && run.seeds > 1 { run_jobs(run, &cfg) } else { run_here(&out, &cfg) };
        match result {
            Ok((p, f, e)) => (pass, fail, events) = (pass + p, fail + f, events + e),
            Err(e) => {
                eprintln!("teseladb-sim: {e}");
                return ExitCode::from(USAGE_EXIT);
            }
        }
    }
    if run.child {
        println!("CHILD pass={pass} fail={fail} events={events}");
    } else {
        println!(
            "seeds={} pass={pass} fail={fail} events={events} elapsed={:.1}",
            pass + fail,
            t0.elapsed().as_secs_f64()
        );
    }
    if fail > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

fn check_history(path: &str, checkers: &[String]) -> ExitCode {
    let read = match std::fs::read_to_string(path) {
        Ok(text) => History::from_jsonl(&text),
        Err(e) => Err(e.to_string()),
    };
    let h = match read {
        Ok(h) => h,
        Err(e) => {
            eprintln!("teseladb-sim: {path}: {e}");
            return ExitCode::from(USAGE_EXIT);
        }
    };
    for c in checkers {
        let r = match c.as_str() {
            "durability" => durability::check(&h),
            _ => linearizable::check(&h),
        };
        if let Err(v) = r {
            println!("FAIL checker={} detail={}", v.checker, v.detail.replace('\n', " "));
            return ExitCode::from(1);
        }
    }
    println!("ok checkers={} operations={}", checkers.join(","), h.ops.len());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(Command::Run(run)) => run_seeds(&run),
        Ok(Command::CheckHistory { path, checkers }) => check_history(&path, &checkers),
        Ok(Command::ListVariants) => {
            for v in teseladb_sim::variants::VARIANTS {
                println!("{}", teseladb_sim::variants::line(v));
            }
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("teseladb-sim: {msg}");
            ExitCode::from(USAGE_EXIT)
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_sim::checkers::Violation;
    use teseladb_types::Nanos;

    use super::*;

    fn result(seed: u64, verdict: Verdict, trace: Option<Vec<String>>) -> SeedResult {
        SeedResult {
            seed,
            workload: "testkv".into(),
            events: 77,
            sim_time: Nanos::ZERO,
            trace_hash: [0xab; 32],
            verdict,
            trace_lines: trace,
            lost: vec![1, 2],
            history: None,
            group0_bootstrapped: 1,
        }
    }

    fn run_args(trace: Option<&Path>) -> RunArgs {
        let cli::Command::Run(mut r) = cli::parse(&[]).unwrap() else { unreachable!() };
        r.seeds = 1;
        r.trace = trace.map(|p| p.display().to_string());
        r
    }

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("teseladb-sim-main-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_failed_seed_writes_its_trace_and_names_it() {
        let d = tmp("fail");
        let run = run_args(None);
        let out = Out { run: &run, workloads: 1, artifacts: &d };
        let failed = result(5, Verdict::Fail(Violation::safety("recovery", "n1:\nlost")), None);
        let traced = || result(5, Verdict::Pass, Some(vec!["a".into(), "b".into()]));
        let (line, ok) = out.report(&failed, traced).unwrap();
        let path = d.join("5").join("trace.log");
        assert!(!ok);
        assert_eq!(
            line,
            format!(
                "FAIL seed=5 workload=testkv step=77 checker=recovery detail=n1: lost trace={}",
                path.display()
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
        // With --trace the failure names that file and is not run again.
        let named = d.join("t.log");
        let run = run_args(Some(&named));
        let out = Out { run: &run, workloads: 1, artifacts: &d };
        let failed =
            result(6, Verdict::Fail(Violation::liveness("liveness", "x")), Some(vec!["z".into()]));
        let (line, _) = out.report(&failed, || unreachable!("not run again")).unwrap();
        assert!(line.ends_with(&format!("trace={}", named.display())), "{line}");
        assert_eq!(std::fs::read_to_string(&named).unwrap(), "z\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_passing_seed_lists_its_lost_nodes() {
        let run = run_args(None);
        let out = Out { run: &run, workloads: 1, artifacts: Path::new("unused") };
        let (line, ok) = out.report(&result(3, Verdict::Pass, None), || unreachable!()).unwrap();
        assert!(ok);
        assert_eq!(
            line,
            format!("ok seed=3 workload=testkv events=77 lost=1,2 trace={}", "ab".repeat(16))
        );
    }

    #[test]
    fn files_that_cannot_be_written_are_errors_that_name_them() {
        let d = tmp("errors");
        std::fs::create_dir_all(&d).unwrap();
        // A directory where the file should go.
        let e = write_text(&d, "x").unwrap_err();
        assert!(e.to_string().starts_with(&d.display().to_string()), "{e}");
        // A parent that is a file.
        let file = d.join("f");
        std::fs::write(&file, "").unwrap();
        let e = write_text(&file.join("child"), "x").unwrap_err();
        assert!(e.to_string().starts_with(&file.display().to_string()), "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn child_lines_are_added_or_passed_on() {
        let mut t = (1, 0, 10);
        take_child_line("CHILD pass=2 fail=1 events=30", &mut t);
        assert_eq!(t, (3, 1, 40));
        take_child_line("CHILD garbage", &mut t);
        take_child_line("ok seed=1", &mut t);
        assert_eq!(t, (3, 1, 40));
        assert_eq!(seed_file("h", "kv", 4, 2, false), PathBuf::from("h.kv.4"));
        assert_eq!(seed_file("h", "kv", 4, 1, true), PathBuf::from("h.4"));
        assert_eq!(seed_file("h", "kv", 4, 1, false), PathBuf::from("h"));
        // A child of a run of several seeds names its files by seed even with one seed.
        let mut run = run_args(None);
        run.suffix_files = true;
        let out = Out { run: &run, workloads: 1, artifacts: Path::new("unused") };
        assert_eq!(out.file("t", &result(8, Verdict::Pass, None)), PathBuf::from("t.8"));
    }
}
