//! End to end: the `teseladb-bench` binary with its exit codes and its JSON output.

use std::process::Command;

fn bench(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_teseladb-bench")).args(args).output().unwrap()
}

#[test]
fn usage_errors_exit_with_2() {
    let out = bench(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    // Six letters, as the YCSB scenarios have, and not one of them.
    for name in ["nope", "ycsbxa"] {
        let out = bench(&["--scenario", name]);
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("unknown scenario"));
    }
}

#[test]
fn raftlog_scenario_writes_text_and_json() {
    let dir = std::env::temp_dir().join(format!("teseladb-bench-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let json = dir.with_extension("json");
    let out = bench(&[
        "--scenario",
        "raftlog",
        "--data",
        dir.to_str().unwrap(),
        "--records",
        "500",
        "--groups",
        "20",
        "--duration",
        "100ms",
        "--warmup",
        "0s",
        "--json",
        json.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("scenario raftlog") && text.contains("targets missed"));
    let j = std::fs::read_to_string(&json).unwrap();
    assert!(
        j.starts_with("{\"scenario\":\"raftlog\"") && j.contains("recovery.core1.wal_mb_per_s")
    );
    // Without being able to write the JSON: code 2.
    let out = bench(&[
        "--scenario",
        "raftlog-recover",
        "--data",
        dir.to_str().unwrap(),
        "--json",
        dir.join("no/such.json").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    // A directory that is not empty: scenario error, code 2.
    let out = bench(&["--scenario", "raftlog", "--data", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not empty"));
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_file(&json).unwrap();
}

#[test]
fn without_a_data_directory_the_scenario_uses_a_temporary_one() {
    let out = bench(&[
        "--scenario",
        "raftlog",
        "--records",
        "100",
        "--groups",
        "4",
        "--duration",
        "50ms",
        "--warmup",
        "0s",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("scenario raftlog"));
}

#[test]
fn the_range_scenarios_report_their_targets() {
    // 005 FR-021: `write` and `read` time an operation a minimum of times whatever the duration;
    // `failover` kills the leader 100 times (SC-004) in logical time, whatever the duration.
    let scenarios =
        [("write", "write.p99"), ("read", "read_lease.p50"), ("failover", "failover.p99")];
    for (scenario, metric) in scenarios {
        let out = bench(&["--scenario", scenario, "--duration", "0s", "--warmup", "0s"]);
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(&format!("scenario {scenario}")), "{text}");
        assert!(text.contains(metric) && text.contains("target"), "{text}");
    }
}

#[test]
fn a_warm_up_runs_before_the_timed_phases() {
    // 002 contract `bench-cli.md`: `--warmup` takes a duration whose operations do not count.
    let out = bench(&["--scenario", "bson", "--duration", "0s", "--warmup", "10ms"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("bson.read_1kib.p50"));
    let out = bench(&["--scenario", "bson", "--warmup", "soon"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid duration: soon"));
}
