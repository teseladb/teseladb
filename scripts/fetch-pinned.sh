#!/usr/bin/env bash
# Downloads the files of a pinned list into .tools/ (ignored by git), each checked against its
# SHA-256: test data and source data that the repository does not copy (decisions D-042, D-043).
# A list has a `# source <base URL>` line, a `# dest <directory under the repository>` line and one
# `<sha256>  <path>` line per file; a file is fetched from `<source>/<path>` into `<dest>/<path>`.
# Optional `# mirror <base URL>` lines name other places with the same files, tried in order when
# the source does not answer or serves other bytes; every copy is held to the same SHA-256. Each
# download is retried (TESELADB_FETCH_RETRIES times, 4 by default) before moving on.
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

fetched=0
while read -r sha name; do
  [[ -z $sha || $sha == \#* ]] && continue
  file=$DEST/$name
  if [[ -f $file && $(sha_of "$file") == "$sha" ]]; then
    continue
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
    exit 1
  fi
  fetched=$((fetched + 1))
done <"$LIST"
echo "fetch-pinned: $fetched downloaded" >&2
echo "$DEST"
