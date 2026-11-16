//! TeselaDB against the reference server (002 T053, FR-028): `--versus <reports> --reference
//! <reports>` takes the repeats of each side, and ends with 1 when a TeselaDB median of the
//! throughput or of a p50, p99 or p99.9 is worse than the reference's by more than the larger
//! spread of the two sides, with 0 when it is not, and with 2 when the reports do not compare.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Command, Output};

const FINGERPRINT: &str = r#"{"cpu":"Test CPU","cores":8,"memory_bytes":17179869184,"disk":"Test SSD","os":"TestOS 1.0"}"#;

fn bench(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_teseladb-bench")).args(args).output().expect("run")
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-bench-versus-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("dir");
    d
}

/// One repeat of a side: what `--json` writes, with p50, p99 and p99.9 of `op` in nanoseconds
/// and the throughput.
struct Run<'a> {
    scenario: &'a str,
    concurrency: u32,
    p50: f64,
    p99: f64,
    p999: f64,
    throughput: f64,
    fingerprint: &'a str,
}

const RUN: Run<'static> = Run {
    scenario: "ycsb-a",
    concurrency: 8,
    p50: 100.0,
    p99: 1000.0,
    p999: 5000.0,
    throughput: 10_000.0,
    fingerprint: FINGERPRINT,
};

fn save(name: &str, r: &Run) -> String {
    let text = format!(
        r#"{{"scenario":"{}","concurrency":{},"metrics":[
          {{"name":"op.p50","unit":"ns","value":{}}},
          {{"name":"op.p99","unit":"ns","value":{}}},
          {{"name":"op.p999","unit":"ns","value":{}}},
          {{"name":"throughput","unit":"op/s","value":{}}},
          {{"name":"errors","unit":"op","value":0}},
          {{"name":"host.load_per_core","unit":"load","value":0.1}}],
          "notes":[],"fingerprint":{}}}"#,
        r.scenario, r.concurrency, r.p50, r.p99, r.p999, r.throughput, r.fingerprint
    );
    let path = dir().join(name);
    std::fs::write(&path, text).expect("write");
    path.display().to_string()
}

/// Saves the repeats and joins their paths with commas, as the option takes them.
fn side(prefix: &str, runs: &[Run]) -> String {
    runs.iter()
        .enumerate()
        .map(|(i, r)| save(&format!("{prefix}-{i}.json"), r))
        .collect::<Vec<_>>()
        .join(",")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_median_worse_beyond_the_spread_loses() {
    // The reference's p50 goes from 95 to 105 (spread 10, median 100); TeselaDB's from 112 to 114.
    let reference = side(
        "ref-a",
        &[Run { p50: 95.0, ..RUN }, Run { p50: 100.0, ..RUN }, Run { p50: 105.0, ..RUN }],
    );
    let ours = side(
        "ours-a",
        &[Run { p50: 112.0, ..RUN }, Run { p50: 113.0, ..RUN }, Run { p50: 114.0, ..RUN }],
    );
    let out = bench(&["--versus", &ours, "--reference", &reference]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let text = stdout(&out);
    let line = text.lines().find(|l| l.contains("op.p50")).expect("p50 line");
    assert!(line.contains("LOSS"), "{text}");
    assert!(text.lines().filter(|l| l.contains("LOSS")).count() == 1, "{text}");
    // Within the spread it does not: 13 worse with a spread of 10 loses, 8 worse does not.
    let ours = side(
        "ours-b",
        &[Run { p50: 107.0, ..RUN }, Run { p50: 108.0, ..RUN }, Run { p50: 109.0, ..RUN }],
    );
    let out = bench(&["--versus", &ours, "--reference", &reference]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!stdout(&out).contains("LOSS"));
}

#[test]
fn the_larger_spread_of_the_two_sides_counts() {
    // TeselaDB's own repeats spread over 30: a median 20 worse is inside it.
    let reference = side("ref-c", &[Run { p99: 1000.0, ..RUN }, Run { p99: 1000.0, ..RUN }]);
    let ours = side(
        "ours-c",
        &[Run { p99: 1005.0, ..RUN }, Run { p99: 1020.0, ..RUN }, Run { p99: 1035.0, ..RUN }],
    );
    let out = bench(&["--versus", &ours, "--reference", &reference]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
}

#[test]
fn less_throughput_or_a_worse_p999_loses_and_one_repeat_has_no_spread() {
    let reference = side("ref-d", &[RUN]);
    let slower = side("ours-d", &[Run { throughput: 9_999.0, ..RUN }]);
    let out = bench(&["--versus", &slower, "--reference", &reference]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(stdout(&out).lines().any(|l| l.contains("throughput") && l.contains("LOSS")));
    let tail = side("ours-e", &[Run { p999: 5001.0, ..RUN }]);
    let out = bench(&["--versus", &tail, "--reference", &reference]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).lines().any(|l| l.contains("op.p999") && l.contains("LOSS")));
    // Better everywhere, or equal, wins.
    let better =
        side("ours-f", &[Run { p50: 50.0, p99: 500.0, p999: 2500.0, throughput: 20_000.0, ..RUN }]);
    assert_eq!(code(&bench(&["--versus", &better, "--reference", &reference])), 0);
    assert_eq!(code(&bench(&["--versus", &reference, "--reference", &reference])), 0);
}

#[test]
fn reports_that_do_not_compare_are_an_error() {
    let reference = side("ref-g", &[RUN]);
    for (name, run, want) in [
        ("scenario", Run { scenario: "ycsb-b", ..RUN }, "scenario"),
        ("concurrency", Run { concurrency: 32, ..RUN }, "concurrency"),
        (
            "machine",
            Run { fingerprint: &FINGERPRINT.replace("Test SSD", "Other"), ..RUN },
            "machine",
        ),
    ] {
        let ours = side(&format!("ours-g-{name}"), &[run]);
        let out = bench(&["--versus", &ours, "--reference", &reference]);
        assert_eq!(code(&out), 2, "{name}");
        assert!(stderr(&out).contains(want), "{name}: {}", stderr(&out));
    }
    // Repeats of one side that are not of the same run.
    let mixed = side("ours-h", &[RUN, Run { concurrency: 1, ..RUN }]);
    let out = bench(&["--versus", &mixed, "--reference", &reference]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("concurrency"), "{}", stderr(&out));
    // A measure of the reference that TeselaDB's reports lack.
    let lacking = dir().join("lacking.json");
    std::fs::write(
        &lacking,
        format!(
            r#"{{"scenario":"ycsb-a","concurrency":8,"metrics":[{{"name":"throughput","unit":"op/s","value":1}}],"notes":[],"fingerprint":{FINGERPRINT}}}"#
        ),
    )
    .expect("write");
    let out = bench(&["--versus", lacking.to_str().expect("path"), "--reference", &reference]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("op.p50"), "{}", stderr(&out));
    let missing = dir().join("missing.json").display().to_string();
    assert_eq!(code(&bench(&["--versus", &missing, "--reference", &reference])), 2);
    let out = bench(&["--versus", &reference]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("--reference"), "{}", stderr(&out));
}

#[test]
fn a_run_writes_its_concurrency_in_the_report() {
    let path = dir().join("keys.json");
    let out = bench(&[
        "--scenario",
        "keys",
        "--duration",
        "100ms",
        "--warmup",
        "0s",
        "--concurrency",
        "3",
        "--json",
        path.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = std::fs::read_to_string(&path).expect("report");
    assert!(text.contains("\"concurrency\":3"), "{text}");
}
