//! End to end: the `teseladb-sim` binary (contract `sim-cli.md` of 002): a line per seed and a
//! summary, child processes with `--jobs`, `--steps`, `--plan` and `--fault-rate`, traces and
//! histories to files, `--variant`, `--check-history` with `--checkers`, and the exit codes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn sim(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_teseladb-sim")).current_dir(dir).args(args).output().unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-sim-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_line_per_seed_and_a_summary() {
    let d = tmp("lines");
    let o = sim(&d, &["--seeds", "3", "--start", "10", "--sim-time", "2s", "--plan", "heavy"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let text = stdout(&o);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    for (line, seed) in lines.iter().zip([10, 11, 12]) {
        assert!(line.starts_with(&format!("ok seed={seed} workload=testkv events=")), "{line}");
        assert!(line.contains(" lost=") && line.contains(" trace="), "{line}");
    }
    assert!(lines[3].starts_with("seeds=3 pass=3 fail=0 events="), "{}", lines[3]);
    // The same with child processes: the same lines, in seed order.
    let jobs = sim(
        &d,
        &["--seeds", "3", "--start", "10", "--sim-time", "2s", "--plan", "heavy", "--jobs", "2"],
    );
    assert_eq!(jobs.status.code(), Some(0));
    let same: Vec<String> = stdout(&jobs).lines().take(3).map(String::from).collect();
    assert_eq!(same, lines[..3].iter().map(|l| l.to_string()).collect::<Vec<_>>());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn every_workload_plan_rate_and_size() {
    let d = tmp("options");
    for args in [
        &["--seed", "1", "--sim-time", "1s", "--plan", "none", "--workload", "all"][..],
        &["--seed", "2", "--sim-time", "2s", "--plan", "light", "--nodes", "5", "--cores", "1"],
        &["--seed", "3", "--sim-time", "2s", "--plan", "default", "--fault-rate", "300"],
        &["--seed", "4", "--sim-time", "2s", "--workload", "storage", "--cores", "4"],
    ] {
        let o = sim(&d, args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}{}", stdout(&o), stderr(&o));
    }
    // `all` runs every workload for each seed.
    let all = stdout(&sim(
        &d,
        &["--seed", "1", "--sim-time", "1s", "--plan", "none", "--workload", "all"],
    ));
    assert!(all.contains("workload=storage") && all.contains("workload=testkv"), "{all}");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn steps_bound_a_seed_and_traces_and_histories_go_to_files() {
    let d = tmp("files");
    let o = sim(&d, &["--seed", "4", "--steps", "300", "--trace", "t.log", "--history", "h.jsonl"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    // The final check adds its own events after the 300 steps.
    let trace = std::fs::read_to_string(d.join("t.log")).unwrap();
    assert!(trace.lines().count() >= 300, "{}", trace.lines().count());
    let history = std::fs::read_to_string(d.join("h.jsonl")).unwrap();
    assert!(history.lines().all(|l| l.starts_with(r#"{"process":"#)), "{history}");
    // Several seeds: one file each.
    let o =
        sim(&d, &["--seeds", "2", "--sim-time", "1s", "--trace", "t.log", "--history", "h.jsonl"]);
    assert_eq!(o.status.code(), Some(0));
    for f in ["t.log.1", "t.log.2", "h.jsonl.1", "h.jsonl.2"] {
        assert!(d.join(f).exists(), "{f}");
    }
    // The history of a seed passes `--check-history`.
    let o = sim(&d, &["--check-history", "h.jsonl.1"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).starts_with("ok checkers=linearizable,durability operations="),
        "{}",
        stdout(&o)
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn check_history_reports_anomalies_and_bad_files() {
    let d = tmp("hist");
    let ok = r#"{"process":1,"type":"invoke","f":"cas","key":"g1/x","value":[null,"a"],"time":0,"node":0}
{"process":1,"type":"ok","f":"cas","key":"g1/x","value":[null,"a"],"time":5,"node":0}
{"process":2,"type":"invoke","f":"read","key":"g1/x","time":6,"node":1}
{"process":2,"type":"ok","f":"read","key":"g1/x","value":"a","time":7,"node":1}
"#;
    std::fs::write(d.join("ok.jsonl"), ok).unwrap();
    let o = sim(&d, &["--check-history", "ok.jsonl", "--checkers", "linearizable"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    assert_eq!(stdout(&o), "ok checkers=linearizable operations=2\n");
    // A read that does not see a write that ended before it.
    std::fs::write(
        d.join("stale.jsonl"),
        ok.replace(r#""value":"a","time":7"#, r#""value":null,"time":7"#),
    )
    .unwrap();
    let o = sim(&d, &["--check-history", "stale.jsonl"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).starts_with("FAIL checker=linearizable detail="), "{}", stdout(&o));
    for (args, what) in [
        (&["--check-history", "missing.jsonl"][..], "missing.jsonl"),
        (&["--check-history", "ok.jsonl", "--checkers", "durability,odd"], "unknown checker: odd"),
        (&["--check-history"], "--check-history needs a file"),
        (
            &["--check-history", "ok.jsonl", "--seeds", "3"],
            "--seeds does not go with --check-history",
        ),
    ] {
        let o = sim(&d, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains(what), "{args:?}: {}", stderr(&o));
    }
    std::fs::write(d.join("bad.jsonl"), "{nope").unwrap();
    let o = sim(&d, &["--check-history", "bad.jsonl"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("bad.jsonl: line 1"), "{}", stderr(&o));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn the_variants_are_listed_with_their_checker_plan_and_seeds() {
    let d = tmp("list");
    let o = sim(&d, &["--list-variants"]);
    assert_eq!(o.status.code(), Some(0));
    let text = stdout(&o);
    assert_eq!(text.lines().count(), teseladb_sim::variants::NAMES.len(), "{text}");
    assert!(text.lines().any(|l| l == "vote_twice raft_safety default 1000"), "{text}");
    for l in text.lines() {
        let f: Vec<&str> = l.split(' ').collect();
        assert_eq!(f.len(), 4, "{l}");
        assert!(f[3].parse::<u64>().is_ok(), "{l}");
    }
    let o = sim(&d, &["--list-variants", "--seeds", "2"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--list-variants takes no other option"), "{}", stderr(&o));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn variants_and_usage_errors() {
    let d = tmp("usage");
    // This build has no faulty variant: asking for one is a usage error that says how to build it.
    let o = sim(&d, &["--seed", "1", "--variant", "vote_twice"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("built without a faulty variant") && stderr(&o).contains("vote_twice"),
        "{}",
        stderr(&o)
    );
    let o = sim(&d, &["--seed", "1", "--variant", "nope"]);
    assert!(stderr(&o).contains("unknown variant: nope"), "{}", stderr(&o));
    for (args, what) in [
        (&["--nodes", "4"][..], "--nodes accepts 3, 5 or 7"),
        (&["--cores", "9"], "--cores accepts 1 to 4"),
        (&["--plan", "odd"], "unknown plan: odd"),
        (&["--fault-rate", "0"], "--fault-rate accepts 1 to 10000"),
        (&["--steps", "0"], "--steps needs a number above 0"),
        (&["--workload", "api"], "unknown or not yet available workload: api"),
        (&["--sim-time", "3h"], "unknown duration unit"),
        (&["--seeds", "2", "--seed", "1"], "--seed and --seeds do not go together"),
        (&["--seeds"], "missing value for --seeds"),
        (&["--nothing"], "unknown option: --nothing"),
        (&["run"], "unknown option: run"),
    ] {
        let o = sim(&d, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains(what), "{args:?}: {}", stderr(&o));
    }
    let help = sim(&d, &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).starts_with("usage: teseladb-sim"), "{}", stdout(&help));
    std::fs::remove_dir_all(&d).unwrap();
}
