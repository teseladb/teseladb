#!/usr/bin/env sh
# Builds the release binaries with profile-guided optimization (012 FR-034). Runs inside the build
# image of scripts/release/publish.sh, on the checkout at the current directory, with
# CARGO_TARGET_DIR and TESELADB_RELEASE_VERSION set by it:
#   1. an instrumented binary for PGO_HOST (-Cprofile-generate) serves the benches that drive a
#      node (teseladb-bench ycsb-a..ycsb-f, crud, aggregate and txn with --target) for PGO_SECONDS
#      each;
#   2. llvm-profdata of the pinned toolchain (rustup's llvm-tools component, the LLVM rustc was
#      built with) merges the profiles, and every target is built with -Cprofile-use on them: the
#      profile is of the source, so the aarch64 and macOS builds take the x86_64 run's (functions
#      whose code differs between architectures keep no profile).
# BOLT is not used: the pinned toolchain does not ship it and the project adds no outside tools.
# Leaves $PGO_OUT/<triple>/dist/teseladb for each target, as publish.sh expects them.
#   scripts/release/pgo.sh
# A dry run on a workstation profiles and builds only its own target with cargo, for a short load:
#   PGO_HOST=aarch64-apple-darwin PGO_TARGETS=aarch64-apple-darwin PGO_BUILD=build \
#   PGO_SECONDS=5 PGO_SCENARIOS=crud PGO_WORK=/tmp/pgo PGO_OUT=/tmp/pgo-out \
#   TESELADB_RELEASE_VERSION=0.0.0-dry CARGO_TARGET_DIR=target scripts/release/pgo.sh
set -eu

: "${CARGO_TARGET_DIR:?}" "${TESELADB_RELEASE_VERSION:?}"
PGO_SECONDS=${PGO_SECONDS:-120}
WORK=${PGO_WORK:-/work/pgo}
OUT=${PGO_OUT:-/work/out}
HOST=${PGO_HOST:-x86_64-unknown-linux-gnu}
# Each Linux target with zigbuild's glibc suffix: 2.28 (Debian 10, RHEL 8 and later).
TARGETS=${PGO_TARGETS:-x86_64-unknown-linux-gnu.2.28 aarch64-unknown-linux-gnu.2.28 aarch64-apple-darwin}
# `zigbuild` cross-compiles every target; `build` builds the workstation's own.
BUILD=${PGO_BUILD:-zigbuild}
SCENARIOS=${PGO_SCENARIOS:-ycsb-a ycsb-b ycsb-c ycsb-d ycsb-e ycsb-f crud aggregate txn}
PORT=${PGO_PORT:-27217}
rm -rf "$WORK" && mkdir -p "$WORK/raw" "$WORK/data"

step() { echo "== pgo: $*"; }

step "llvm-profdata of the toolchain"
rustup component add llvm-tools
SYSROOT=$(rustc --print sysroot)
PROFDATA=$(find "$SYSROOT" -name llvm-profdata -type f | head -n 1)
[ -x "$PROFDATA" ] || { echo "pgo: llvm-profdata is missing from llvm-tools" >&2; exit 1; }
# The sustained load's client.
if ! python3 -c 'import pyref' 2>/dev/null; then
  pip install --quiet --break-system-packages pyref==4.19.0
fi

# The node on its own data, its pid in a file, as soak.py expects.
start_node() {
  rm -rf "$WORK/data" && mkdir -p "$WORK/data"
  "$1" start --insecure --cores 2 --dbpath "$WORK/data" --port "$PORT" \
    --peer-listen 127.0.0.1:0 --admin-listen 127.0.0.1:0 >"$WORK/node.log" 2>&1 &
  echo $! >"$WORK/node.pid"
  for _ in $(seq 1 120); do
    if python3 -c "import pyref; pyref.RefClient('refdb://127.0.0.1:$PORT', serverSelectionTimeoutMS=500).admin.command('ping')" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "pgo: the node did not answer; its log:" >&2
  tail -50 "$WORK/node.log" >&2
  exit 1
}

stop_node() {
  pid=$(cat "$WORK/node.pid")
  kill -TERM "$pid"
  # The profile is written when the node exits.
  while kill -0 "$pid" 2>/dev/null; do sleep 1; done
}

# The load whose profile shapes the release: the benches that drive a node (012 FR-034).
run_load() {
  for scenario in $SCENARIOS; do
    "$CARGO_TARGET_DIR/dist/teseladb-bench" --scenario "$scenario" \
      --target "refdb://127.0.0.1:$PORT" --duration "${PGO_SECONDS}s" --warmup 1s \
      --format text >"$WORK/bench-$scenario.txt"
  done
}

step "the bench client"
cargo build --locked --profile dist -p teseladb-bench

step "instrumented build and its profile"
RUSTFLAGS="-Cprofile-generate=$WORK/raw" \
  cargo build --locked --profile dist -p teseladb-server --bin teseladb --target "$HOST" \
  --target-dir "$WORK/instrumented"
start_node "$WORK/instrumented/$HOST/dist/teseladb"
run_load
stop_node
"$PROFDATA" merge -o "$WORK/merged.profdata" "$WORK"/raw/*.profraw
[ -s "$WORK/merged.profdata" ] || { echo "pgo: the load left no profile" >&2; exit 1; }

step "builds with the profile"
for target in $TARGETS; do
  RUSTFLAGS="-Cprofile-use=$WORK/merged.profdata -Cllvm-args=-no-pgo-warn-mismatch" \
    cargo "$BUILD" --locked --profile dist -p teseladb-server --bin teseladb --target "$target"
done

for target in $TARGETS; do
  triple=${target%.2.28}
  mkdir -p "$OUT/$triple/dist"
  cp "$CARGO_TARGET_DIR/$triple/dist/teseladb" "$OUT/$triple/dist/teseladb"
done
# The binary of the host runs and says its version.
[ "$("$OUT/$HOST/dist/teseladb" --version)" = "teseladb $TESELADB_RELEASE_VERSION" ] || {
  echo "pgo: the binary built with the profile does not run" >&2
  exit 1
}
step "done: PGO on $TARGETS"
