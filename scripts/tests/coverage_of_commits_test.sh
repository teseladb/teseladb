#!/usr/bin/env bash
# Tests of scripts/coverage-of-commits.py on a throwaway repository: crate `one` has a source whose
# first three lines come from a first commit and whose last three from a second one, and a coverage
# output written by hand in the form scripts/coverage.sh gives (show.txt and an export). Measured for
# the second commit alone, the script must count only its lines, the branch and the function that
# start on them, and name what never ran.
#   scripts/tests/coverage_of_commits_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/coverage-of-commits.py
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

check() {
  local what=$1 want=$2 got=$3
  if [[ $got == "$want" ]]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    printf 'FAIL %s\n  want: %s\n  got:  %s\n' "$what" "$want" "$got" >&2
  fi
}

repo=$WORK/repo
mkdir -p "$repo/crates/one/src"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.invalid
git -C "$repo" config user.name test
src=$repo/crates/one/src/lib.rs
printf 'fn old() {}\nfn old2() {}\n// old\n' >"$src"
git -C "$repo" add -A
git -C "$repo" commit -q -m first
printf 'fn new(x: bool) {\n    if x { y() }\n}\n' >>"$src"
git -C "$repo" commit -q -am second
git -C "$repo" rev-parse HEAD >"$WORK/commits"

out=$WORK/coverage
mkdir -p "$out/export"
# Lines 1 and 2 ran, 3 is a comment; of the second commit's, 4 and 5 ran and 6 did not.
cat >"$out/show.txt" <<'EOF'
crates/one/src/lib.rs:
     1|      1|fn old() {}
     2|      0|fn old2() {}
     3|       |// old
     4|      2|fn new(x: bool) {
     5|      2|    if x { y() }
     6|      0|}

EOF
# A branch of the first commit never taken; one of the second taken one way only. The function of
# the second commit ran, the second one of the first never did.
cat >"$out/export/0.json" <<EOF
{"data": [{"files": [{"filename": "$repo/crates/one/src/lib.rs",
  "branches": [[2, 1, 2, 9, 0, 0, 0, 0, 4], [5, 8, 5, 9, 2, 0, 0, 0, 4]]}],
  "functions": [{"filenames": ["$repo/crates/one/src/lib.rs"], "count": 0, "regions": [[2, 1, 2, 12]]},
                {"filenames": ["$repo/crates/one/src/lib.rs"], "count": 2, "regions": [[4, 1, 6, 2]]}]}]}
EOF

got=$(cd "$repo" && python3 -I "$SCRIPT" "$out" "$WORK/commits" one)
want='one  lines 2/3 66.67 %  functions 1/1 100.00 %  branches 1/2 50.00 %
  crates/one/src/lib.rs:5:8: false
  crates/one/src/lib.rs:6: line'
check "the second commit's lines, branch and function" "$want" "$got"

# Without arguments it prints its usage and fails.
rc=0
python3 -I "$SCRIPT" >/dev/null 2>&1 || rc=$?
check "usage" 2 "$rc"

echo "coverage-of-commits: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
