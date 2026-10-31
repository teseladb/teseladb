//! Baselines (002 T028, FR-017, FR-022): a p50 or p99 more than 10 % worse than the baseline's ends
//! with 1, within 10 % with 0, and a baseline of another machine (another fingerprint) or of another
//! scenario with 2. `--compare <report>` checks a saved report without running the scenario, so the
//! percentages here are exact; `--save-baseline` writes the report with this machine's fingerprint.

use std::path::PathBuf;
use std::process::{Command, Output};

const FINGERPRINT: &str = r#"{"cpu":"Test CPU","cores":8,"memory_bytes":17179869184,"disk":"Test SSD","os":"TestOS 1.0"}"#;

fn bench(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_teseladb-bench")).args(args).output().expect("run")
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-bench-baseline-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("dir");
    d
}

/// A saved report of scenario `scenario` with p50, p99 and p99.9 of `op` and a fingerprint, on a
/// machine with a load of 0.1 per core.
fn report(name: &str, scenario: &str, p50: f64, p99: f64, p999: f64, fingerprint: &str) -> String {
    loaded_report(name, scenario, p50, p99, p999, fingerprint, 0.1)
}

fn loaded_report(
    name: &str,
    scenario: &str,
    p50: f64,
    p99: f64,
    p999: f64,
    fingerprint: &str,
    load: f64,
) -> String {
    let text = format!(
        r#"{{"scenario":"{scenario}","metrics":[
          {{"name":"op.p50","unit":"ns","value":{p50}}},
          {{"name":"op.p99","unit":"ns","value":{p99}}},
          {{"name":"op.p999","unit":"ns","value":{p999}}},
          {{"name":"throughput","unit":"op/s","value":1000}},
          {{"name":"host.load_per_core","unit":"load","value":{load}}}],
          "notes":[],"fingerprint":{fingerprint}}}"#
    );
    let path = dir().join(name);
    std::fs::write(&path, text).expect("write");
    path.display().to_string()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn eleven_per_cent_worse_fails_and_nine_passes() {
    let base = report("base.json", "x", 100.0, 1000.0, 5000.0, FINGERPRINT);
    let p50_11 = report("p50-11.json", "x", 111.0, 1000.0, 5000.0, FINGERPRINT);
    let out = bench(&["--compare", &p50_11, "--baseline", &base]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("op.p50") && text.contains("+11.0 %"), "{text}");
    let p99_11 = report("p99-11.json", "x", 100.0, 1110.0, 5000.0, FINGERPRINT);
    assert_eq!(code(&bench(&["--compare", &p99_11, "--baseline", &base])), 1);
    let both_9 = report("both-9.json", "x", 109.0, 1090.0, 5000.0, FINGERPRINT);
    let out = bench(&["--compare", &both_9, "--baseline", &base]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    // Only p50 and p99 count: a p99.9 50 % worse, or a better run, passes.
    let p999 = report("p999.json", "x", 50.0, 500.0, 7500.0, FINGERPRINT);
    assert_eq!(code(&bench(&["--compare", &p999, "--baseline", &base])), 0);
}

#[test]
fn a_busy_machine_or_unequal_loads_are_an_error() {
    // Identical runs of `reference` differed by 17.7 % in a p50 with the machine at 0.8 to 2.1
    // of load per core (002 T034): such runs are not compared.
    let base = report("quiet.json", "x", 100.0, 1000.0, 5000.0, FINGERPRINT);
    let busy = loaded_report("busy.json", "x", 100.0, 1000.0, 5000.0, FINGERPRINT, 1.5);
    let out = bench(&["--compare", &busy, "--baseline", &base]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("busy"), "{}", stderr(&out));
    let busier = loaded_report("busier.json", "x", 100.0, 1000.0, 5000.0, FINGERPRINT, 0.6);
    let out = bench(&["--compare", &busier, "--baseline", &base]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("not as busy"), "{}", stderr(&out));
}

#[test]
fn another_fingerprint_or_scenario_is_an_error() {
    let base = report("base2.json", "x", 100.0, 1000.0, 5000.0, FINGERPRINT);
    let other = FINGERPRINT.replace("Test SSD", "Other disk");
    let moved = report("moved.json", "x", 100.0, 1000.0, 5000.0, &other);
    let out = bench(&["--compare", &moved, "--baseline", &base]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("fingerprint") && stderr(&out).contains("Other disk"),
        "{}",
        stderr(&out)
    );
    let scenario = report("scenario.json", "y", 100.0, 1000.0, 5000.0, FINGERPRINT);
    let out = bench(&["--compare", &scenario, "--baseline", &base]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("scenario"), "{}", stderr(&out));
    let missing = dir().join("missing.json").display().to_string();
    assert_eq!(code(&bench(&["--compare", &missing, "--baseline", &base])), 2);
    let broken = dir().join("broken.json");
    std::fs::write(&broken, "{").expect("write");
    assert_eq!(
        code(&bench(&["--compare", broken.to_str().expect("path"), "--baseline", &base])),
        2
    );
    // A metric of the baseline that the report lacks is an error too.
    let lacking = dir().join("lacking.json");
    std::fs::write(
        &lacking,
        format!(
            r#"{{"scenario":"x","metrics":[{{"name":"host.load_per_core","unit":"load","value":0.1}}],"notes":[],"fingerprint":{FINGERPRINT}}}"#
        ),
    )
    .expect("write");
    let out = bench(&["--compare", lacking.to_str().expect("path"), "--baseline", &base]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("op.p50"), "{}", stderr(&out));
    assert_eq!(code(&bench(&["--compare", &base])), 2, "--compare needs --baseline");
}

#[test]
fn a_saved_baseline_carries_this_machines_fingerprint_and_passes_against_itself() {
    let saved = dir().join("saved.json");
    let out = bench(&[
        "--scenario",
        "keys",
        "--duration",
        "100ms",
        "--warmup",
        "0s",
        "--save-baseline",
        saved.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = std::fs::read_to_string(&saved).expect("saved");
    assert!(text.contains(r#""fingerprint":{"cpu":""#), "{text}");
    assert!(!text.contains(r#""cpu":"""#), "an empty CPU model: {text}");
    // The run records how busy the machine was; a quiet run compares with itself.
    let at = text.find(r#""name":"host.load_per_core","unit":"load","value":"#).expect("host load");
    let value_at = at + r#""name":"host.load_per_core","unit":"load","value":"#.len();
    let end = value_at + text[value_at..].find('}').expect("value");
    std::fs::write(&saved, format!("{}0.1{}", &text[..value_at], &text[end..])).expect("write");
    let path = saved.to_str().expect("path");
    assert_eq!(code(&bench(&["--compare", path, "--baseline", path])), 0);
    // A real run against a baseline of another machine stops before running.
    let base = report("foreign.json", "keys", 1.0, 1.0, 1.0, FINGERPRINT);
    let out = bench(&[
        "--scenario",
        "keys",
        "--duration",
        "100ms",
        "--warmup",
        "0s",
        "--baseline",
        &base,
    ]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("fingerprint"), "{}", stderr(&out));
}
