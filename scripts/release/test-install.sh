#!/usr/bin/env bash
# Runs install.sh against a release served over HTTP on localhost, as users run it against the downloads host: it must
# pick this machine's archive, install the binary, and stop without installing anything when the SHA-256 does not match.
#   scripts/release/test-install.sh <teseladb binary built for this machine> <version>
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/release/test-install.sh <binary> <version>" >&2
  exit 2
fi
BINARY=$1
VERSION=$2
HERE=$(cd "$(dirname "$0")" && pwd)
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) PLATFORM="linux-x86_64" ;;
  Linux-aarch64 | Linux-arm64) PLATFORM="linux-aarch64" ;;
  Darwin-arm64) PLATFORM="macos-aarch64" ;;
  *) echo "test-install: no platform for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
WORK=$(mktemp -d)
SERVER=""
cleanup() {
  if [[ -n $SERVER ]]; then kill "$SERVER" 2>/dev/null; wait "$SERVER" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

archive="teseladb-$VERSION-$PLATFORM.tar.gz"
mkdir -p "$WORK/site/v$VERSION" "$WORK/pack"
cp "$BINARY" "$WORK/pack/teseladb"
tar -czf "$WORK/site/v$VERSION/$archive" -C "$WORK/pack" teseladb
(cd "$WORK/site/v$VERSION" && { sha256sum "$archive" 2>/dev/null || shasum -a 256 "$archive"; } >"$archive.sha256")
echo "$VERSION" >"$WORK/site/latest.txt"
PORT=$((38000 + RANDOM % 1000))
(cd "$WORK/site" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER=$!
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$PORT/latest.txt" 2>/dev/null && break; sleep 0.1; done

export TESELADB_DOWNLOADS_URL="http://127.0.0.1:$PORT"
TESELADB_INSTALL_DIR="$WORK/ok" sh "$HERE/install.sh"
[[ $("$WORK/ok/teseladb" --version) == "teseladb $VERSION" ]] || { echo "test-install: wrong binary installed" >&2; exit 1; }

echo "0000 $archive" >"$WORK/site/v$VERSION/$archive.sha256"
if TESELADB_INSTALL_DIR="$WORK/bad" sh "$HERE/install.sh" 2>/dev/null; then
  echo "test-install: a bad checksum was installed" >&2
  exit 1
fi
[[ ! -e $WORK/bad/teseladb ]] || { echo "test-install: a binary was left after a bad checksum" >&2; exit 1; }
echo "test-install: install.sh installs $PLATFORM and refuses a bad checksum"
