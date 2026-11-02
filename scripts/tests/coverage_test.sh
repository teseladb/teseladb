#!/usr/bin/env bash
# Tests of scripts/coverage.sh (001 FR-014) on a throwaway workspace with six crates: `full`, whose
# tests cover every line, function and branch; `half`, whose tests leave a function and a branch
# unexecuted; `moving`, whose test breaks its own source while it runs, as another agent editing
# the tree in the middle of a run would; `failing`, with a test that fails; and `generic`, whose
# generic function runs one arm in each of its two instantiations, so that each source line runs
# in one of them; and `program`, whose `main` only runs in the program its test starts. The script
# must pass `full`, `moving`, `generic` and `program` alone, fail on the workspace naming `half`, and
# fail naming the test binary of `failing` after reporting its coverage.
#   scripts/tests/coverage_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/coverage.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

mkdir -p "$WORK/scripts" "$WORK/crates/full/src" "$WORK/crates/half/src" "$WORK/crates/moving/src" \
  "$WORK/crates/failing/src" "$WORK/crates/generic/src" "$WORK/crates/program/src" \
  "$WORK/crates/program/tests" "$WORK/crates/elsewhere/src" "$WORK/crates/elsewhere/tests"
cp "$SCRIPT" "$WORK/scripts/coverage.sh"
cat > "$WORK/Cargo.toml" <<'EOF'
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(coverage)"] }
EOF
for c in full half moving failing generic program elsewhere; do
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\n\n[lints]\nworkspace = true\n' "$c" \
    > "$WORK/crates/$c/Cargo.toml"
done
cat > "$WORK/crates/full/src/lib.rs" <<'EOF'
pub fn sign(x: i32) -> i32 {
    if x < 0 { -1 } else { 1 }
}

#[cfg(test)]
mod tests {
    #[test]
    fn both_signs() {
        assert_eq!(super::sign(-3), -1);
        assert_eq!(super::sign(3), 1);
    }
}
EOF
cat > "$WORK/crates/half/src/lib.rs" <<'EOF'
pub fn sign(x: i32) -> i32 {
    if x < 0 { -1 } else { 1 }
}

pub fn never_called(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_positive() {
        assert_eq!(super::sign(3), 1);
    }
}
EOF

cat > "$WORK/crates/moving/src/lib.rs" <<'EOF'
pub fn twice(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn breaks_its_own_source() {
        assert_eq!(super::twice(2), 4);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs");
        let mut text = std::fs::read_to_string(path).unwrap();
        text.push_str("compile_error!(\"edited in the middle of the run\");\n");
        std::fs::write(path, text).unwrap();
    }
}
EOF
cat > "$WORK/crates/failing/src/lib.rs" <<'EOF'
pub fn one() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrong_expectation() {
        assert_eq!(super::one(), 2);
    }
}
EOF
cat > "$WORK/crates/generic/src/lib.rs" <<'EOF'
pub fn pick<T: Default + PartialEq>(x: T) -> &'static str {
    if x == T::default() {
        "zero"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_arm_in_each_instantiation() {
        assert_eq!(super::pick(0_i32), "zero");
        assert_eq!(super::pick(String::from("a")), "other");
    }
}
EOF
cat > "$WORK/crates/program/src/lib.rs" <<'EOF'
pub fn greeting() -> &'static str {
    "hi"
}
EOF
cat > "$WORK/crates/program/src/main.rs" <<'EOF'
fn main() {
    println!("{}", program::greeting());
}
EOF
cat > "$WORK/crates/program/tests/run.rs" <<'EOF'
#[test]
fn the_program_greets() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_program")).output().unwrap();
    assert_eq!(out.stdout, b"hi\n");
}
EOF

cp "$WORK/crates/full/src/lib.rs" "$WORK/crates/elsewhere/src/lib.rs"
cat > "$WORK/crates/elsewhere/tests/other_platform.rs" <<'EOF'
#![cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn runs_on_another_platform() {}
EOF

# expect NAME CODE PATTERN ARGS...: runs the script with ARGS and checks the exit code and output.
expect() {
  local name=$1 code=$2 pattern=$3 out rc=0
  shift 3
  out=$(cd "$WORK" && TESELADB_COV_MIN=95 scripts/coverage.sh "$@" 2>&1) || rc=$?
  if [[ $rc -eq $code ]] && grep -Eq -- "$pattern" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: $name (exit $rc, expected $code; /$pattern/ in output)"
    echo "  | ${out//$'\n'/$'\n'  | }"
  fi
}

expect "fully covered crate passes" 0 "^full +lines 100.00 % +functions 100.00 % +branches 100.00 %" full
expect "a crate below the minimum fails" 1 "^coverage: half below 95: lines, functions, branches$" full half
expect "the covered crate is reported in the same run" 1 "^full +lines 100.00 %" full half
expect "untaken branch side of the crate below" 1 "^  crates/half/src/lib.rs:2:[0-9]+: true$" full half
expect "function never run of the crate below" 1 "^  crates/half/src/lib.rs:5:1: function never run$" full half
expect "a source edited during the run does not change what is measured" 0 \
  "^moving +lines 100.00 % +functions 100.00 % +branches 100.00 %" moving
expect "a line run by another instantiation of its generic function counts" 0 \
  "^generic +lines 100.00 % +functions 100.00 % +branches 100.00 %" generic
expect "what only the program its test starts runs counts" 0 \
  "^program +lines 100.00 % +functions 100.00 % +branches 100.00 %" program
expect "a test file of another platform has nothing to measure and is left out" 0 \
  "^elsewhere +lines 100.00 % +functions 100.00 % +branches 100.00 %" elsewhere
expect "a failing test fails the run naming its binary" 1 "^coverage: tests failed: .*failing-[0-9a-f]+$" failing
expect "the coverage of a run with a failing test is still reported" 1 "^failing +lines" failing

echo "coverage_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
