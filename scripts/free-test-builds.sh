#!/usr/bin/env bash
# Makes room in a Cargo profile directory before a build of tests. A run of the tests of many
# crates links hundreds of test executables of 200-400 MiB each, and those of the crates of the
# run before are still there: the Docker disk filled up in the middle of a build three times on
# 18-11-2026. When the disk that holds <profile dir> has less than <GiB> free, this removes the
# executables of earlier builds (the files `deps/<name>-<16 hex digits>` that can run, with their
# `.d`) and their fingerprints (`.fingerprint/<package>-<the same digits>`), so that Cargo links
# again the ones the next build needs; and, if that does not make the room, the incremental
# caches. The libraries stay: nothing is compiled again but what the incremental caches held.
#   scripts/free-test-builds.sh <profile dir> <GiB>
set -euo pipefail

dir=$1
need=$2
[[ -d $dir/deps ]] || exit 0
# The GiB free on the disk of the directory (POSIX df: 1024-byte blocks, the fourth column).
free_gib() { df -Pk "$dir" | awk 'NR == 2 { print int($4 / 1048576) }'; }
[[ $(free_gib) -ge $need ]] && exit 0

removed=0
for exe in "$dir"/deps/*; do
  file=$(basename "$exe")
  [[ -f $exe && -x $exe && $file =~ ^.+-([0-9a-f]{16})$ ]] || continue
  hash=${BASH_REMATCH[1]}
  rm -f -- "$exe" "$exe.d"
  for fingerprint in "$dir"/.fingerprint/*-"$hash"; do
    if [[ -d $fingerprint ]]; then rm -r -- "$fingerprint"; fi
  done
  removed=$((removed + 1))
done
echo "free-test-builds: removed $removed executables of earlier builds"
if [[ $(free_gib) -lt $need && -d $dir/incremental ]]; then
  find "$dir/incremental" -mindepth 1 -maxdepth 1 -exec rm -r -- {} +
  echo "free-test-builds: removed the incremental caches"
fi
