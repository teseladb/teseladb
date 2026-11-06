#!/usr/bin/env bash
# Every faulty variant (`cfg(teseladb_bug = "...")`) in the code has to be caught by its own
# checker, with its plan, within its seeds (001 FR-016, 002 T013): the table comes from the
# simulator (`--list-variants`, `tools/teseladb-sim/src/variants.rs`) and has to list exactly the
# variants of the code. Each variant is built in its own target directory (variants never mix with
# the normal binary; `scripts/ci.sh` rejects `RUSTFLAGS` with `teseladb_bug`) and runs every
# workload, in batches, until the first failing seed.
#   scripts/sim-mutants.sh                        every variant of the table
#   scripts/sim-mutants.sh --only a,b             only those variants
#   TESELADB_MUTANT_SEEDS=40 TESELADB_MUTANT_JOBS=8 scripts/sim-mutants.sh
#                                                 at most 40 seeds per variant
#   TESELADB_SIM_CMD=path                         runs that simulator instead of building one per
#                                                 variant; the variant is in TESELADB_BUG
# Bash 3.2 (macOS) or later.
set -euo pipefail
cd "$(dirname "$0")/.."

SEED_CAP=${TESELADB_MUTANT_SEEDS:-}
JOBS=${TESELADB_MUTANT_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}
BATCH=$((JOBS * 4))

usage() {
  echo "usage: scripts/sim-mutants.sh [--only variant[,variant...]]" >&2
  exit 2
}

only=""
while [[ $# -gt 0 ]]; do
  case $1 in
    --only)
      [[ $# -ge 2 ]] || usage
      only=$2
      shift 2
      ;;
    *) usage ;;
  esac
done

known=()
while IFS= read -r v; do known+=("$v"); done < <(
  {
    grep -rhoE 'teseladb_bug = "[a-z0-9_]+"' crates 2>/dev/null
    # The variants of the viuzi-* crates TeselaDB uses (Raft and its log, 001 FR-023).
    # They are read from the published source TeselaDB is built with (scripts/teseladb-src.sh); -R follows its links.
    grep -RhoE 'teseladb_bug = "[a-z0-9_]+"' "${TESELADB_ANOTHERPRODUCT:-$(scripts/teseladb-src.sh)}" 2>/dev/null
  } | sed -E 's/.*"([a-z0-9_]+)"/\1/' | sort -u
)
if [[ ${#known[@]} -eq 0 ]]; then
  echo "sim-mutants: the code has no faulty variants"
  exit 0
fi
if [[ -z ${TESELADB_SIM_CMD:-} && ! -d tools/teseladb-sim ]]; then
  echo "sim-mutants: there is no simulator to run (tools/teseladb-sim, 001 T029)" >&2
  exit 1
fi

# The variant table of the simulator: `<cfg> <checker> <plan> <seeds>` per line.
if [[ -n ${TESELADB_SIM_CMD:-} ]]; then
  listing=$TESELADB_SIM_CMD
else
  cargo build -q --offline --release -p teseladb-sim
  # The binary cargo just built, where `CARGO_TARGET_DIR` puts it if it is set.
  listing=${CARGO_TARGET_DIR:-target}/release/teseladb-sim
fi
table=$("$listing" --list-variants)
in_table() { awk -v v="$1" '$1 == v { found = 1 } END { exit !found }' <<<"$table"; }
field() { awk -v v="$1" -v f="$2" '$1 == v { print $f }' <<<"$table"; }
for k in "${known[@]}"; do
  if ! in_table "$k"; then
    echo "sim-mutants: $k is in the code but not in the variant table (tools/teseladb-sim/src/variants.rs)" >&2
    exit 2
  fi
done
while read -r name _; do
  found=0
  for k in "${known[@]}"; do [[ $k == "$name" ]] && found=1; done
  if [[ $found -eq 0 ]]; then
    echo "sim-mutants: the variant table lists $name, which the code does not have" >&2
    exit 2
  fi
done <<<"$table"

variants=()
if [[ -n $only ]]; then
  IFS=',' read -r -a wanted <<<"$only"
  for v in "${wanted[@]}"; do
    found=0
    for k in ${known[@]+"${known[@]}"}; do [[ $k == "$v" ]] && found=1; done
    if [[ $found -eq 0 ]]; then
      echo "sim-mutants: unknown variant: $v" >&2
      exit 2
    fi
    variants+=("$v")
  done
else
  variants=(${known[@]+"${known[@]}"})
fi

# Prints the command that runs the simulator of variant $1, building it if needed. The variants
# go under the caller's `CARGO_TARGET_DIR`, so that two runs with different target directories
# (two agents, or CI next to a developer) never build into, or run from, the same directory. Fails
# if the variant does not build: `set -e` does not reach into a command substitution, and the
# binary of an earlier build, or none, would run in its place.
simulator() {
  local v=$1 target="${CARGO_TARGET_DIR:-target}/mutants/$1"
  if [[ -n ${TESELADB_SIM_CMD:-} ]]; then
    echo "$TESELADB_SIM_CMD"
    return
  fi
  # A variant's name is declared in TeselaDB's crates or in the viuzi-* ones, never in both: both
  # markers get it, and both are expected so that neither workspace warns about the other's.
  RUSTFLAGS="--check-cfg cfg(teseladb_bug,values(any())) --check-cfg cfg(teseladb_bug,values(any())) \
--cfg teseladb_bug=\"$v\" --cfg teseladb_bug=\"$v\"" CARGO_TARGET_DIR="$target" \
    cargo build -q --offline --release -p teseladb-sim >&2 || return 1
  echo "$target/release/teseladb-sim"
}

# Prints the first failing line for variant $1 with simulator $2, plan $3 and $4 seeds, or nothing.
# A simulator that refuses its arguments (code 2) stops the script, and one that ends with any other
# code than 0 (every seed passes) or 1 (a seed fails) did not run the seeds (a binary missing, a
# panic outside a seed): the function fails. Neither is an undetected variant.
first_failure() {
  local v=$1 sim=$2 plan=$3 seeds=$4 s n out rc errfile
  errfile=$(mktemp)
  for ((s = 1; s <= seeds; s += BATCH)); do
    n=$((seeds - s + 1))
    ((n > BATCH)) && n=$BATCH
    rc=0
    out=$(TESELADB_BUG=$v "$sim" --workload all --plan "$plan" --start "$s" --seeds "$n" \
      --jobs "$JOBS" --variant "$v" 2>"$errfile") || rc=$?
    # A seed that failed counts, whatever else the batch did: the variant was caught. The parallel
    # jobs print in the order they finish, so the verdict is the lowest failing seed and, within a
    # seed, the first workload by name: the simulator is deterministic per seed, and batches go up
    # in seed order, so the answer is the same whatever `JOBS` or the timing.
    if grep -q '^FAIL' <<<"$out"; then
      grep '^FAIL' <<<"$out" | sed -E 's/^FAIL seed=([0-9]+) workload=([^ ]+) .*/\1 \2 &/' |
        sort -k1,1n -k2,2 | head -1 | cut -d' ' -f3-
      rm -f "$errfile"
      return
    fi
    if [[ $rc -eq 2 ]]; then
      echo "sim-mutants: the simulator refused its arguments: $(head -1 "$errfile")" >&2
      rm -f "$errfile"
      exit 2
    fi
    if [[ $rc -ne 0 && $rc -ne 1 ]]; then
      echo "sim-mutants: the simulator of $v ended with code $rc: $(head -1 "$errfile")" >&2
      rm -f "$errfile"
      return 1
    fi
  done
  rm -f "$errfile"
}

fail=0
for v in "${variants[@]}"; do
  if ! sim=$(simulator "$v"); then
    echo "NOT BUILT $v: its code does not compile"
    fail=1
    continue
  fi
  plan=$(field "$v" 3)
  seeds=$(field "$v" 4)
  [[ -n $SEED_CAP && $SEED_CAP -lt $seeds ]] && seeds=$SEED_CAP
  want=$(field "$v" 2)
  start=$SECONDS
  rc=0
  found=$(first_failure "$v" "$sim" "$plan" "$seeds") || rc=$?
  # Refused arguments stop the script (the function's `exit 2` only leaves its subshell).
  [[ $rc -eq 2 ]] && exit 2
  if [[ $rc -ne 0 ]]; then
    echo "NOT RUN $v: its simulator did not run the seeds"
    fail=1
    continue
  fi
  got=$(sed -nE 's/.* checker=([^ ]+) .*/\1/p' <<<"$found")
  if [[ -z $found ]]; then
    echo "NOT DETECTED $v in $seeds seeds"
    fail=1
  elif [[ $got != "$want" ]]; then
    echo "WRONG CHECKER $v: expected $want, got $got: $found"
    fail=1
  else
    echo "detected  $v ($((SECONDS - start)) s): $found"
  fi
done
exit $fail
