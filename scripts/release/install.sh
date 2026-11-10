#!/bin/sh
# Installs the teseladb binary: picks the archive for this system and CPU, checks its SHA-256 and copies the binary to
# /usr/local/bin, or to TESELADB_INSTALL_DIR when it is set. It installs the newest release (latest.txt), or
# TESELADB_VERSION when it is set.
#   curl -fsSL https://downloads.teseladb.com/install.sh | sh
set -eu

DOWNLOADS_URL="${TESELADB_DOWNLOADS_URL:-https://downloads.teseladb.com}"
INSTALL_DIR="${TESELADB_INSTALL_DIR:-/usr/local/bin}"

fail() {
  echo "teseladb: $1" >&2
  exit 1
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) PLATFORM="linux-x86_64" ;;
  Linux-aarch64 | Linux-arm64) PLATFORM="linux-aarch64" ;;
  Darwin-arm64) PLATFORM="macos-aarch64" ;;
  *) fail "there is no binary for $(uname -s) $(uname -m); run it with Docker: docker run teseladb/teseladb:latest" ;;
esac

# The Linux binaries are built for glibc 2.28 or later: a system without glibc (musl, as in Alpine) or with an older one
# cannot run them, so it gets the same answer as an unknown platform before anything is downloaded.
if [ "$(uname -s)" = Linux ]; then
  GLIBC="$(getconf GNU_LIBC_VERSION 2>/dev/null | sed -n 's/^glibc //p')" || GLIBC=""
  [ -n "$GLIBC" ] ||
    fail "there is no binary for Linux $(uname -m) without glibc; run it with Docker: docker run teseladb/teseladb:latest"
  GLIBC_MAJOR="${GLIBC%%.*}"
  GLIBC_MINOR="${GLIBC#*.}"
  GLIBC_MINOR="${GLIBC_MINOR%%.*}"
  if [ "$GLIBC_MAJOR" -lt 2 ] || { [ "$GLIBC_MAJOR" -eq 2 ] && [ "$GLIBC_MINOR" -lt 28 ]; }; then
    fail "there is no binary for Linux $(uname -m) with glibc $GLIBC, it needs 2.28 or later; run it with Docker: docker run teseladb/teseladb:latest"
  fi
fi

command -v curl >/dev/null 2>&1 || fail "curl is needed to download the binary"
VERSION="${TESELADB_VERSION:-$(curl -fsSL "$DOWNLOADS_URL/latest.txt")}" || fail "could not read the newest version"
VERSION="${VERSION#v}"
ARCHIVE="teseladb-$VERSION-$PLATFORM.tar.gz"
URL="$DOWNLOADS_URL/v$VERSION/$ARCHIVE"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "Downloading $ARCHIVE"
curl -fsSL "$URL" -o "$WORK/$ARCHIVE" || fail "could not download $URL"
curl -fsSL "$URL.sha256" -o "$WORK/$ARCHIVE.sha256" || fail "could not download $URL.sha256"

EXPECTED="$(cut -d ' ' -f 1 "$WORK/$ARCHIVE.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$WORK/$ARCHIVE" | cut -d ' ' -f 1)"
else
  ACTUAL="$(shasum -a 256 "$WORK/$ARCHIVE" | cut -d ' ' -f 1)"
fi
[ "$EXPECTED" = "$ACTUAL" ] || fail "the SHA-256 of $ARCHIVE does not match; nothing was installed"

tar -xzf "$WORK/$ARCHIVE" -C "$WORK"
mkdir -p "$INSTALL_DIR"
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$WORK/teseladb" "$INSTALL_DIR/teseladb"
else
  sudo install -m 0755 "$WORK/teseladb" "$INSTALL_DIR/teseladb"
fi
echo "teseladb $VERSION installed in $INSTALL_DIR/teseladb"
