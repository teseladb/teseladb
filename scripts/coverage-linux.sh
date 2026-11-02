#!/usr/bin/env bash
# Line, function and branch coverage of what only runs on Linux (012: io_uring, NUMA), measured on
# the Linux kernel of the local Docker engine: the tests of the given crates (default
# teseladb-sys and teseladb-io) are built instrumented for `<arch>-unknown-linux-musl` with the
# nightly toolchain, run as scripts/test-linux.sh runs them, and llvm-cov reports the files that
# match FILES (a regex; default: the Linux-only sources of 012).
#   scripts/coverage-linux.sh [crate...]
#   FILES='io_uring|numa' TESELADB_COV_MIN=95 scripts/coverage-linux.sh
# Writes coverage-linux/report.txt under the caller's `CARGO_TARGET_DIR` (default target).
set -euo pipefail
cd "$(dirname "$0")/.."

case $(uname -m) in
  arm64 | aarch64) TARGET=aarch64-unknown-linux-musl ;;
  *) TARGET=x86_64-unknown-linux-musl ;;
esac
TOOLCHAIN=${TESELADB_COV_TOOLCHAIN:-nightly}
SYSROOT=$(rustc +"$TOOLCHAIN" --print sysroot)
HOST=$(rustc +"$TOOLCHAIN" -vV | sed -n 's/^host: //p')
BIN="$SYSROOT/lib/rustlib/$HOST/bin"
FILES=${FILES:-'linux/io_uring\.rs|io_uring_disk\.rs|numa\.rs|node_disk\.rs|buffers\.rs'}
IMAGE=teseladb-x86_64-qemu
BASE=${CARGO_TARGET_DIR:-target}
OUT=$BASE/coverage-linux
rm -rf "$OUT"
mkdir -p "$OUT/prof"
crates=("$@")
[[ ${#crates[@]} -gt 0 ]] || crates=(teseladb-sys teseladb-io)
pkgs=()
for c in "${crates[@]}"; do pkgs+=(-p "$c"); done

linker_var=CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER
export "$linker_var"=rust-lld
export RUSTFLAGS="-C instrument-coverage -Z coverage-options=branch --cfg coverage -A unused_features"
export CARGO_TARGET_DIR=$BASE/coverage-linux-build
docker build -q -t "$IMAGE" -f scripts/docker/x86_64-qemu.Dockerfile scripts/docker >/dev/null
runs=()
while IFS= read -r run; do runs+=("$run"); done < <(
  cargo +"$TOOLCHAIN" test --offline --target "$TARGET" "${pkgs[@]}" --no-run --message-format=json 2>/dev/null |
    grep -E '"profile":\{[^}]*"test":true' |
    sed -n 's/.*"manifest_path":"\([^"]*\)\/Cargo.toml".*"executable":"\([^"]*\)".*/\1:\2/p'
)
[[ ${#runs[@]} -gt 0 ]] || { echo "coverage-linux: nothing was built" >&2; exit 1; }
PROF=$(cd "$OUT/prof" && pwd)
docker run --rm -i --security-opt seccomp=unconfined --tmpfs /full:rw,size=1m \
  -e TESELADB_FULL_DIR=/full -e LLVM_PROFILE_FILE="$PROF/%p-%m.profraw" -v "$PWD:$PWD" -w "$PWD" \
  "$IMAGE" sh -s -- "${runs[@]}" <<'SH'
fail=0
for run in "$@"; do
  dir=${run%%:*}
  bin=${run#*:}
  (cd "$dir" && "$bin" >/dev/null 2>&1) || { echo "FAILED: $(basename "$bin")"; fail=1; }
done
exit $fail
SH
"$BIN/llvm-profdata" merge -sparse "$OUT"/prof/*.profraw -o "$OUT/merged.profdata"
objects=()
for run in "${runs[@]}"; do objects+=(-object "${run#*:}"); done
sources=()
while IFS= read -r f; do sources+=("$f"); done < <(
  for c in "${crates[@]}"; do find "crates/$c/src" -name '*.rs'; done | grep -E "$FILES"
)
"$BIN/llvm-cov" report "${objects[@]:1}" -instr-profile="$OUT/merged.profdata" \
  --show-branch-summary "${sources[@]}" | tee "$OUT/report.txt"
if [[ -n ${TESELADB_COV_MIN:-} ]]; then
  python3 -I - "$OUT/report.txt" "$TESELADB_COV_MIN" <<'PY'
import sys
low = []
for line in open(sys.argv[1], encoding="utf-8"):
    cols = line.split()
    if not cols or not cols[0].endswith(".rs"):
        continue
    pcts = [c for c in cols if c.endswith("%")]
    if any(float(p.rstrip("%")) < float(sys.argv[2]) for p in pcts[1:]):
        low.append(line.strip())
if low:
    print("coverage-linux: below the minimum:\n" + "\n".join(low), file=sys.stderr)
    sys.exit(1)
PY
fi
