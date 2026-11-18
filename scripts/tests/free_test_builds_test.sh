#!/usr/bin/env bash
# Tests of scripts/free-test-builds.sh: with room enough it removes nothing; without it, the
# executables of earlier builds go with their `.d` and their fingerprints, then the incremental
# caches, and the libraries, the shared objects and the files without a build's digits stay.
#   scripts/tests/free_test_builds_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

FREE=$PWD/scripts/free-test-builds.sh
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

A=0123456789abcdef B=fedcba9876543210 C=aaaaaaaaaaaaaaaa
# A profile directory: two test executables, a library and a shared object of a build, a tool
# without digits, their fingerprints and an incremental cache.
profile() {
  local dir=$1
  mkdir -p "$dir/deps" "$dir/.fingerprint/pkg-$A" "$dir/.fingerprint/pkg-$B" \
    "$dir/.fingerprint/lib-$C" "$dir/incremental/pkg-1x/s-1"
  for f in "scan-$A" "teseladb-$B" "libmacro-$C.so" tool; do
    printf 'x' >"$dir/deps/$f" && chmod +x "$dir/deps/$f"
  done
  for f in "scan-$A.d" "libpkg-$C.rlib" "libpkg-$C.rmeta"; do printf 'x' >"$dir/deps/$f"; done
  printf 'x' >"$dir/.fingerprint/pkg-$A/test-integration-test-scan"
  printf 'x' >"$dir/.fingerprint/pkg-$B/bin-teseladb"
  printf 'x' >"$dir/.fingerprint/lib-$C/lib-pkg"
  printf 'x' >"$dir/incremental/pkg-1x/s-1/query-cache.bin"
}
left() { (cd "$1" && find . -type f | sort | tr '\n' ' '); }

profile "$WORK/roomy"
before=$(left "$WORK/roomy")
"$FREE" "$WORK/roomy" 0 >/dev/null
check "with room enough nothing goes" "$before" "$(left "$WORK/roomy")"

profile "$WORK/full"
out=$("$FREE" "$WORK/full" 100000000)
check "without room the executables go with their fingerprints, then the incremental caches" \
  "./.fingerprint/lib-$C/lib-pkg ./deps/libmacro-$C.so ./deps/libpkg-$C.rlib ./deps/libpkg-$C.rmeta ./deps/tool " \
  "$(left "$WORK/full")"
check "it says what it removed" "2" "$(grep -c . <<<"$out")"
check "the incremental directory itself stays" "1" "$(find "$WORK/full/incremental" -maxdepth 0 | grep -c .)"

mkdir -p "$WORK/none"
check "a directory without builds is no error" "0" "$("$FREE" "$WORK/none" 100000000 >/dev/null 2>&1; echo $?)"

[[ $failed -eq 0 ]] || exit 1
echo "free_test_builds_test: OK"
