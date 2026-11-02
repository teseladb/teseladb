#!/usr/bin/env bash
# Runs the tests of the given crates (default: teseladb-sys and teseladb-io) on the Linux kernel of
# the local Docker engine, natively (no emulation): what only a real Linux kernel has, io_uring
# above all (012), runs here. The tests are built for `<arch>-unknown-linux-musl` (static, linked
# with rust-lld) and run inside the image of scripts/docker/x86_64-qemu.Dockerfile, without the
# seccomp profile (Docker's default one refuses the io_uring calls) and with a 1 MiB tmpfs at
# `TESELADB_FULL_DIR`, the full disk of the executors' equivalence battery.
#   scripts/test-linux.sh [crate...]
set -euo pipefail
cd "$(dirname "$0")/.."

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
