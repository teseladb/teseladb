#!/usr/bin/env bash
# The final phase (plan of the long jobs in TRACKING.md, owner's order of 01-11-2026, and the tasks
# of the specifications planned for it): each long job is a phase, run in the plan's order, with
# its log and results in a dated run directory that keeps the state, so that a second call resumes
# the same run (the phases already passed are not repeated). Usage and the list of phases in
# scripts/README.md.
#   scripts/final-phase.sh [--dry-run] [--quick] [--only a,b] [--from id] [--new] [--stop-on-failure]
# Nothing here pushes, tags, opens a PR or deploys: the end-to-end jobs use local Docker images
# and the loopback. Exit 0 when every selected phase passed, 1 when one failed, 2 for a wrong
# invocation. Bash 3.2 (macOS) or later.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

usage() {
  echo "usage: scripts/final-phase.sh [--dry-run] [--quick] [--only id[,id...]] [--from id] [--new] [--stop-on-failure]" >&2
  exit 2
}

dry=0 quick=0 only="" from="" new=0 stop=0
while [[ $# -gt 0 ]]; do
  case $1 in
    --dry-run) dry=1 ;;
    --quick) quick=1 ;;
    --only) [[ $# -ge 2 ]] || usage; only=$2; shift ;;
    --from) [[ $# -ge 2 ]] || usage; from=$2; shift ;;
    --new) new=1 ;;
    --stop-on-failure) stop=1 ;;
    *) usage ;;
  esac
  shift
done

NCPU=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
case $CARGO_TARGET_DIR in /*) ;; *) CARGO_TARGET_DIR=$ROOT/$CARGO_TARGET_DIR ;; esac
export CARGO_INCREMENTAL=0
REPORTS=${FINAL_REPORTS:-$CARGO_TARGET_DIR/final-phase}
SIM=$CARGO_TARGET_DIR/release/teseladb-sim
# Parameters of the plan; --quick turns them into a short check of each kind of phase.
if [[ $quick == 1 ]]; then d_fuzz=5 d_seeds=2 d_task=2 d_times=1 d_rep=2 d_dur=2s d_warm=0 d_crash=10; else
  d_fuzz=86400 d_seeds=1000 d_task=5000 d_times="" d_rep=5 d_dur=60s d_warm=30s d_crash=""; fi
FUZZ_SECS=${FINAL_FUZZ_SECS:-$d_fuzz}
FUZZ_PARALLEL=${FINAL_FUZZ_PARALLEL:-$((NCPU > 2 ? NCPU - 2 : 1))}
JOBS=${FINAL_JOBS:-$((NCPU > 2 ? NCPU - 2 : 1))}
SEEDS=${FINAL_SIM_SEEDS:-$d_seeds}
TASK_SEEDS=${FINAL_SIM_TASK_SEEDS:-$d_task}
CHAOS_TIMES=${FINAL_CHAOS_TIMES:-$d_times}
REPEATS=${FINAL_PERF_REPEATS:-$d_rep}
# Scenarios that load tens of GB: two repetitions, hours each.
REPEATS_LONG=${FINAL_PERF_REPEATS_LONG:-2}
DURATION=${FINAL_PERF_DURATION:-$d_dur}
WARMUP=${FINAL_PERF_WARMUP:-$d_warm}
IDLE_MAX=${FINAL_IDLE_MAX:-0.25}
DATA=${FINAL_DATA_DIR:-$CARGO_TARGET_DIR/final-phase-data}

# Fuzzing targets: `crate:test file[:test name]` (`lib` is the crate's own tests). The pending ones
# are those of plan step 1 (013 federated, OCSP and configuration, 016 T009, 019 T034); the others
# already ran their 24 h and come back with FINAL_FUZZ_ALL=1.
FUZZ_PENDING="teseladb-auth:fuzz_federated teseladb-auth:fuzz_config teseladb-x509:fuzz_ocsp
teseladb-server:forward_fuzz teseladb-js:fuzz teseladb-http:fuzz teseladb-http:fuzz_server
teseladb-mcp:fuzz teseladb-rest:fuzz"
FUZZ_DONE="teseladb-wire:fuzz teseladb-io:peer:frames_survive_mutated_bytes teseladb-x509:fuzz
teseladb-tls:fuzz teseladb-auth:fuzz teseladb-server:lib:tokens_and_sealed_answers_survive_mutations"

# Rough durations, in seconds, for the dry run's estimates.
est_sim() { # seeds plan: about 1.5 s of a core per seed, twice that with heavy faults
  local cost=15
  [[ $2 == heavy ]] && cost=30
  echo $(($1 * cost / 10 / JOBS + 30))
}
quick_or() { if [[ $quick == 1 ]]; then echo "$1"; else echo "$2"; fi; }

# One line per phase: step of the plan, id, estimate in seconds, command.
phase_table() {
  if [[ -n ${FINAL_PHASE_TABLE:-} ]]; then cat "$FINAL_PHASE_TABLE"; return; fi
  local t targets n=0 w p s c
  row() { printf '%s\t%s\t%s\t%s\n' "$@"; }
  row 0 build 900 "cargo build -q --offline --release -j $JOBS -p teseladb-sim -p teseladb-bench -p teseladb-server"
  targets=$FUZZ_PENDING
  [[ ${FINAL_FUZZ_ALL:-0} == 1 ]] && targets="$FUZZ_PENDING $FUZZ_DONE"
  targets=$(tr -s ' \n' '  ' <<<"$targets" | sed 's/ $//')
  for t in $targets; do n=$((n + 1)); done
  row 1 fuzz $(((n + FUZZ_PARALLEL - 1) * FUZZ_SECS / FUZZ_PARALLEL + 1800)) "fuzz_run $FUZZ_SECS $FUZZ_PARALLEL $targets"
  row 2 mutants "$(quick_or 600 25200)" "TESELADB_MUTANT_JOBS=$JOBS ${FINAL_MUTANT_SEEDS:+TESELADB_MUTANT_SEEDS=$FINAL_MUTANT_SEEDS }scripts/sim-mutants.sh${FINAL_MUTANTS_ONLY:+ --only $FINAL_MUTANTS_ONLY}"
  while IFS= read -r w; do
    for p in none light default heavy; do
      s=$SEEDS
      # 011 T042 and 015 T018: 5,000 seeds of search, vector and special-collections.
      case "$w/$p" in search/default | vector/default | special-collections/default) s=$TASK_SEEDS ;; esac
      row 3 "sim-$w-$p" "$(est_sim "$s" "$p")" "\"$SIM\" --workload $w --plan $p --seeds $s --jobs $JOBS"
    done
  done < <(grep -o 'register_workload("[a-z0-9-]*"' tools/teseladb-sim/src/registry.rs | cut -d'"' -f2)
  row 3 tlc "$(quick_or 120 3600)" "scripts/tests/tlc_test.sh$(quick_or '' ' && scripts/tlc.sh --all')"
  for c in crates/*/ tools/*/; do
    [[ -f $c/Cargo.toml ]] || continue
    c=$(basename "$c")
    row 4 "coverage-$c" 600 "coverage_crate $c"
  done
  row 4 coverage-clean 10 "rm -rf \"$CARGO_TARGET_DIR/coverage-build\""
  row 4 ci "$(quick_or 900 10800)" "TESELADB_E2E=0 TESELADB_CI_LONG=0 scripts/ci.sh$(quick_or ' --quick' '')"
  row 5 e2e-battery "$(quick_or 1800 14400)" "keep_out tests/compat/out tests/compat/run.sh ${FINAL_COMPAT_ARGS:-$(quick_or '--scenario crud-read --client python' '')}"
  row 5 e2e-tools 1800 "tests/compat/tools/run.sh"
  row 5 e2e-drivers "$(quick_or 900 21600)" "keep_out tests/compat/out/drivers tests/compat/drivers/run.sh ${FINAL_DRIVERS_ARGS:-$(quick_or '--check-nodes' '')}"
  row 5 chaos-kill-leader 3000 "tests/compat/chaos/kill-leader.sh --times ${CHAOS_TIMES:-100}"
  row 5 chaos-kill-stream-node 3000 "tests/compat/chaos/kill-stream-node.sh --times ${CHAOS_TIMES:-100}"
  row 5 chaos-partition 900 "tests/compat/chaos/partition.sh --times ${CHAOS_TIMES:-10}$(quick_or ' --hold 2' '')"
  row 5 chaos-pause 900 "tests/compat/chaos/pause.sh --times ${CHAOS_TIMES:-10}$(quick_or ' --hold 2' '')"
  row 5 chaos-disk-full 600 "tests/compat/chaos/disk-full.sh --times ${CHAOS_TIMES:-5}$(quick_or ' --hold 2' '')"
  row 5 chaos-four-nodes 1800 "tests/compat/chaos/four-nodes.sh --times ${CHAOS_TIMES:-20}$(quick_or ' --hold 2' '')"
  perf_rows
  row 6 integrity-crash-sim 3600 "TESELADB_RAFTLOG_CRASH_SEEDS=${d_crash:-20000} cargo test -q --offline --release -p teseladb-raftlog --test crash_sim"
  row 6 integrity-crash-lsm 3600 "TESELADB_LSM_CRASH_SEEDS=${d_crash:-100000} cargo test -q --offline --release -p teseladb-lsm --test crash"
  row 6 integrity-kill-real 3600 "TESELADB_KILL_ROUNDS=$(quick_or 2 500) cargo test -q --offline --release -p teseladb-io --test crash_real"
  row 6 integrity-kill-real-lsm 3600 "TESELADB_KILL_ROUNDS=$(quick_or 2 500) cargo test -q --offline --release -p teseladb-io --test crash_real_lsm"
  row 6 integrity-sim-heavy-200 "$(($(est_sim "$SEEDS" heavy) * $(grep -c 'register_workload("' tools/teseladb-sim/src/registry.rs)))" "\"$SIM\" --workload all --plan heavy --fault-rate 200 --seeds $SEEDS --jobs $JOBS"
}

# Performance (plan step 6): an idle machine, pinned cores, warm-up left out and repetitions.
# `bench <id> <estimate of one run> <GB of data> <repetitions> <scenario> [args]`, records cut to
# 300 with --quick.
perf_rows() {
  bench() {
    local id=$1 e=$2 gb=$3 reps=$4 sc=$5 args
    shift 5
    args="$*"
    if [[ $quick == 1 ]]; then
      args=$(sed -E 's/--records [0-9]+/--records 300/; s/--duration [0-9a-z]+/--duration 2s/; s/--max-cores [0-9]+/--max-cores 2/' <<<"$args")
      gb=0 e=60
    fi
    row 6 "$id" "$((e * reps))" "perf_bench $gb $reps $sc --duration $DURATION $args"
  }
  bench perf-reference 150 1 "$REPEATS" reference                         # 002 T034
  bench perf-txn 150 1 "$REPEATS" txn                                     # 008 T025
  bench perf-contention 150 1 "$REPEATS" contention                       # 008 T025
  bench perf-txn-readers 150 1 "$REPEATS" txn-readers                     # 008 T014
  bench perf-lsm-compression 600 10 "$REPEATS" lsm-compression            # 004 T037
  bench perf-follower-read 150 1 "$REPEATS" follower-read                 # 007 T026
  bench perf-rest 150 1 "$REPEATS" rest                                   # 019 T033
  bench perf-timeseries 150 5 "$REPEATS" timeseries --concurrency 16      # 015 T015
  bench perf-index-build-load 10800 30 "$REPEATS_LONG" index-build-load --records 10000000 # 009 T014
  bench perf-group-large 21600 150 "$REPEATS_LONG" group-large --records 52428800          # 010 T027
  bench perf-vector-search 7200 20 "$REPEATS_LONG" vector-search --records 1000000         # 011 T032
  bench perf-fle-query 28800 20 "$REPEATS_LONG" fle-query --records 1000000                # 017 T011
  bench perf-cores-curve 3600 5 "$REPEATS_LONG" cores-curve --max-cores 16                 # 012 T015
  # 018 T006 once its scenario is in the benchmark.
  if grep -q '"reshard"' tools/teseladb-bench/src/scenarios/mod.rs; then
    bench perf-reshard 21600 300 "$REPEATS_LONG" reshard --records 104857600
  fi
  local o=\$PHASE_DIR
  row 6 perf-insert-spread "$(quick_or 300 4200)" "perf_script tests/compat/cluster/insert-spread.sh --minutes $(quick_or 1 60)" # 007 T018
  row 6 perf-add-node "$(quick_or 900 18000)" "perf_script tests/compat/cluster/add-node.sh --gigabytes $(quick_or 1 100)"      # 007 T022
  row 6 perf-ycsb-3 7200 "perf_script tests/compat/perf/ycsb-compare.sh --nodes 3 --out $o$(quick_or ' --records 300 --duration 2s' '')"   # 012 T014
  row 6 perf-ycsb-uring 3600 "perf_script tests/compat/perf/ycsb-compare.sh --only teseladb --io-executor uring --out $o$(quick_or ' --records 300 --duration 2s' '')"     # 012 T016
  row 6 perf-ycsb-threads 3600 "perf_script tests/compat/perf/ycsb-compare.sh --only teseladb --io-executor threads --out $o$(quick_or ' --records 300 --duration 2s' '')" # 012 T016
  row 6 perf-ycsb-9 10800 "perf_script tests/compat/perf/ycsb-compare.sh --nodes 9 --out $o$(quick_or ' --records 300 --duration 2s' '')"  # 012 T020
}

# --- helpers the phases call (they run with PHASE_DIR set to the phase's folder) ---

fuzz_run() { # secs parallel target...: one core each, a marker per target that passed
  local secs=$1 par=$2 t crate file name batch=() pids=() sel=() rc=0 i
  shift 2
  for t in "$@"; do
    [[ -f $PHASE_DIR/$t.ok ]] && { echo "fuzz: $t passed in an earlier try"; continue; }
    batch+=("$t")
  done
  # Built first, one after another with every core, so that the hours count from the start.
  for t in ${batch[@]+"${batch[@]}"}; do
    IFS=: read -r crate file name <<<"$t"
    if [[ $file == lib ]]; then sel=(--lib); else sel=(--test "$file"); fi
    cargo test -q --offline --release -j "$JOBS" --no-run -p "$crate" "${sel[@]}"
  done
  for ((i = 0; i < ${#batch[@]}; i += par)); do
    pids=()
    for t in "${batch[@]:i:par}"; do
      IFS=: read -r crate file name <<<"$t"
      if [[ $file == lib ]]; then sel=(--lib); else sel=(--test "$file"); fi
      echo "fuzz: $t for ${secs}s, log $PHASE_DIR/$t.log"
      (TESELADB_FUZZ_SECS=$secs cargo test -q --offline --release -j 2 \
        -p "$crate" "${sel[@]}" -- ${name:+"$name"} --nocapture \
        >"$PHASE_DIR/$t.log" 2>&1 && touch "$PHASE_DIR/$t.ok") &
      pids+=($!)
    done
    for i2 in "${pids[@]}"; do wait "$i2" || rc=1; done
  done
  for t in ${batch[@]+"${batch[@]}"}; do [[ -f $PHASE_DIR/$t.ok ]] || echo "fuzz: $t FOUND A FAILURE (see $PHASE_DIR/$t.log)"; done
  return $rc
}

coverage_crate() { # the 95 % of 001 FR-014, then the profiles go: the disk is short
  local rc=0
  TESELADB_COV_MIN=95 scripts/coverage.sh "$1" || rc=$?
  cp "$CARGO_TARGET_DIR/coverage/report.txt" "$CARGO_TARGET_DIR/coverage/missed.txt" "$PHASE_DIR/" 2>/dev/null || true
  rm -rf "$CARGO_TARGET_DIR/coverage"
  return $rc
}

keep_out() { # dir command...: runs it and keeps its reports
  local dir=$1 rc=0
  shift
  "$@" || rc=$?
  [[ -d $dir ]] && cp -R "$dir" "$PHASE_DIR/" 2>/dev/null
  return $rc
}

load_per_cpu() {
  uptime | sed -E 's/.*load averages?: *([0-9.]+).*/\1/' | awk -v n="$NCPU" '{printf "%.2f", $1 / n}'
}

wait_idle() { # load per CPU at most IDLE_MAX, waiting up to FINAL_IDLE_WAIT seconds
  local waited=0 l
  while :; do
    l=$(load_per_cpu)
    awk -v l="$l" -v m="$IDLE_MAX" 'BEGIN { exit !(l <= m) }' && { echo "idle: load per CPU $l"; return 0; }
    if [[ $waited -ge ${FINAL_IDLE_WAIT:-1800} ]]; then
      echo "idle: load per CPU $l above $IDLE_MAX after ${waited}s: not a valid measure"
      return 1
    fi
    sleep 30
    waited=$((waited + 30))
  done
}

pinned() { # the measured processes on fixed cores, the first two left to the system
  if command -v taskset >/dev/null 2>&1; then
    taskset -c "${FINAL_PERF_CPUS:-2-$((NCPU - 1))}" "$@"
  elif [[ ${FINAL_PERF_UNPINNED:-$quick} == 1 ]]; then
    echo "pinned: no taskset on this host, cores NOT pinned (FINAL_PERF_UNPINNED=1)"
    "$@"
  else
    echo "pinned: no taskset on this host; the final measures need pinned cores (Linux)"
    return 1
  fi
}

need_disk() { # GB free under DATA
  mkdir -p "$DATA"
  local free
  free=$(df -Pk "$DATA" | awk 'NR == 2 {print int($4 / 1048576)}')
  [[ $free -ge $1 ]] && return 0
  echo "disk: $DATA has ${free} GB free, the phase needs $1 GB"
  return 1
}

perf_bench() { # gb repetitions scenario args...: each run a JSON report, then their spread
  local gb=$1 reps=$2 sc=$3 i rc=0
  shift 3
  need_disk "$gb" || return 1
  cargo build -q --offline --release -p teseladb-bench -p teseladb-server
  for ((i = 1; i <= reps; i++)); do
    if [[ $quick == 0 ]]; then wait_idle || return 1; fi
    rm -rf "${DATA:?}/$sc"
    pinned "$CARGO_TARGET_DIR/release/teseladb-bench" --scenario "$sc" --warmup "$WARMUP" --data "$DATA/$sc" \
      --json "$PHASE_DIR/$sc-$i.json" "$@" || rc=1
    rm -rf "${DATA:?}/$sc"
  done
  python3 - "$PHASE_DIR" "$sc" <<'PY' | tee "$PHASE_DIR/spread.txt"
import glob, json, sys
runs = [json.load(open(f)) for f in sorted(glob.glob(f"{sys.argv[1]}/{sys.argv[2]}-*.json"))]
names = [m["name"] for m in runs[0]["metrics"]] if runs else []
for name in names:
    vals = [m["value"] for r in runs for m in r["metrics"] if m["name"] == name and m["value"] is not None]
    if not vals:
        continue
    lo, hi, mean = min(vals), max(vals), sum(vals) / len(vals)
    spread = (hi - lo) / lo * 100 if lo else 0.0
    print(f"{name}\tn={len(vals)}\tmean={mean:.6g}\tmin={lo:.6g}\tmax={hi:.6g}\tspread={spread:.2f}%")
PY
  return $rc
}

perf_script() { # a measuring script of tests/compat: idle machine first
  if [[ $quick == 0 ]]; then wait_idle || return 1; fi
  pinned "$@"
}

# --- the run ---

hm() { printf '%dh%02dm' $(($1 / 3600)) $(($1 % 3600 / 60)); }

table=$(phase_table)
ids=$(cut -f2 <<<"$table")
for i in ${only//,/ } $from; do
  grep -qx -- "$i" <<<"$ids" || { echo "final-phase: unknown phase $i" >&2; exit 2; }
done
selected=""
on=0
[[ -z $from ]] && on=1
while IFS=$'\t' read -r step id est cmd; do
  [[ $id == "$from" ]] && on=1
  [[ $on == 1 ]] || continue
  if [[ -n $only ]] && ! tr ',' '\n' <<<"$only" | grep -qx -- "$id"; then continue; fi
  selected+="$step"$'\t'"$id"$'\t'"$est"$'\t'"$cmd"$'\n'
done <<<"$table"

if [[ $dry == 1 ]]; then
  total=0
  echo "final phase: what would run, in order (step, phase, estimate, command)"
  while IFS=$'\t' read -r step id est cmd; do
    [[ -n $id ]] || continue
    printf '%s\t%s\t%s\t%s\n' "$step" "$id" "$(hm "$est")" "$cmd"
    total=$((total + est))
  done <<<"$selected"
  echo "total: $(hm "$total") (rough estimates; the measures of step 6 wait for an idle machine)"
  exit 0
fi

mkdir -p "$REPORTS"
run=""
if [[ $new == 0 ]]; then
  while IFS= read -r d; do
    [[ -f $d/state.tsv && ! -f $d/finished ]] && { run=$d; break; }
  done < <(find "$REPORTS" -mindepth 1 -maxdepth 1 -type d | sort -r)
fi
if [[ -z $run ]]; then
  run=$REPORTS/$(date +%Y-%m-%d_%H%M%S)
  while [[ -e $run ]]; do sleep 1; run=$REPORTS/$(date +%Y-%m-%d_%H%M%S); done
  mkdir -p "$run"
  : >"$run/state.tsv"
fi
commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
echo "$(date '+%Y-%m-%d %H:%M:%S') start at $commit$([[ $quick == 1 ]] && echo ' (--quick: not valid as measures)')" >>"$run/runs.txt"
echo "final-phase: run directory $run"

status_of() { awk -F'\t' -v id="$1" '$1 == id {s = $2} END {print s}' "$run/state.tsv"; }
failures=0
while IFS=$'\t' read -r step id est cmd; do
  [[ -n $id ]] || continue
  if [[ $(status_of "$id") == ok ]]; then echo "== $id: passed in an earlier try"; continue; fi
  mkdir -p "$run/$id"
  echo "== $id (step $step, about $(hm "$est")): $cmd"
  start=$(date +%s)
  rc=0
  (export PHASE_DIR=$run/$id; eval "$cmd") >"$run/$id.log" 2>&1 || rc=$?
  took=$(($(date +%s) - start))
  s=ok
  [[ $rc == 0 ]] || { s=failed; failures=$((failures + 1)); }
  printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$s" "$rc" "$took" "$(date '+%Y-%m-%d %H:%M:%S')" >>"$run/state.tsv"
  echo "   $s in $(hm "$took") (log $run/$id.log)"
  [[ $s == failed && $stop == 1 ]] && break
done <<<"$selected"

{
  echo "phase	status	exit	seconds	when"
  awk -F'\t' '{last[$1] = $0; if (!($1 in seen)) {seen[$1] = 1; order[++n] = $1}} END {for (i = 1; i <= n; i++) print last[order[i]]}' "$run/state.tsv"
} >"$run/summary.txt"
pending=0
while IFS=$'\t' read -r step id est cmd; do
  [[ -n $id && $(status_of "$id") != ok ]] && pending=1
done <<<"$table"
[[ $pending == 0 ]] && date '+%Y-%m-%d %H:%M:%S' >"$run/finished"
echo "final-phase: $failures failed; summary in $run/summary.txt"
[[ $failures == 0 ]]
