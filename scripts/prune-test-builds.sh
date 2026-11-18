#!/usr/bin/env bash
# Removes the earlier builds of test executables, which Cargo leaves next to the new ones, each up
# to 1 GiB with its debug information (they filled the Docker disk: 91 GiB, 18-11-2026). For each
# executable given, `<dir>/<name>-<hash>`, it removes the files `<dir>/<name>-<other hash>` that
# are executables or their `.d`, and never one of the executables given: two crates may build a
# test of the same name (the engine's own tests and the tests of the server's `teseladb` binary
# are both `teseladb-<hash>`), and each one's cleanup removed the other's, so a run of both
# found one of them missing. The libraries, and so the incremental build, stay.
#   scripts/prune-test-builds.sh <executable>...
set -euo pipefail

current=$(printf '%s\n' "$@")
for exe in "$@"; do
  name=$(basename "${exe%-*}")
  for old in "${exe%-*}"-*; do
    [[ -f $old ]] || continue
    file=$(basename "$old")
    # Another build of this same test: its name, a hash, and what goes with it (`.d`).
    [[ $file =~ ^${name}-[0-9a-f]{16}(\..*)?$ ]] || continue
    build=$(dirname "$old")/${file%%.*}
    if grep -Fxq -- "$build" <<<"$current"; then continue; fi
    if [[ -x $old || $old == *.d ]]; then rm -f -- "$old"; fi
  done
done
