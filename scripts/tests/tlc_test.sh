#!/usr/bin/env bash
# Tests of scripts/tlc.sh (002 FR-012, T016, T017): a model with a broken invariant makes TLC fail
# with its counterexample, and `--all` writes RESULTS.md with states, depth and date for every
# configuration, or nothing if one fails. The models live in a temporary directory under target/
# (without Java, TLC runs in Docker over the repository) and are deleted at the end.
#   scripts/tests/tlc_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

mkdir -p target
WORK=$(mktemp -d "$PWD/target/tlc-test.XXXXXX")
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

check() {
  if [[ $2 -eq 0 ]]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: $1"
    [[ -n ${3:-} ]] && echo "$3"
  fi
}

# A counter that goes up to 3 while the invariant says it never passes 2.
mkdir -p "$WORK/broken" "$WORK/good"
cat >"$WORK/broken/Broken.tla" <<'EOF'
---- MODULE Broken ----
EXTENDS Naturals
VARIABLE x
Init == x = 0
Next == x < 3 /\ x' = x + 1
Spec == Init /\ [][Next]_x
Safe == x <= 2
====
EOF
printf 'SPECIFICATION Spec\nINVARIANT Safe\n' >"$WORK/broken/Broken.cfg"

out=$(scripts/tlc.sh "$WORK/broken/Broken.tla" 2>&1) && rc=0 || rc=$?
ok=1
[[ $rc -ne 0 ]] && grep -q "Invariant Safe is violated" <<<"$out" &&
  grep -q "The behavior up to this point is" <<<"$out" && grep -q "x = 3" <<<"$out" && ok=0
check "a broken invariant fails with its counterexample (exit $rc)" "$ok" "$out"

# `--all` over two configurations of a good model.
cp spec/tla/Smoke.tla spec/tla/Smoke.cfg "$WORK/good/"
cp spec/tla/Smoke.cfg "$WORK/good/Smoke.second.cfg"
out=$(scripts/tlc.sh --all "$WORK/good" 2>&1) && rc=0 || rc=$?
results=$WORK/good/RESULTS.md
ok=1
if [[ $rc -eq 0 && -f $results ]] &&
  grep -q "^| \`Smoke\` | \`Smoke.cfg\` | 5 | 4 | [0-9][0-9]* | [0-9][0-9]* s |$" "$results" &&
  grep -q "^| \`Smoke\` | \`Smoke.second.cfg\` | 5 | 4 |" "$results" &&
  grep -q "el $(date +%d-%m-%Y) con TLA+ tools" "$results"; then
  ok=0
fi
check "--all writes a row per configuration with states, depth and date (exit $rc)" "$ok" \
  "$out$(cat "$results" 2>/dev/null)"

# With a failing model `--all` fails and leaves no RESULTS.md.
cp "$WORK/broken/Broken.tla" "$WORK/broken/Broken.cfg" "$WORK/good/"
find "$WORK/good" -name RESULTS.md -delete
out=$(scripts/tlc.sh --all "$WORK/good" 2>&1) && rc=0 || rc=$?
ok=1
[[ $rc -eq 1 && ! -f $results ]] && grep -q "Broken.cfg: TLC found an error" <<<"$out" && ok=0
check "--all fails without writing RESULTS.md when a model fails (exit $rc)" "$ok" "$out"

# Usage errors.
usage_error() {
  local rc
  scripts/tlc.sh "$@" >/dev/null 2>&1 && rc=0 || rc=$?
  check "usage error for '$*' (exit $rc)" "$([[ $rc -eq 2 ]] && echo 0 || echo 1)"
}
usage_error
usage_error --all /nonexistent
usage_error --all a b
usage_error missing.tla

echo "tlc_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
