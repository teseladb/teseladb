#!/usr/bin/env bash
# Runs the tests of the given crates (default: teseladb-sys and teseladb-io) on the Linux kernel of
# the local Docker engine, natively (no emulation): the server, its I/O crates and the engine are
# Linux only (023 FR-007, D-177), and io_uring above all runs here. The tests are built and run
# inside a container for `<arch>-unknown-linux-gnu`, the target of the release binaries (023 R10):
# the engine is tested and measured with glibc. The image (`TESELADB_LINUX_IMAGE`, by default
# `anotherproduct-rs-bench:1.98.0`) has the pinned Rust toolchain, glibc and the libraries the engine's
# battery links; the build goes to the volume `TESELADB_LINUX_VOLUME` (default
# `teseladb-linux-target`) under `TESELADB_LINUX_TARGET_DIR`, and the crates come from the host's
# Cargo registry (offline). `TESELADB_LINUX_CPUS` pins the container to a cpuset.
# The container is privileged (io_uring, the engine's huge pages and NUMA binding, and the tests of
# the port on AF_XDP, 012 FR-033, which make `veth` pairs and namespaces inside it, load the XDP
# program and lock the UMEM), with a 1 MiB tmpfs at `TESELADB_FULL_DIR`, the full disk of the
# executors' equivalence battery.
# With --huge-pages the engine's kernel first reserves 2 MiB pages, so the engine's memory goes on
# them (023 FR-014), and gets its own counts back at the end.
# With --test <name> (once or more) only those test files of the crates are built and run.
#   scripts/test-linux.sh [--huge-pages] [--test <name>]... [crate...]
set -euo pipefail
cd "$(dirname "$0")/.."

huge=0
if [[ ${1:-} == --huge-pages ]]; then
  huge=1
  shift
fi
only=()
while [[ ${1:-} == --test ]]; do
  only+=(--test "$2")
  shift 2
done

case $(uname -m) in
  arm64 | aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  *) TARGET=x86_64-unknown-linux-gnu ;;
esac
IMAGE=${TESELADB_LINUX_IMAGE:-anotherproduct-rs-bench:1.98.0}
VOLUME=${TESELADB_LINUX_VOLUME:-teseladb-linux-target}
TARGET_DIR=${TESELADB_LINUX_TARGET_DIR:-/target/test-linux}
REGISTRY=${CARGO_HOME:-$HOME/.cargo}/registry
crates=("$@")
[[ ${#crates[@]} -gt 0 ]] || crates=(teseladb-sys teseladb-io)
args=()
for c in "${crates[@]}"; do args+=(-p "$c"); done
args+=(${only[@]+"${only[@]}"})
cpuset=()
[[ -n ${TESELADB_LINUX_CPUS:-} ]] && cpuset=(--cpuset-cpus "$TESELADB_LINUX_CPUS")

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
  -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR="$TARGET_DIR" -e TARGET="$TARGET" \
  -v "$REGISTRY:/cargo-home/registry" -v "$VOLUME:/target" -v "$PWD:$PWD" -w "$PWD" "$IMAGE" \
  bash -s -- "${args[@]}" <<'SH'
set -uo pipefail
runs=()
while IFS= read -r run; do runs+=("$run"); done < <(
  cargo test --offline --target "$TARGET" "$@" --no-run --message-format=json 2>/dev/null |
    grep -E '"profile":\{[^}]*"test":true' |
    sed -n 's/.*"manifest_path":"\([^"]*\)\/Cargo.toml".*"executable":"\([^"]*\)".*/\1:\2/p'
)
[[ ${#runs[@]} -gt 0 ]] || { echo "test-linux: nothing was built" >&2; exit 1; }
fail=0
for run in "${runs[@]}"; do
  dir=${run%%:*}
  bin=${run#*:}
  name=$(basename "$bin")
  if out=$(cd "$dir" && "$bin" 2>&1); then
    echo "$out" | grep -E "^test result|^[a-z_]+: ok" | sed "s|^|$name: |"
  else
    echo "$out" | tail -40
    echo "FAILED: $name"
    fail=1
  fi
done
exit $fail
SH
