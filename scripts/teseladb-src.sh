#!/usr/bin/env bash
# Prints a folder with one entry per viuzi-* crate of the build, each a link to the source Cargo unpacked from the
# private registry at the version of Cargo.lock: target/teseladb-src/<crate>. The gates that read or test that code (the
# locks of the hot path in check-deps.sh, the faulty variants of sim-mutants.sh, the simulated crashes of teseladb-raftlog
# and the fuzzing of final-phase.sh) take it from here, so they check the code TeselaDB is built with.
#   scripts/teseladb-src.sh            the folder of links, for the gates that read the sources
#   scripts/teseladb-src.sh <crate>    the real folder of one crate, outside this repository, for the commands that build it
#                                   (Cargo would take a link under the repository for a member of this workspace)
# Bash 3.2 (macOS) or later.
set -euo pipefail
cd "$(dirname "$0")/.."

out=target/teseladb-src
mkdir -p "$out"
# Downloads the crates of Cargo.lock when they are not unpacked yet; the metadata then names their manifests.
cargo fetch --locked >/dev/null
found=0
while IFS= read -r manifest; do
  dir=${manifest%/Cargo.toml}
  base=${dir##*/}
  name=${base%-[0-9]*.[0-9]*.[0-9]*}
  ln -sfn "$dir" "$out/$name"
  found=$((found + 1))
done < <(cargo metadata --locked --format-version 1 |
  grep -oE '"manifest_path":"[^"]*/viuzi-[a-z0-9-]+-[0-9]+\.[0-9]+\.[0-9]+[^"/]*/Cargo.toml"' |
  sed -E 's/^"manifest_path":"(.*)"$/\1/' | sort -u)
[[ $found -gt 0 ]] || { echo "teseladb-src: the build has no viuzi-* crate from the registry" >&2; exit 1; }
if [[ $# -eq 0 ]]; then
  echo "$PWD/$out"
elif [[ -L $out/$1 ]]; then
  (cd -P "$out/$1" && pwd)
else
  echo "teseladb-src: $1 is not a viuzi-* crate of the build" >&2
  exit 1
fi
