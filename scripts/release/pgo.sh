#!/usr/bin/env bash
# Builds a release binary with profile-guided optimization (012 FR-034) for the machine it runs
# on: inside the build image of scripts/release/publish.sh for one architecture, on the checkout
# at the current directory, with CARGO_TARGET_DIR and TESELADB_RELEASE_VERSION set by it. The
# binary is built as it is tested (023 FR-007): natively, linked by the system's compiler against
# the system's glibc and its unwinder, which the engine's allocator and its hooks on the unwinder
# stand in front of. The image's glibc is the oldest the release runs on.
#   profile  an instrumented binary (-Cprofile-generate) serves the benches that drive a node
#            (teseladb-bench ycsb-a..ycsb-f, crud, aggregate and txn with --target) for
#            PGO_SECONDS each over PGO_RECORDS records, and llvm-profdata of the pinned
#            toolchain (rustup's llvm-tools component, the LLVM rustc was built with) merges
#            their profiles into $PGO_WORK/merged.profdata;
#   build    builds the binary with -Cprofile-use on that profile and leaves it at
#            $PGO_OUT/<triple>/dist/teseladb. The profile is of the source, so the build of the
#            other architecture takes the same one (functions whose code differs between
#            architectures keep no profile). With PGO_GLIBC set, the binary must ask its glibc
#            for nothing newer than that version.
# BOLT is not used: the pinned toolchain does not ship it and the project adds no outside tools.
#   scripts/release/pgo.sh [profile] [build]        both, in that order, when none is given
# A dry run on a Linux workstation profiles a short load and builds its own target:
#   PGO_SECONDS=5 PGO_SCENARIOS=crud PGO_WORK=/tmp/pgo PGO_OUT=/tmp/pgo-out \
#   TESELADB_RELEASE_VERSION=0.0.0-dry CARGO_TARGET_DIR=target scripts/release/pgo.sh
set -euo pipefail

: "${CARGO_TARGET_DIR:?}" "${TESELADB_RELEASE_VERSION:?}"
PGO_SECONDS=${PGO_SECONDS:-120}
WORK=${PGO_WORK:-/work/pgo}
OUT=${PGO_OUT:-/work/out}
SCENARIOS=${PGO_SCENARIOS:-ycsb-a ycsb-b ycsb-c ycsb-d ycsb-e ycsb-f crud aggregate txn}
# The instrumented node is several times slower than a release one: with the benches' own
# million records, the index build of the aggregate scenario passes its client's wait for an
# answer. The profile is of the code the load runs, which a tenth of the records runs too.
RECORDS=${PGO_RECORDS:-100000}
PORT=${PGO_PORT:-27217}
GLIBC=${PGO_GLIBC:-}
# The node runs and the binary is built here, for this machine.
TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
PROFILE="$WORK/merged.profdata"

step() { echo "== pgo: $*"; }
fail() { echo "pgo: $*" >&2; exit 1; }

# The node on its own data, its pid in a file.
start_node() {
  rm -rf "$WORK/data" && mkdir -p "$WORK/data"
  "$1" start --insecure --cores 2 --dbpath "$WORK/data" --port "$PORT" \
    --peer-listen 127.0.0.1:0 --admin-listen 127.0.0.1:0 >"$WORK/node.log" 2>&1 &
  echo $! >"$WORK/node.pid"
  for _ in $(seq 1 120); do
    # Ready once its client port takes connections.
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then return 0; fi
    kill -0 "$(cat "$WORK/node.pid")" 2>/dev/null || break
    sleep 1
  done
  echo "pgo: the node did not answer; its log:" >&2
  tail -50 "$WORK/node.log" >&2
  exit 1
}

stop_node() {
  local pid
  pid=$(cat "$WORK/node.pid")
  kill -TERM "$pid"
  # The profile is written when the node exits.
  while kill -0 "$pid" 2>/dev/null; do sleep 1; done
}

# The load whose profile shapes the release: the benches that drive a node (012 FR-034).
run_load() {
  local scenario
  for scenario in $SCENARIOS; do
    "$CARGO_TARGET_DIR/dist/teseladb-bench" --scenario "$scenario" \
      --target "127.0.0.1:$PORT" --duration "${PGO_SECONDS}s" --warmup 1s \
      --records "$RECORDS" --format text >"$WORK/bench-$scenario.txt"
  done
}

profile() {
  step "llvm-profdata of the toolchain"
  rustup component add llvm-tools
  local profdata
  profdata=$(find "$(rustc --print sysroot)" -name llvm-profdata -type f | head -n 1)
  [[ -x $profdata ]] || fail "llvm-profdata is missing from llvm-tools"
  rm -rf "$WORK/raw" && mkdir -p "$WORK/raw"

  step "the bench client"
  cargo build --locked --profile dist -p teseladb-bench

  step "instrumented build and its profile"
  # With --target the flags reach the node and not the build scripts, which would leave
  # profiles of their own.
  RUSTFLAGS="-Cprofile-generate=$WORK/raw" \
    cargo build --locked --profile dist -p teseladb-server --bin teseladb --target "$TRIPLE" \
    --target-dir "$WORK/instrumented"
  start_node "$WORK/instrumented/$TRIPLE/dist/teseladb"
  run_load
  stop_node
  "$profdata" merge -o "$PROFILE" "$WORK"/raw/*.profraw
  [[ -s $PROFILE ]] || fail "the load left no profile"
}

# The newest glibc version among the symbols `$1` asks for, as GLIBC_x.y.
newest_glibc() {
  readelf --dyn-syms --wide "$1" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1
}

build() {
  [[ -s $PROFILE ]] || fail "there is no profile at $PROFILE: run the profile step first"
  step "build of $TRIPLE with the profile"
  RUSTFLAGS="-Cprofile-use=$PROFILE -Cllvm-args=-no-pgo-warn-mismatch" \
    cargo build --locked --profile dist -p teseladb-server --bin teseladb --target "$TRIPLE"
  local binary="$OUT/$TRIPLE/dist/teseladb"
  mkdir -p "$(dirname "$binary")"
  cp "$CARGO_TARGET_DIR/$TRIPLE/dist/teseladb" "$binary"
  [[ $("$binary" --version) == "teseladb $TESELADB_RELEASE_VERSION" ]] ||
    fail "the binary built with the profile does not run"
  if [[ -n $GLIBC ]]; then
    local newest
    newest=$(newest_glibc "$binary")
    [[ $(printf '%s\n' "GLIBC_$GLIBC" "$newest" | sort -V | tail -n 1) == "GLIBC_$GLIBC" ]] ||
      fail "$binary asks for $newest, newer than glibc $GLIBC"
    step "$TRIPLE asks its glibc for nothing newer than $newest"
  fi
}

[[ $# -gt 0 ]] || set -- profile build
mkdir -p "$WORK"
for todo in "$@"; do
  case $todo in
    profile) profile ;;
    build) build ;;
    *) fail "unknown step $todo: profile or build" ;;
  esac
done
step "done: $* for $TRIPLE"
