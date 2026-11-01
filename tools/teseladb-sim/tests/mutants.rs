//! The faulty variants (002 FR-011, T013), checked in `cargo test` with what is fast and
//! deterministic: the variant table of the simulator names exactly the variants the code declares
//! with `cfg(teseladb_bug = "...")`, each row with its checker, plan and seeds, and the logic of
//! `scripts/sim-mutants.sh` passes its own tests (`scripts/tests/sim_mutants_test.sh`, with a fake
//! simulator). Building every variant and running its seeds until its checker catches it is the
//! long job of gate 5 of `scripts/ci.sh` (`scripts/sim-mutants.sh`); this test used to run that pass
//! too, a second time inside the test battery, and no longer does: nothing is skipped, the duplicate
//! is gone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use teseladb_sim::variants::{NAMES, VARIANTS};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The Rust files under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "target") {
                rust_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The variant names the code declares under `crates/`, as `scripts/sim-mutants.sh` finds them.
fn declared() -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(&Path::new(ROOT).join("crates"), &mut files);
    let marker = "teseladb_bug = \"";
    let mut names = BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let name: String = rest.chars().take_while(|c| *c != '"').collect();
            let ok = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if ok {
                names.insert(name);
            }
        }
    }
    names
}

#[test]
fn the_variant_table_names_exactly_the_variants_of_the_code() {
    let table: BTreeSet<String> = NAMES.iter().map(|n| (*n).to_owned()).collect();
    let code = declared();
    let missing: Vec<&String> = code.difference(&table).collect();
    let stale: Vec<&String> = table.difference(&code).collect();
    assert!(missing.is_empty(), "in the code, not in the table: {missing:?}");
    assert!(stale.is_empty(), "in the table, not in the code: {stale:?}");
    assert!(NAMES.windows(2).all(|w| w[0] < w[1]), "the table is in alphabetical order");
    for v in VARIANTS {
        assert!(!v.checker.is_empty() && v.seeds > 0, "{v:?}");
    }
}

#[test]
fn the_script_logic_passes_its_tests() {
    // The script builds nothing here (its tests use a fake simulator); the flags of a coverage or
    // variant build of this test are cleared anyway, so no child build inherits them.
    let out = Command::new("bash")
        .arg(format!("{ROOT}/scripts/tests/sim_mutants_test.sh"))
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("LLVM_PROFILE_FILE")
        .env_remove("RUSTC_WRAPPER")
        .output()
        .expect("bash");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains(" 0 failed"), "{text}");
}
