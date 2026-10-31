#!/usr/bin/env bash
# Tests of scripts/sim-mutants.sh (001 FR-016, 002 T013) with a fake simulator (`TESELADB_SIM_CMD`)
# that lists the variants of `LIST` (or the three of the code) and reports a failure, by the checker
# in `CHECKER` (or raft_safety), for the variants listed in `DETECTS`.
#   scripts/tests/sim_mutants_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/sim-mutants.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

# A repository with three faulty variants and the fake simulator.
mkdir -p "$WORK/scripts" "$WORK/crates/a/src" "$WORK/crates/b/src"
cp "$SCRIPT" "$WORK/scripts/sim-mutants.sh"
cat > "$WORK/crates/a/src/lib.rs" <<'EOF'
#[cfg(teseladb_bug = "vote_twice")]
fn a() {}
#[cfg(teseladb_bug = "ack_before_fsync")]
fn b() {}
EOF
cat > "$WORK/crates/b/src/lib.rs" <<'EOF'
#[cfg(teseladb_bug = "vote_twice")]
fn c() {}
#[cfg(teseladb_bug = "lease_without_check_quorum")]
fn d() {}
EOF
cat > "$WORK/fake-sim" <<'EOF'
#!/usr/bin/env bash
# Called as `fake-sim --list-variants`, or as `fake-sim --workload all --plan PLAN --start S
# --seeds N --jobs J --variant V` with the variant also in TESELADB_BUG. REFUSE makes it answer as to
# arguments it does not take.
if [[ $1 == --list-variants ]]; then
  printf '%s\n' "${LIST:-ack_before_fsync raft_safety default 40
lease_without_check_quorum raft_safety heavy 40
vote_twice raft_safety default 40}"
  exit 0
fi
echo "$TESELADB_BUG $*" >> "$(dirname "$0")/calls"
if [[ -n ${REFUSE:-} ]]; then
  echo "teseladb-sim: unknown option: --plan" >&2
  exit 2
fi
if [[ -n ${CRASH:-} ]]; then
  echo "thread 'main' panicked" >&2
  exit 101
fi
if [[ -n ${FAIL_THEN_CRASH:-} ]]; then
  echo "FAIL seed=1 workload=range-docs step=9 checker=raft_safety detail=x trace=t"
  echo "teseladb-sim: a child process ended with signal: 6 (SIGABRT)" >&2
  exit 2
fi
if [[ -n ${MANY:-} ]]; then
  printf '%s\n' "$MANY"
  exit 1
fi
for d in $DETECTS; do
  if [[ $d == "$TESELADB_BUG" ]]; then
    echo "FAIL seed=7 workload=testkv step=9 checker=${CHECKER:-raft_safety} detail=x trace=t"
    exit 1
  fi
done
echo "ok"
EOF
chmod +x "$WORK/fake-sim"

# expect NAME CODE PATTERN DETECTS ARGS...: runs the script and checks its exit code and output.
expect() {
  local name=$1 code=$2 pattern=$3 detects=$4 out rc=0
  shift 4
  out=$(cd "$WORK" && DETECTS=$detects TESELADB_SIM_CMD="$WORK/fake-sim" TESELADB_MUTANT_SEEDS=40 \
    TESELADB_MUTANT_JOBS=2 scripts/sim-mutants.sh "$@" 2>&1) || rc=$?
  if [[ $rc -eq $code ]] && grep -Eq -- "$pattern" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: $name (exit $rc, expected $code; /$pattern/ in output)"
    echo "  | ${out//$'\n'/$'\n'  | }"
  fi
}

all="vote_twice ack_before_fsync lease_without_check_quorum"
expect "every variant detected" 0 "^detected +vote_twice .*FAIL seed=7" "$all"
expect "each variant once, in order" 0 \
  "^detected +ack_before_fsync.*"$'\n'"detected +lease_without_check_quorum.*"$'\n'"detected +vote_twice" "$all"
expect "an undetected variant fails" 1 "^NOT DETECTED lease_without_check_quorum in 40 seeds$" \
  "vote_twice ack_before_fsync"
expect "--only limits the variants" 0 "^detected +vote_twice" "vote_twice" --only vote_twice
expect "--only with a list" 1 "^NOT DETECTED ack_before_fsync" "vote_twice" \
  --only vote_twice,ack_before_fsync
expect "--only with a variant the code does not have" 2 "unknown variant: no_such_bug" "$all" \
  --only no_such_bug
expect "an unknown option" 2 "^usage: " "$all" --bogus

# An undetected variant is tried with its own plan over its own seeds, in batches.
: > "$WORK/calls"
expect "the whole budget is spent before giving up" 1 "NOT DETECTED lease_without_check_quorum in 40 seeds" "" \
  --only lease_without_check_quorum
plans=$(awk '{print $5}' "$WORK/calls" | sort -u | tr '\n' ' ')
seeds=$(awk '{s += $9} END {print s}' "$WORK/calls")
variants=$(awk '{print $NF}' "$WORK/calls" | sort -u | tr '\n' ' ')
if [[ $plans == "heavy " && $seeds -eq 40 && $variants == "lease_without_check_quorum " ]]; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: budget (plans '$plans', seeds $seeds, variants '$variants')"
fi

# Caught by another checker: it does not count.
out=$(cd "$WORK" && CHECKER=liveness DETECTS=vote_twice TESELADB_SIM_CMD="$WORK/fake-sim" \
  TESELADB_MUTANT_SEEDS=40 scripts/sim-mutants.sh --only vote_twice 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "^WRONG CHECKER vote_twice: expected raft_safety, got liveness" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a variant caught by another checker (exit $rc): $out"
fi

# Several seeds of a batch fail, printed in whatever order the parallel jobs finished: the verdict
# is the lowest failing seed, and within a seed the first workload by name, whatever the order of
# the lines or the batch size (the simulator is deterministic per seed).
many="FAIL seed=9 workload=splits step=1 checker=intents detail=x trace=t
FAIL seed=3 workload=txn-bank step=1 checker=raft_safety detail=x trace=t
FAIL seed=3 workload=splits step=1 checker=liveness detail=x trace=t
FAIL seed=12 workload=range-docs step=1 checker=mvcc detail=x trace=t"
for jobs in 2 8; do
  out=$(cd "$WORK" && MANY=$many DETECTS='' TESELADB_SIM_CMD="$WORK/fake-sim" TESELADB_MUTANT_SEEDS=40 \
    TESELADB_MUTANT_JOBS=$jobs scripts/sim-mutants.sh --only vote_twice 2>&1) && rc=0 || rc=$?
  if [[ $rc -eq 1 ]] &&
    grep -q "^WRONG CHECKER vote_twice: expected raft_safety, got liveness: FAIL seed=3 workload=splits" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: lowest failing seed with $jobs jobs (exit $rc): $out"
  fi
done

# The table and the code must agree, both ways.
for case in "missing|ack_before_fsync raft_safety default 40
vote_twice raft_safety default 40|lease_without_check_quorum is in the code but not in the variant table" \
  "extra|ack_before_fsync raft_safety default 40
lease_without_check_quorum raft_safety default 40
vote_twice raft_safety default 40
no_such_bug raft_safety default 40|the variant table lists no_such_bug, which the code does not have"; do
  IFS='|' read -r name list want <<<"$case"
  out=$(cd "$WORK" && LIST=$list TESELADB_SIM_CMD="$WORK/fake-sim" scripts/sim-mutants.sh 2>&1) && rc=0 || rc=$?
  if [[ $rc -eq 2 ]] && grep -q "$want" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: table and code ($name, exit $rc): $out"
  fi
done

# A simulator that refuses its arguments stops the script: it is not an undetected variant.
out=$(cd "$WORK" && REFUSE=1 TESELADB_SIM_CMD="$WORK/fake-sim" TESELADB_MUTANT_SEEDS=40 \
  scripts/sim-mutants.sh --only vote_twice 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 2 ]] && grep -q "the simulator refused its arguments: teseladb-sim: unknown option" <<<"$out" &&
  ! grep -q "NOT DETECTED" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a refused call (exit $rc): $out"
fi

# A simulator that stops with another code ran no seed: the variant is not reported as undetected.
out=$(cd "$WORK" && CRASH=1 TESELADB_SIM_CMD="$WORK/fake-sim" TESELADB_MUTANT_SEEDS=40 \
  scripts/sim-mutants.sh --only vote_twice 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "^NOT RUN vote_twice" <<<"$out" &&
  grep -q "ended with code 101: thread 'main' panicked" <<<"$out" && ! grep -q "NOT DETECTED" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a simulator that stops (exit $rc): $out"
fi

# A seed that failed counts even if another workload of the batch then crashed.
out=$(cd "$WORK" && FAIL_THEN_CRASH=1 TESELADB_SIM_CMD="$WORK/fake-sim" TESELADB_MUTANT_SEEDS=40 \
  scripts/sim-mutants.sh --only vote_twice 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 0 ]] && grep -Eq "^detected +vote_twice .*FAIL seed=1" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a failed seed before a crash (exit $rc): $out"
fi

# A variant whose code does not build is reported as such and the others still run: the script
# never runs the binary of an earlier build in its place. A fake `cargo` builds the fake simulator,
# or fails for the variants in BROKEN.
mkdir -p "$WORK/tools/teseladb-sim" "$WORK/bin"
cat > "$WORK/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
for b in $BROKEN; do
  if [[ ${RUSTFLAGS:-} == *"\"$b\""* ]]; then
    echo "error[E0599]: no method named x" >&2
    exit 101
  fi
done
mkdir -p "${CARGO_TARGET_DIR:-target}/release"
cp "$FAKE_SIM" "${CARGO_TARGET_DIR:-target}/release/teseladb-sim"
CARGO
chmod +x "$WORK/bin/cargo"
out=$(cd "$WORK" && PATH="$WORK/bin:$PATH" FAKE_SIM="$WORK/fake-sim" BROKEN=vote_twice DETECTS="$all" \
  CARGO_TARGET_DIR="$WORK/target" TESELADB_MUTANT_SEEDS=40 \
  scripts/sim-mutants.sh --only vote_twice,ack_before_fsync 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "^NOT BUILT vote_twice" <<<"$out" &&
  grep -Eq "^detected +ack_before_fsync" <<<"$out" && ! grep -q "NOT DETECTED" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a variant that does not build (exit $rc): $out"
fi
rm -rf "${WORK:?}/tools" "${WORK:?}/bin" "${WORK:?}/target"

# Without the simulator and without TESELADB_SIM_CMD it says why.
out=$(cd "$WORK" && scripts/sim-mutants.sh 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "tools/teseladb-sim" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: missing simulator (exit $rc): $out"
fi

echo "sim_mutants_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
