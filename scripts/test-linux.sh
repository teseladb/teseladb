#!/usr/bin/env bash
# Runs the tests of the given crates (default: teseladb-sys and teseladb-io) on the Linux kernel of
# the local Docker engine, natively (no emulation): what only a real Linux kernel has, io_uring
# above all (012), runs here. The tests are built for `<arch>-unknown-linux-musl` (static, linked
# with rust-lld) and run inside the image of scripts/docker/x86_64-qemu.Dockerfile, without the
# seccomp profile (Docker's default one refuses the io_uring calls) and with a 1 MiB tmpfs at
# `TESELADB_FULL_DIR`, the full disk of the executors' equivalence battery.
# With --huge-pages the engine's kernel first reserves 2 MiB pages, and 1 GiB pages where it can,
# so the tests of 012 FR-032 map their pools on them, and gets its own counts back at the end.
#   scripts/test-linux.sh [--huge-pages] [crate...]
set -euo pipefail
cd "$(dirname "$0")/.."

huge=0
if [[ ${1:-} == --huge-pages ]]; then
  huge=1
  shift
fi

case $(uname -m) in
  arm64 | aarch64) TARGET=aarch64-unknown-linux-musl ;;
  *) TARGET=x86_64-unknown-linux-musl ;;
esac
IMAGE=teseladb-x86_64-qemu
crates=("$@")
[[ ${#crates[@]} -gt 0 ]] || crates=(teseladb-sys teseladb-io)
pkgs=()
for c in "${crates[@]}"; do pkgs+=(-p "$c"); done

linker_var=CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER
export "$linker_var"=rust-lld
docker build -q -t "$IMAGE" -f scripts/docker/x86_64-qemu.Dockerfile scripts/docker >/dev/null

# The kernel's pools of huge pages: /sys/kernel/mm is the engine's, reached from a privileged
# container. 256 pages of 2 MiB, and two of 1 GiB (a test maps one while a cache maps another)
# where the kernel can make them after boot: one without CONFIG_CONTIG_ALLOC only has the 1 GiB
# pages its command line reserved, and the tests then take the path of a host without them.
HUGE_DIR=/sys/kernel/mm/hugepages
set_huge() {
  docker run --rm --privileged "$IMAGE" sh -c \
    "echo $2 > $HUGE_DIR/hugepages-2048kB/nr_hugepages; echo $1 > $HUGE_DIR/hugepages-1048576kB/nr_hugepages 2>/dev/null;
     echo \$(cat $HUGE_DIR/hugepages-1048576kB/nr_hugepages $HUGE_DIR/hugepages-2048kB/nr_hugepages)"
}
if [[ $huge == 1 ]]; then
  read -r before_1g before_2m < <(docker run --rm "$IMAGE" sh -c \
    "echo \$(cat $HUGE_DIR/hugepages-1048576kB/nr_hugepages $HUGE_DIR/hugepages-2048kB/nr_hugepages)")
  trap 'set_huge "$before_1g" "$before_2m" >/dev/null' EXIT
  read -r got_1g got_2m < <(set_huge 2 256)
  [[ $got_2m == 256 ]] || { echo "test-linux: the kernel reserved $got_2m of 256 pages of 2 MiB" >&2; exit 1; }
  echo "test-linux: reserved $got_2m pages of 2 MiB and $got_1g of 1 GiB"
fi
runs=()
while IFS= read -r run; do runs+=("$run"); done < <(
  cargo test --offline --target "$TARGET" "${pkgs[@]}" --no-run --message-format=json 2>/dev/null |
    grep -E '"profile":\{[^}]*"test":true' |
    sed -n 's/.*"manifest_path":"\([^"]*\)\/Cargo.toml".*"executable":"\([^"]*\)".*/\1:\2/p'
)
[[ ${#runs[@]} -gt 0 ]] || { echo "test-linux: nothing was built" >&2; exit 1; }

docker run --rm -i --security-opt seccomp=unconfined --tmpfs /full:rw,size=1m \
  -e TESELADB_FULL_DIR=/full -v "$PWD:$PWD" -w "$PWD" "$IMAGE" \
  sh -s -- "${runs[@]}" <<'SH'
fail=0
for run in "$@"; do
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
