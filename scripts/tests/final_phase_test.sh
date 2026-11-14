#!/usr/bin/env bash
# Tests of scripts/final-phase.sh: the dry run lists every long job of the final phase in the
# order of the plan with its parameters and estimate, the jobs it names exist in the tree (fuzzing
# targets, simulator workloads, benchmark scenarios, scripts), nothing it runs starts a CI runner
# or a deployment, and a run keeps its state so that a second run resumes where the first stopped.
# The runs use a table of fake phases (FINAL_PHASE_TABLE) in a temporary directory under target/.
#   scripts/tests/final_phase_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

mkdir -p target
WORK=$(mktemp -d "$PWD/target/final-phase-test.XXXXXX")
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
yes_if() { if "$@"; then echo 0; else echo 1; fi; }

# The dry run: every phase, in the plan's order, and no run directory.
out=$(FINAL_REPORTS="$WORK/dry" scripts/final-phase.sh --dry-run 2>&1) && rc=0 || rc=$?
check "the dry run ends with 0 (exit $rc)" "$rc" "$out"
check "the dry run writes nothing" "$(yes_if test ! -e "$WORK/dry")"
steps=$(awk -F'\t' '$1 ~ /^[0-9]+$/ {print $1}' <<<"$out")
check "the phases follow the plan's steps" "$(yes_if test "$steps" = "$(sort -n <<<"$steps")")" "$steps"
for p in build fuzz mutants tlc ci perf-reference perf-txn perf-contention perf-txn-readers \
  integrity-sim-heavy-200; do
  check "the dry run lists $p" "$(yes_if grep -q "	$p	" <<<"$out")"
done
# 002 T034 and 008 T014, T025: the scenarios of their measures, repeated on an idle machine.
for s in reference txn contention txn-readers; do
  check "the performance phase measures $s with its repetitions" \
    "$(yes_if grep -qE "perf_bench [0-9]+ 5 $s --duration 60s" <<<"$out")"
done
while IFS= read -r w; do
  for plan in none light default heavy; do
    check "the dry run sweeps $w with $plan" "$(yes_if grep -q "	sim-$w-$plan	" <<<"$out")"
  done
done < <(grep -o 'register_workload("[a-z0-9-]*"' tools/teseladb-sim/src/registry.rs | cut -d'"' -f2)
for d in crates/*/ tools/*/; do
  [[ -f $d/Cargo.toml ]] || continue
  c=$(basename "$d")
  check "the dry run measures the coverage of $c" "$(yes_if grep -q "	coverage-$c	" <<<"$out")"
done
check "the fuzzing lasts 24 h per target" "$(yes_if grep -q "fuzz_run 86400 " <<<"$out")"
check "special-collections takes 5000 seeds (015 T018)" \
  "$(yes_if grep -q -- "--workload special-collections --plan default --seeds 5000" <<<"$out")"
check "the dry run gives a total estimate" "$(yes_if grep -q "^total: " <<<"$out")"

# Every fuzzing target of the tree is in the table with FINAL_FUZZ_ALL=1.
all=$(FINAL_FUZZ_ALL=1 scripts/final-phase.sh --dry-run 2>&1)
while IFS= read -r f; do
  crate=$(cut -d/ -f2 <<<"$f")
  name=$(basename "$f" .rs)
  [[ $f == */src/* ]] && name=lib
  check "the fuzzing target $f is in the table" "$(yes_if grep -q "$crate:$name" <<<"$all")"
done < <(grep -rl TESELADB_FUZZ_SECS crates tools | grep '\.rs$' | sort)

# What the phases name exists: benchmark scenarios and scripts.
while IFS= read -r s; do
  check "the benchmark has the scenario $s" "$(yes_if grep -q "\"$s\"" tools/teseladb-bench/src/scenarios/mod.rs)"
done < <(grep -oE 'perf_bench [0-9]+ [^ ]+ [a-z0-9-]+' <<<"$all" | cut -d' ' -f4 | sort -u)
while IFS= read -r s; do
  check "$s is an executable script" "$(yes_if test -x "$s")"
done < <(grep -oE 'scripts/[A-Za-z0-9_/.-]+\.sh' <<<"$all" | sort -u)

# Nothing starts a runner or a deployment, in the commands nor in the script.
forbidden='git push|gh (workflow|run|pr|release)|docker push|terraform|tofu |nomad |kubectl|helm |aws |vault |consul '
check "no command starts a runner or a deployment" "$(yes_if test -z "$(grep -E "$forbidden" <<<"$all" || true)")"
check "the script has no such command" \
  "$(yes_if test -z "$(grep -vE '^\s*#' scripts/final-phase.sh | grep -E "$forbidden" || true)")"

# --quick: minimal parameters for a short check of each kind.
quick=$(scripts/final-phase.sh --dry-run --quick 2>&1)
check "--quick fuzzes for seconds" "$(yes_if grep -q "fuzz_run 5 " <<<"$quick")"
check "--quick sweeps two seeds" "$(yes_if grep -q -- "--plan heavy --seeds 2 " <<<"$quick")"

# A run of fake phases: b fails the first time; the run goes on, keeps the state and resumes.
table=$WORK/table.tsv
printf '%s\t%s\t%s\t%s\n' \
  1 a 1 "echo a >>$WORK/count; echo ran-a >\"\$PHASE_DIR/out.txt\"" \
  2 b 1 "echo b >>$WORK/count; test -e $WORK/fixed" \
  3 c 1 "echo c >>$WORK/count" >"$table"
run() { FINAL_PHASE_TABLE=$table FINAL_REPORTS="$WORK/reports" scripts/final-phase.sh "$@" 2>&1; }
out=$(run) && rc=0 || rc=$?
first=$(basename "$(find "$WORK/reports" -mindepth 1 -maxdepth 1)")
check "a failed phase makes the run fail (exit $rc)" "$(yes_if test "$rc" = 1)" "$out"
check "the run goes on after a failure" "$(yes_if test "$(tr -d '\n' <"$WORK/count")" = abc)"
check "the run directory carries the date" "$(yes_if grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2}_[0-9]{6}$' <<<"$first")" "$first"
check "each phase leaves its log" "$(yes_if test -f "$WORK/reports/$first/b.log")"
check "a phase writes into its own folder" "$(yes_if grep -q ran-a "$WORK/reports/$first/a/out.txt")"
check "the state says which phase failed" "$(yes_if grep -qE '^b	failed' "$WORK/reports/$first/state.tsv")"
touch "$WORK/fixed"
out=$(run) && rc=0 || rc=$?
check "the resumed run ends with 0 (exit $rc)" "$rc" "$out"
check "the resumed run only repeats the failed phase" "$(yes_if test "$(tr -d '\n' <"$WORK/count")" = abcb)"
check "the resumed run uses the same directory" "$(yes_if test "$(find "$WORK/reports" -mindepth 1 -maxdepth 1)" = "$WORK/reports/$first")"
check "a finished run is marked" "$(yes_if test -f "$WORK/reports/$first/finished")"
check "the summary lists every phase" "$(yes_if test "$(grep -cE '	ok	' "$WORK/reports/$first/summary.txt")" = 3)"
rm "$WORK/fixed"
sleep 1
out=$(run --stop-on-failure) && rc=0 || rc=$?
check "after a finished run a new one starts" "$(yes_if test "$(find "$WORK/reports" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 2)"
check "--stop-on-failure stops at the failure" "$(yes_if test "$(tr -d '\n' <"$WORK/count")" = abcbab)"
out=$(run --new --only c) && rc=0 || rc=$?
check "--only runs that phase (exit $rc)" "$(yes_if test "$(tr -d '\n' <"$WORK/count")" = abcbabc)" "$out"
out=$(run --new --from b) && rc=0 || rc=$?
check "--from starts there" "$(yes_if test "$(tr -d '\n' <"$WORK/count")" = abcbabcbc)"
out=$(run --only nope) && rc=0 || rc=$?
check "an unknown phase is a usage error (exit $rc)" "$(yes_if test "$rc" = 2)"
out=$(run --bogus) && rc=0 || rc=$?
check "an unknown option is a usage error (exit $rc)" "$(yes_if test "$rc" = 2)"

echo "final-phase tests: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
