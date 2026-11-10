#!/usr/bin/env bash
# Tests of the platform check of scripts/release/install.sh with uname, getconf and curl played by stand-ins: a Linux
# without glibc (musl, as in Alpine) or with a glibc older than 2.28 gets «there is no binary for» before anything is
# downloaded, a Linux with glibc 2.28 or later and macOS on Apple Silicon go on to read the newest version, and an
# unknown CPU is refused.
#   scripts/tests/install_platform_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/release/install.sh
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
passed=0
failed=0

mkdir -p "$WORK/bin"
cat >"$WORK/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo "$FAKE_SYSTEM" ;;
  -m) echo "$FAKE_CPU" ;;
esac
EOF
cat >"$WORK/bin/getconf" <<'EOF'
#!/bin/sh
[ -n "$FAKE_GLIBC" ] || exit 1
echo "glibc $FAKE_GLIBC"
EOF
# The download of latest.txt fails on purpose: reaching it means the platform check let the system through.
cat >"$WORK/bin/curl" <<'EOF'
#!/bin/sh
exit 22
EOF
chmod +x "$WORK/bin/uname" "$WORK/bin/getconf" "$WORK/bin/curl"

# check <name> <system> <cpu> <glibc or empty> <expected text>
check() {
  local name=$1 out
  out=$(env PATH="$WORK/bin:$PATH" FAKE_SYSTEM="$2" FAKE_CPU="$3" FAKE_GLIBC="$4" sh "$SCRIPT" 2>&1 || true)
  if grep -qF -- "$5" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $name: no «${5}» in: $out" >&2
  fi
}

check "Linux with musl" Linux x86_64 "" "there is no binary for Linux x86_64 without glibc"
check "Linux aarch64 with musl" Linux aarch64 "" "there is no binary for Linux aarch64 without glibc"
check "a glibc older than 2.28" Linux x86_64 2.17 "there is no binary for Linux x86_64 with glibc 2.17"
check "glibc 2.28" Linux x86_64 2.28 "could not read the newest version"
check "a newer glibc" Linux aarch64 2.36 "could not read the newest version"
check "glibc 3.0" Linux x86_64 3.0 "could not read the newest version"
check "macOS on Apple Silicon" Darwin arm64 "" "could not read the newest version"
check "an unknown CPU" Linux riscv64 2.36 "there is no binary for Linux riscv64"

echo "install_platform_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
