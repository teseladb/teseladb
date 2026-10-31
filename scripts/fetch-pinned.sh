#!/usr/bin/env bash
# Downloads the files of a pinned list into .tools/ (ignored by git), each checked against its
# SHA-256: test data and source data that the repository does not copy (decisions D-042, D-043).
# A list has a `# source <base URL>` line, a `# dest <directory under the repository>` line and one
# `<sha256>  <path>` line per file; a file is fetched from `<source>/<path>` into `<dest>/<path>`.
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
  if ! curl -fsSL -o "$file.tmp" "$URL/$name"; then
    rm -f "$file.tmp"
    echo "fetch-pinned: cannot download $URL/$name" >&2
    exit 1
  fi
  got=$(sha_of "$file.tmp")
  if [[ $got != "$sha" ]]; then
    rm -f "$file.tmp" "$file"
    echo "fetch-pinned: $name has SHA-256 $got, not $sha; not used" >&2
    exit 1
  fi
  mv "$file.tmp" "$file"
  fetched=$((fetched + 1))
done <"$LIST"
echo "fetch-pinned: $fetched downloaded" >&2
echo "$DEST"
