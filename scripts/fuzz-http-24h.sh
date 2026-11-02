#!/usr/bin/env bash
# The 24-hour fuzzing of the parsers of 019 (T034, FR-029, SC-006): HTTP/1.1 and URLs, HTTP/2,
# HPACK and SSE, JSON-RPC of MCP, and Extended JSON, BSON and REST requests, each target for
# TESELADB_FUZZ_SECS seconds (86400 by default) with seeds drawn from the clock, one after another
# so that each has the machine to itself. Each target prints its seed; a finding fails its test
# with the input that broke it. Logs go to target/fuzz-019/<target>.log.
# FUZZ_PROFILE=dev runs the debug build (a short check of the script); by default, release.
# Usage: scripts/fuzz-http-24h.sh [seconds]
set -euo pipefail

secs="${1:-86400}"
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${CARGO_TARGET_DIR:-$root/target}/fuzz-019"
mkdir -p "$out"
profile=(--release)
if [ "${FUZZ_PROFILE:-release}" = "dev" ]; then
  profile=()
fi

targets=(
  "teseladb-http fuzz"
  "teseladb-http fuzz_server"
  "teseladb-mcp fuzz"
  "teseladb-rest fuzz"
)

failed=0
for t in "${targets[@]}"; do
  read -r crate test <<<"$t"
  log="$out/$crate-$test.log"
  echo "fuzz: $crate/$test for ${secs}s, log $log"
  if ! TESELADB_FUZZ_SECS="$secs" CARGO_INCREMENTAL=0 cargo test -j 4 ${profile[@]+"${profile[@]}"} -p "$crate" --test "$test" -- --nocapture >"$log" 2>&1; then
    echo "fuzz: $crate/$test FOUND A FAILURE (see $log)"
    failed=1
  fi
done
exit "$failed"
