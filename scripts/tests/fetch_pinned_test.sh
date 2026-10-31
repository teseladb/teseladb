#!/usr/bin/env bash
# Tests of scripts/fetch-pinned.sh (D-042, D-043) with a local source over file://: the files of
# a list arrive with their SHA-256, in subdirectories too; a second run downloads nothing; a file
# whose SHA-256 does not match is refused and not left in place; a source without the file fails;
# a list without its source or destination is a usage error.
#   scripts/tests/fetch_pinned_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/fetch-pinned.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

check() {
  if [[ $2 -eq 0 ]]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: $1"
    [[ -n ${3:-} ]] && echo "$3"
  fi
}

# A repository with a two-file list, one of them in a subdirectory, and a source that has both.
mkdir -p "$WORK/repo/scripts" "$WORK/source/sub"
cp "$SCRIPT" "$WORK/repo/scripts/"
printf '{"a": 1}\n' >"$WORK/source/one.json"
printf '{"b": 2}\n' >"$WORK/source/sub/two.json"
LIST=$WORK/repo/scripts/data.sha256
{
  echo "# source https://example.invalid/data"
  echo "# dest .tools/data"
  (cd "$WORK/source" && shasum -a 256 one.json sub/two.json)
} >"$LIST"

fetch() {
  TESELADB_FETCH_URL="file://$WORK/source" "$WORK/repo/scripts/fetch-pinned.sh" "$LIST"
}

dest=$WORK/repo/.tools/data
out=$(fetch 2>&1) && rc=0 || rc=$?
ok=1
[[ $rc -eq 0 ]] && cmp -s "$dest/one.json" "$WORK/source/one.json" &&
  cmp -s "$dest/sub/two.json" "$WORK/source/sub/two.json" && grep -q "2 downloaded" <<<"$out" &&
  [[ $(tail -1 <<<"$out") == "$dest" ]] && ok=0
check "every file arrives under its destination (exit $rc)" "$ok" "$out"

# Already there: nothing is downloaded, even with the source gone.
mv "$WORK/source" "$WORK/away"
out=$(fetch 2>&1) && rc=0 || rc=$?
check "a second run downloads nothing (exit $rc)" "$([[ $rc -eq 0 ]] && grep -q "0 downloaded" <<<"$out" && echo 0 || echo 1)" "$out"
mv "$WORK/away" "$WORK/source"

# A changed file in the source is refused and the local copy goes away with it.
printf '{"b": 3}\n' >"$WORK/source/sub/two.json"
printf 'damaged' >"$dest/sub/two.json"
out=$(fetch 2>&1) && rc=0 || rc=$?
ok=1
[[ $rc -eq 1 && ! -e $dest/sub/two.json && ! -e $dest/sub/two.json.tmp ]] &&
  grep -q "sub/two.json has SHA-256 .*; not used" <<<"$out" && ok=0
check "a file with another SHA-256 is not used (exit $rc)" "$ok" "$out"

# A source that does not have the file fails.
rm "$WORK/source/sub/two.json"
out=$(fetch 2>&1) && rc=0 || rc=$?
check "a missing file fails (exit $rc)" "$([[ $rc -eq 1 ]] && grep -q "cannot download" <<<"$out" && echo 0 || echo 1)" "$out"

# A list without its source or destination, and no list at all, are usage errors.
for header in "# source" "# dest"; do
  grep -v "^$header " "$LIST" >"$WORK/partial.sha256"
  out=$("$WORK/repo/scripts/fetch-pinned.sh" "$WORK/partial.sha256" 2>&1) && rc=0 || rc=$?
  check "a list without '$header' (exit $rc)" "$([[ $rc -eq 2 ]] && echo 0 || echo 1)" "$out"
done
out=$("$WORK/repo/scripts/fetch-pinned.sh" 2>&1) && rc=0 || rc=$?
check "no list (exit $rc)" "$([[ $rc -eq 2 ]] && echo 0 || echo 1)" "$out"

echo "fetch_pinned_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
