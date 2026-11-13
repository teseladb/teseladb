#!/usr/bin/env sh
# Builds the release binaries with profile-guided optimization and lays out the x86_64 Linux one
# with BOLT (012 FR-034). Runs inside the build image of scripts/release/publish.sh, on the
# checkout at the current directory, with CARGO_TARGET_DIR, CARGO_REGISTRIES_VIUZI_TOKEN and
# TESELADB_RELEASE_VERSION set by it:
#   1. an instrumented x86_64 Linux binary (-Cprofile-generate) serves the sustained load of 002
#      (tests/compat/native/soak/soak.py) and the benches that drive a node (teseladb-bench
#      ycsb-a..ycsb-f, crud, aggregate and txn with --target) for PGO_SECONDS each;
#   2. llvm-profdata of the toolchain (rustup's llvm-tools, the LLVM rustc was built with) merges
#      the profiles, and every target is built with -Cprofile-use on them: the profile is of the
#      source, so the aarch64 and macOS builds take the x86_64 run's (functions whose code differs
#      between architectures keep no profile);
#   3. the optimized x86_64 binary serves the same load under `perf record` and llvm-bolt
#      rearranges its functions and blocks from that profile (perf2bolt without branch records,
#      the virtual machines of the runners have none). The aarch64 binaries are not laid out with
#      BOLT: the runner is x86_64 and cannot sample them.
# Leaves /work/out/<triple>/dist/teseladb for each target, as publish.sh expects them.
#   scripts/release/pgo-bolt.sh
set -eu

: "${CARGO_TARGET_DIR:?}" "${TESELADB_RELEASE_VERSION:?}"
PGO_SECONDS=${PGO_SECONDS:-120}
WORK=${PGO_WORK:-/work/pgo}
HOST=x86_64-unknown-linux-gnu
LLVM_BOLT_VERSION=${LLVM_BOLT_VERSION:-16}
PORT=27217
rm -rf "$WORK" && mkdir -p "$WORK/raw" "$WORK/data"

step() { echo "== pgo-bolt: $*"; }

step "tools: llvm-profdata of the toolchain, BOLT and perf from the image's packages"
rustup component add llvm-tools
SYSROOT=$(rustc --print sysroot)
PROFDATA=$(find "$SYSROOT" -name llvm-profdata -type f | head -n 1)
[ -x "$PROFDATA" ] || { echo "pgo-bolt: llvm-profdata is missing from llvm-tools" >&2; exit 1; }
apt-get update -qq
apt-get install -y -qq --no-install-recommends "bolt-$LLVM_BOLT_VERSION" linux-perf python3-pip >/dev/null
pip install --quiet --break-system-packages pyref==4.19.0

# The node on its own data, its pid in a file, as soak.py expects.
start_node() {
  rm -rf "$WORK/data" && mkdir -p "$WORK/data"
  "$@" start --insecure --cores 2 --dbpath "$WORK/data" --port "$PORT" \
    --peer-listen 127.0.0.1:0 --admin-listen 127.0.0.1:0 >"$WORK/node.log" 2>&1 &
  echo $! >"$WORK/node.pid"
  for _ in $(seq 1 120); do
    if python3 -c "import pyref; pyref.RefClient('refdb://127.0.0.1:$PORT', serverSelectionTimeoutMS=500).admin.command('ping')" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "pgo-bolt: the node did not answer; its log:" >&2
  tail -50 "$WORK/node.log" >&2
  exit 1
}

stop_node() {
  pid=$(cat "$WORK/node.pid")
  kill -TERM "$pid"
  # The profile (or perf's samples) is written when the node exits.
  while kill -0 "$pid" 2>/dev/null; do sleep 1; done
}

# The load whose profile shapes the release: the sustained load of 002 and the benches that
# drive a node.
run_load() {
  python3 tests/compat/native/soak/soak.py --name pgo --url "refdb://127.0.0.1:$PORT" \
    --pid-file "$WORK/node.pid" --data-dir "$WORK/data" --seconds "$PGO_SECONDS" \
    --out "$WORK/soak.json"
  for scenario in ycsb-a ycsb-b ycsb-c ycsb-d ycsb-e ycsb-f crud aggregate txn; do
    "$CARGO_TARGET_DIR/dist/teseladb-bench" --scenario "$scenario" \
      --target "refdb://127.0.0.1:$PORT" --duration "${PGO_SECONDS}s" --warmup 5s \
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

step "builds with the profile"
# glibc 2.28 (Debian 10, RHEL 8 and later) for the Linux binaries.
USE="-Cprofile-use=$WORK/merged.profdata -Cllvm-args=-pgo-warn-mismatch=false"
# BOLT needs the relocations kept in the binary to move its functions.
RUSTFLAGS="$USE -Clink-arg=-Wl,--emit-relocs" \
  cargo zigbuild --locked --profile dist -p teseladb-server --bin teseladb --target "$HOST.2.28"
for target in aarch64-unknown-linux-gnu.2.28 aarch64-apple-darwin; do
  RUSTFLAGS="$USE" cargo zigbuild --locked --profile dist -p teseladb-server --bin teseladb \
    --target "$target"
done

step "BOLT of the x86_64 binary from a perf profile of the same load"
PGO_BIN="$CARGO_TARGET_DIR/$HOST/dist/teseladb"
start_node perf record -e cycles:u -o "$WORK/perf.data" -- "$PGO_BIN"
run_load
stop_node
"perf2bolt-$LLVM_BOLT_VERSION" -nl -p "$WORK/perf.data" -o "$WORK/perf.fdata" "$PGO_BIN"
"llvm-bolt-$LLVM_BOLT_VERSION" "$PGO_BIN" -o "$WORK/teseladb.bolt" -data="$WORK/perf.fdata" \
  -reorder-blocks=ext-tsp -reorder-functions=hfsort -split-functions -split-all-cold \
  -dyno-stats
[ "$("$WORK/teseladb.bolt" --version)" = "teseladb $TESELADB_RELEASE_VERSION" ] || {
  echo "pgo-bolt: the binary laid out by BOLT does not run" >&2
  exit 1
}

for triple in "$HOST" aarch64-unknown-linux-gnu aarch64-apple-darwin; do
  mkdir -p "/work/out/$triple/dist"
  cp "$CARGO_TARGET_DIR/$triple/dist/teseladb" "/work/out/$triple/dist/teseladb"
done
cp "$WORK/teseladb.bolt" "/work/out/$HOST/dist/teseladb"
step "done: PGO on every target, BOLT on $HOST"
