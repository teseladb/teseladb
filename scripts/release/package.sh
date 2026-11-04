#!/usr/bin/env bash
# Packs the release binaries the workflow cross-compiled: one teseladb-<version>-<platform>.tar.gz per platform, with the
# binary, LICENSE and COMMERCIAL.md at its root, and its .sha256 in the format of sha256sum. Also writes latest.txt, the
# install script and the license files at the root of the downloads.
#   scripts/release/package.sh <version> <target dir> <out dir>
# <target dir> is the CARGO_TARGET_DIR of the build, with <triple>/dist/teseladb for each triple below.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: scripts/release/package.sh <version> <target dir> <out dir>" >&2
  exit 2
fi
VERSION=${1#v}
TARGET_DIR=$2
OUT=$3
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

# platform name of the downloads = Rust triple of the build
PLATFORMS=(
  "linux-x86_64=x86_64-unknown-linux-gnu"
  "linux-aarch64=aarch64-unknown-linux-gnu"
  "macos-aarch64=aarch64-apple-darwin"
)

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}

mkdir -p "$OUT/v$VERSION"
for entry in "${PLATFORMS[@]}"; do
  platform=${entry%%=*}
  triple=${entry#*=}
  binary="$TARGET_DIR/$triple/dist/teseladb"
  [[ -x $binary ]] || { echo "package: $binary is missing" >&2; exit 1; }
  archive="teseladb-$VERSION-$platform.tar.gz"
  stage=$(mktemp -d)
  cp "$binary" "$stage/teseladb"
  cp "$ROOT/LICENSE" "$ROOT/COMMERCIAL.md" "$stage/"
  tar -czf "$OUT/v$VERSION/$archive" -C "$stage" teseladb LICENSE COMMERCIAL.md
  rm -rf "$stage"
  (cd "$OUT/v$VERSION" && sha256 "$archive" >"$archive.sha256")
  echo "packed $archive"
done
echo "$VERSION" >"$OUT/latest.txt"
cp "$HERE/install.sh" "$OUT/install.sh"
cp "$ROOT/LICENSE" "$ROOT/COMMERCIAL.md" "$OUT/"
