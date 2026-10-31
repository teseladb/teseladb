#!/usr/bin/env bash
# Tests of scripts/coverage.sh (001 FR-014) on a throwaway workspace with four crates: `full`, whose
# tests cover every line, function and branch; `half`, whose tests leave a function and a branch
# unexecuted; `moving`, whose test breaks its own source while it runs, as another agent editing
# the tree in the middle of a run would; and `failing`, with a test that fails. The script must
# pass `full` and `moving` alone, fail on the workspace naming `half`, and fail naming the test
# binary of `failing` after reporting its coverage.
#   scripts/tests/coverage_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/coverage.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

mkdir -p "$WORK/scripts" "$WORK/crates/full/src" "$WORK/crates/half/src" "$WORK/crates/moving/src" \
  "$WORK/crates/failing/src"
cp "$SCRIPT" "$WORK/scripts/coverage.sh"
cat > "$WORK/Cargo.toml" <<'EOF'
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(coverage)"] }
EOF
for c in full half moving failing; do
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
expect "a failing test fails the run naming its binary" 1 "^coverage: tests failed: .*failing-[0-9a-f]+$" failing
expect "the coverage of a run with a failing test is still reported" 1 "^failing +lines" failing

echo "coverage_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
