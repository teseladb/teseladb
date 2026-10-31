//! Smoke test of `scripts/tlc.sh` with `spec/tla/Smoke.tla`. It needs the TLA+ tools jar
//! (downloaded once and its SHA-256 verified) and Java or Docker: without them the test fails.

use std::process::Command;

#[test]
fn tlc_checks_the_smoke_model() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/scripts/tlc.sh"))
        .arg(format!("{root}/spec/tla/Smoke.tla"))
        .output()
        .expect("bash");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("No error has been found"), "{text}");
}
