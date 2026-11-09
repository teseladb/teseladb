#!/usr/bin/env bash
# Downloads the files of a pinned list into .tools/ (ignored by git), each checked against its
# SHA-256: test data and source data that the repository does not copy (decisions D-042, D-043).
# A list has a `# source <base URL>` line, a `# dest <directory under the repository>` line and one
# `<sha256>  <path>` line per file; a file is fetched from `<source>/<path>` into `<dest>/<path>`.
# Optional `# mirror <base URL>` lines name other places with the same files, tried in order when
# the source does not answer or serves other bytes; every copy is held to the same SHA-256. Each
# download is retried (TESELADB_FETCH_RETRIES times, 4 by default) before moving on. The files
# are fetched TESELADB_FETCH_JOBS at a time (8 by default): a list of a thousand small files
# otherwise spends its time waiting on one connection after another.
# A file already there with the right SHA-256 is not downloaded again; one that does not match is
# never left in place. Prints the destination directory.
#   scripts/fetch-pinned.sh scripts/unicode-cldr.sha256
#   TESELADB_FETCH_URL=file:///dir scripts/fetch-pinned.sh <list>   another source (tests)
#   TESELADB_FETCH_DIR=/dir scripts/fetch-pinned.sh <list>          another destination (tests)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ $# -ne 1 || ! -f $1 ]]; then
  echo "usage: scripts/fetch-pinned.sh <list>" >&2
  exit 2
fi
LIST=$1
SOURCE=$(sed -nE 's/^# source (.+)$/\1/p' "$LIST")
DEST=$(sed -nE 's/^# dest (.+)$/\1/p' "$LIST")
if [[ -z $SOURCE || -z $DEST ]]; then
  echo "fetch-pinned: $LIST needs '# source <url>' and '# dest <directory>' lines" >&2
  exit 2
fi
URL=${TESELADB_FETCH_URL:-$SOURCE}
PLACES=("$URL")
while read -r mirror; do
  PLACES+=("$mirror")
done < <(sed -nE 's/^# mirror (.+)$/\1/p' "$LIST")
RETRIES=${TESELADB_FETCH_RETRIES:-4}
DEST=${TESELADB_FETCH_DIR:-$ROOT/$DEST}

sha_of() {
  shasum -a 256 "$1" | cut -d' ' -f1
}

JOBS=${TESELADB_FETCH_JOBS:-8}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/fetch-pinned.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# Fetches file `name` with SHA-256 `sha` unless it is there already; records the download in
# `<WORK>/fetched.<shard>` and a failure in `<WORK>/failed`.
fetch_one() {
  local sha=$1 name=$2 shard=$3 file got place
  file=$DEST/$name
  if [[ -f $file && $(sha_of "$file") == "$sha" ]]; then
    return 0
  fi
  mkdir -p "$(dirname "$file")"
  rm -f "$file"
  for place in "${PLACES[@]}"; do
    if ! curl -fsSL --retry "$RETRIES" --retry-all-errors --retry-delay 5 -o "$file.tmp" "$place/$name" 2>/dev/null; then
      rm -f "$file.tmp"
      continue
    fi
    got=$(sha_of "$file.tmp")
    if [[ $got == "$sha" ]]; then
      mv "$file.tmp" "$file"
      break
    fi
    rm -f "$file.tmp"
    echo "fetch-pinned: $name has SHA-256 $got, not $sha, at $place; not used" >&2
  done
  if [[ ! -f $file ]]; then
    echo "fetch-pinned: cannot download $name from ${PLACES[*]}" >&2
    echo "$name" >>"$WORK/failed"
    return 0
  fi
  echo "$name" >>"$WORK/fetched.$shard"
}

# Shard `k` of `JOBS`: the list's file lines whose number modulo `JOBS` is `k`, one after another.
shard() {
  local k=$1 i=0 sha name
  while read -r sha name; do
    [[ -z $sha || $sha == \#* ]] && continue
    if (( i % JOBS == k )); then
      fetch_one "$sha" "$name" "$k"
    fi
    i=$((i + 1))
  done <"$LIST"
}

for ((k = 0; k < JOBS; k++)); do
  shard "$k" &
done
wait
if [[ -s $WORK/failed ]]; then
  exit 1
fi
fetched=0
for f in "$WORK"/fetched.*; do
  [[ -f $f ]] && fetched=$((fetched + $(wc -l <"$f")))
done
echo "fetch-pinned: $fetched downloaded" >&2
echo "$DEST"
