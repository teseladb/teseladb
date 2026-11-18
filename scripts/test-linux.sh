#!/usr/bin/env bash
# Runs the tests of the given crates (default: the five Linux-only packages, 023 FR-007: the
# engine `teseladb`, teseladb-sys, teseladb-io, teseladb-server and teseladb-bench) on the Linux
# kernel of the local Docker engine, natively (no emulation): the server, its I/O crates and the
# engine are Linux only (D-177), and io_uring above all runs here. The tests are built and run
# inside a container for `<arch>-unknown-linux-gnu`, the target of the release binaries (023 R10):
# the engine is tested and measured with glibc. The image (`TESELADB_LINUX_IMAGE`, by default
# `anotherproduct-rs-bench:1.98.0`) has the pinned Rust toolchain, glibc and the libraries the engine's
# battery links; the build goes to the volume `TESELADB_LINUX_VOLUME` (default
# `teseladb-linux-target`) under `TESELADB_LINUX_TARGET_DIR`, and the crates come from the host's
# Cargo registry (offline). `TESELADB_LINUX_CPUS` pins the container to a cpuset, and
# `TESELADB_LINUX_FREE_GIB` (50) is the room a build of tests wants on the volume's disk and
# `TESELADB_LINUX_DEBUG` the debug information of the tests' build (none). Every other
# `TESELADB_*` variable of the caller (`TESELADB_KILL_ROUNDS`…) reaches the tests.
# The container is privileged (io_uring, the engine's huge pages and NUMA binding, and the tests of
# the port on AF_XDP, 012 FR-033, which make `veth` pairs and namespaces inside it, load the XDP
# program and lock the UMEM), with a 1 MiB tmpfs at `TESELADB_FULL_DIR`, the full disk of the
# executors' equivalence battery.
# Every test binary runs, those with their own `main` (`harness = false`) included.
# Options:
#   --huge-pages    the engine's kernel first reserves 2 MiB pages, so the engine's memory goes on
#                   them (023 FR-014), and gets its own counts back at the end
#   --release       builds and runs the tests with the release profile
#   --test <name>   (once or more) only those test files of the crates
#   --lints         Clippy on the crates, every target, warnings as errors, instead of the tests
#   --needed        builds the release `teseladb` and checks the libraries it needs with
#                   `scripts/check-deps.sh --needed` (023 SC-015), instead of the tests
#   scripts/test-linux.sh [--huge-pages] [--release] [--test <name>]... [--lints|--needed] [crate...]
set -euo pipefail
cd "$(dirname "$0")/.."

LINUX_CRATES=(teseladb teseladb-sys teseladb-io teseladb-server teseladb-bench)

huge=0
mode=tests
cargo_args=()
while [[ ${1:-} == --* ]]; do
  case $1 in
    --huge-pages) huge=1 ;;
    --release) cargo_args+=(--release) ;;
    --test)
      [[ $# -ge 2 ]] || { echo "test-linux: --test needs a name" >&2; exit 2; }
      cargo_args+=(--test "$2")
      shift
      ;;
    --lints) mode=lints ;;
    --needed) mode=needed ;;
    *)
      echo "usage: scripts/test-linux.sh [--huge-pages] [--release] [--test <name>]... [--lints|--needed] [crate...]" >&2
      exit 2
      ;;
  esac
  shift
done

case $(uname -m) in
  arm64 | aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  *) TARGET=x86_64-unknown-linux-gnu ;;
esac
IMAGE=${TESELADB_LINUX_IMAGE:-anotherproduct-rs-bench:1.98.0}
VOLUME=${TESELADB_LINUX_VOLUME:-teseladb-linux-target}
TARGET_DIR=${TESELADB_LINUX_TARGET_DIR:-/target/test-linux}
# The room a build of tests wants on the volume's disk, in GiB: with less, the executables of
# earlier builds are removed before it (scripts/free-test-builds.sh).
FREE_GIB=${TESELADB_LINUX_FREE_GIB:-50}
# The debug information of the tests' build (Cargo's `debug`): none. Every test executable links
# the whole server; with line tables only each one weighed 280 MiB, 196 of them debug sections,
# and the 635 of the workspace did not fit in the Docker disk (18-11-2026). A backtrace with
# lines wants `TESELADB_LINUX_DEBUG=line-tables-only`, which builds everything again.
TEST_DEBUG=${TESELADB_LINUX_DEBUG:-0}
REGISTRY=${CARGO_HOME:-$HOME/.cargo}/registry
crates=("$@")
[[ ${#crates[@]} -gt 0 ]] || crates=("${LINUX_CRATES[@]}")
args=()
for c in "${crates[@]}"; do args+=(-p "$c"); done
args+=(${cargo_args[@]+"${cargo_args[@]}"})
cpuset=()
[[ -n ${TESELADB_LINUX_CPUS:-} ]] && cpuset=(--cpuset-cpus "$TESELADB_LINUX_CPUS")
# The caller's TESELADB_* variables but this script's own, by name: Docker takes their values.
passed_env=()
while IFS= read -r name; do
  passed_env+=(-e "$name")
done < <(env | sed -n 's/^\(TESELADB_[A-Za-z0-9_]*\)=.*/\1/p' | grep -v '^TESELADB_LINUX_' || true)

# The kernel's pools of huge pages: /sys/kernel/mm is the engine's, reached from a privileged
# container.
HUGE_DIR=/sys/kernel/mm/hugepages/hugepages-2048kB
set_huge() {
  docker run --rm --privileged "$IMAGE" sh -c \
    "echo $1 > $HUGE_DIR/nr_hugepages; cat $HUGE_DIR/nr_hugepages"
}
if [[ $huge == 1 ]]; then
  before=$(docker run --rm "$IMAGE" cat "$HUGE_DIR/nr_hugepages")
  trap 'set_huge "$before" >/dev/null' EXIT
  got=$(set_huge 1024)
  [[ $got == 1024 ]] || { echo "test-linux: the kernel reserved $got of 1024 pages of 2 MiB" >&2; exit 1; }
  echo "test-linux: reserved $got pages of 2 MiB"
fi

# --dns 127.0.0.1: the container's resolver asks loopback, where engine_blocking's test of a slow
# lookup (023 T033) runs a DNS server that never answers; nothing else resolves names off the host.
docker run --rm -i --privileged --dns 127.0.0.1 ${cpuset[@]+"${cpuset[@]}"} \
  --tmpfs /full:rw,size=1m --ulimit memlock=-1 -e TESELADB_FULL_DIR=/full \
  ${passed_env[@]+"${passed_env[@]}"} -e MODE="$mode" -e FREE_GIB="$FREE_GIB" \
  -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR="$TARGET_DIR" -e TARGET="$TARGET" \
  -e CARGO_PROFILE_DEV_DEBUG="$TEST_DEBUG" \
  -v "$REGISTRY:/cargo-home/registry" -v "$VOLUME:/target" -v "$PWD:$PWD" -w "$PWD" "$IMAGE" \
  bash -s -- "${args[@]}" <<'SH'
set -uo pipefail
case $MODE in
  lints)
    exec cargo clippy --offline --target "$TARGET" --all-targets "$@" -- -D warnings ;;
  needed)
    cargo build --offline --target "$TARGET" --release -p teseladb-server || exit 1
    exec scripts/check-deps.sh --needed "$CARGO_TARGET_DIR/$TARGET/release/teseladb" ;;
esac
# The tests build without debug information (`CARGO_PROFILE_DEV_DEBUG`, see `TEST_DEBUG`): a
# panic still names its file and line, which the compiler writes in the code.
# Every test binary, as `crate-dir:binary`: those built with the test profile, and the test targets
# with their own `main` (`harness = false`, built without it). The plain programs that integration
# tests run through `CARGO_BIN_EXE_*`, and examples, are not tests.
# A test that does not build fails the run and shows cargo's errors: before, they went to /dev/null and
# the binaries that did build ran alone, or the run ended with "nothing was built".
# One run of tests at a time in a volume; a second one waits here for the first. Each run makes
# room and prunes: it removes test executables, and one started while another still ran its
# tests took away those it had built and not run yet (18-11-2026: 87 "No such file").
mkdir -p "$CARGO_TARGET_DIR"
exec 9>"$CARGO_TARGET_DIR/.test-linux.lock"
flock 9
# Room first: the executables of the crates of earlier runs stay in the volume, and a build of
# many crates ended with "No space left on device" (scripts/free-test-builds.sh).
scripts/free-test-builds.sh "$CARGO_TARGET_DIR/$TARGET/debug" "$FREE_GIB"
if ! cargo test --offline --target "$TARGET" "$@" --no-run --message-format=json \
  >/tmp/test-build.json 2>/dev/null; then
  # The JSON carries the diagnostics: the same build, cached, says them as text.
  cargo test --offline --target "$TARGET" "$@" --no-run >&2
  echo "test-linux: the tests did not build" >&2
  exit 1
fi
runs=()
while IFS= read -r run; do runs+=("$run"); done < <(
  grep -E '"profile":\{[^}]*"test":true|"kind":\["test"\]' /tmp/test-build.json |
    sed -n 's/.*"manifest_path":"\([^"]*\)\/Cargo.toml".*"executable":"\([^"]*\)".*/\1:\2/p'
)
[[ ${#runs[@]} -gt 0 ]] || { echo "test-linux: nothing was built" >&2; exit 1; }
# Cargo keeps every earlier build of a test executable next to the new one, each up to 1 GiB with
# its debug information: they filled the Docker disk (91 GiB of test binaries, 18-11-2026). Only
# this build's executables stay (scripts/prune-test-builds.sh).
exes=()
for run in "${runs[@]}"; do exes+=("${run#*:}"); done
scripts/prune-test-builds.sh "${exes[@]}"
fail=0
for run in "${runs[@]}"; do
  dir=${run%%:*}
  bin=${run#*:}
  name=$(basename "$bin")
  # The engine's two programs that a Python driver of its tests/data starts run through it, as
  # scripts/coverage.sh runs them: alone, metrics_tester has no configuration to read.
  case $name in
    metrics_tester-*)
      cmd=(python3 tests/data/prometheus_test.py --exporter "$bin" --config tests/data/conf-example.yaml) ;;
    rest_api_httpd-*) cmd=(python3 tests/data/json2code_test.py --rest-api-httpd "$bin") ;;
    *) cmd=("$bin") ;;
  esac
  if out=$(cd "$dir" && "${cmd[@]}" 2>&1); then
    # A program with its own `main` may print neither kind of line: its name alone says it ran.
    shown=$(echo "$out" | grep -E "^test result|^[a-z_]+: ok" | sed "s|^|$name: |")
    echo "${shown:-$name: ok}"
    [[ ${cmd[0]} == python3 ]] && echo "$name: ok (through ${cmd[1]##*/})"
  else
    # What failed first: a test that starts node processes prints their logs after its panic,
    # and the last lines alone showed the logs without it. The whole message of the panic: a
    # test that reports what each node held when it gave up says it in a dozen lines, and six
    # showed the first node's alone.
    echo "$out" | grep -E -A40 "panicked at|^error|^failures:" | head -160
    echo "$out" | tail -40
    echo "FAILED: $name"
    fail=1
  fi
done
exit $fail
SH
