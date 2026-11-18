#!/usr/bin/env bash
# Tests of scripts/prune-test-builds.sh: it removes the earlier builds of each test executable
# and their `.d`, keeps this build's, the libraries and the tests of other names, and keeps two
# executables of the same name when both are of this build.
#   scripts/tests/prune_test_builds_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

PRUNE=$PWD/scripts/prune-test-builds.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
failed=0

check() {
  local what=$1 want=$2 got=$3
  if [[ $got == "$want" ]]; then
    echo "ok   $what"
  else
    echo "FAIL $what: wanted [$want], got [$got]"
    failed=$((failed + 1))
  fi
}

A=aaaaaaaaaaaaaaaa B=bbbbbbbbbbbbbbbb OLD=0123456789abcdef OLDER=fedcba9876543210
deps=$WORK/deps
mkdir -p "$deps"
exe() { printf 'x' >"$deps/$1" && chmod +x "$deps/$1"; }
plain() { printf 'x' >"$deps/$1"; }

# Two tests named `teseladb` of this build (the engine's and the server's binary's), with an
# earlier build; a test of another name with two earlier builds; and what must stay whatever.
exe "teseladb-$A"; plain "teseladb-$A.d"
exe "teseladb-$B"
exe "teseladb-$OLD"; plain "teseladb-$OLD.d"
exe "scan_parts-$A"
exe "scan_parts-$OLD"; exe "scan_parts-$OLDER"; plain "scan_parts-$OLDER.d"
exe "teseladb_io-$OLD"
plain "libteseladb-$OLD.rlib"; plain "libteseladb-$OLD.rmeta"
exe "teseladb-io-tool"

"$PRUNE" "$deps/teseladb-$A" "$deps/teseladb-$B" "$deps/scan_parts-$A"

left=$(cd "$deps" && ls | sort | tr '\n' ' ')
want="libteseladb-$OLD.rlib libteseladb-$OLD.rmeta scan_parts-$A teseladb-$A teseladb-$A.d teseladb-$B teseladb-io-tool teseladb_io-$OLD "
check "this build's executables stay, the earlier builds of the same tests go" \
  "$(printf '%s\n' $want | sort | tr '\n' ' ')" "$left"

"$PRUNE" "$deps/teseladb-$A" "$deps/teseladb-$B" "$deps/scan_parts-$A"
check "a second pass removes nothing" "$left" "$(cd "$deps" && ls | sort | tr '\n' ' ')"

check "an executable that is not there is no error" "0" \
  "$("$PRUNE" "$deps/none-$A" >/dev/null 2>&1; echo $?)"

[[ $failed -eq 0 ]] || exit 1
echo "prune_test_builds_test: OK"
