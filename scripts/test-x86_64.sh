#!/usr/bin/env bash
# Runs the tests of the given crates (default: every crate of the workspace) on x86_64 Linux: they
# are built for `x86_64-unknown-linux-musl` (static, linked with rust-lld) and run with
# `qemu-x86_64 -cpu max` inside a local image (scripts/docker/x86_64-qemu.Dockerfile). That CPU
# model has SSE4.2, SSSE3, SSE4.1 and the SHA extensions on any host, so the x86_64 paths (CRC32C,
# Adler-32, SHA-NI, epoll, eventfd) run even on an AArch64 machine; `TESELADB_EXPECT_ACCEL` makes the
# tests of teseladb-hash fail if one of the hash accelerations is not detected (001 FR-023).
#   scripts/test-x86_64.sh [crate...]
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-musl
IMAGE=teseladb-x86_64-qemu
crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
  for d in crates/*/; do crates+=("$(basename "$d")"); done
fi
pkgs=()
for c in "${crates[@]}"; do pkgs+=(-p "$c"); done

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
docker build -q -t "$IMAGE" -f scripts/docker/x86_64-qemu.Dockerfile scripts/docker >/dev/null
# Only test binaries, as `crate-dir:binary`: those built with the test profile. A target's own
# `"test":true` only says that it can be tested; the plain binaries that integration tests run
# through `CARGO_BIN_EXE_*`, and examples, are not tests.
runs=()
while IFS= read -r run; do runs+=("$run"); done < <(
  cargo test --offline --target "$TARGET" "${pkgs[@]}" --no-run --message-format=json 2>/dev/null |
    grep -E '"profile":\{[^}]*"test":true' |
    sed -n 's/.*"manifest_path":"\([^"]*\)\/Cargo.toml".*"executable":"\([^"]*\)".*/\1:\2/p'
)
[[ ${#runs[@]} -gt 0 ]] || { echo "test-x86_64: nothing was built" >&2; exit 1; }

# The repository is mounted at its own path: the tests find their files through paths fixed at
# build time (`env!("CARGO_MANIFEST_DIR")`). Every binary runs from its crate's directory, as
# `cargo test` does.
docker run --rm -i -e TESELADB_EXPECT_ACCEL=crc32c,sha1,sha256,adler32 -v "$PWD:$PWD" -w "$PWD" \
  "$IMAGE" sh -s -- "${runs[@]}" <<'EOF'
fail=0
for run in "$@"; do
  dir=${run%%:*}
  bin=${run#*:}
  name=$(basename "$bin")
  if out=$(cd "$dir" && qemu-x86_64 -cpu max "$bin" 2>&1); then
    echo "$out" | grep -E "^test result|^[a-z_]+: ok" | sed "s|^|$name: |"
  else
    echo "$out"
    echo "FAILED: $name"
    fail=1
  fi
done
exit $fail
EOF
