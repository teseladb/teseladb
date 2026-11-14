#!/usr/bin/env bash
# Publishes a production release on GitHub, the public repository of TeselaDB, once scripts/release/publish.sh has
# published it on the downloads host and Docker Hub: the release v<version> of GITHUB_REPO with the three archives and
# their .sha256, marked as the latest, and the image of Docker Hub copied to GHCR as <version> and latest. Run again on
# the same version, it uploads the files and the notes again and copies the same image.
#   scripts/release/github-release.sh <version> <downloads dir> [changes file]
# <downloads dir> is the out dir of scripts/release/package.sh, with v<version>/. The changes file, in Markdown, opens
# the notes of the release. Reads IMAGE, the image of Docker Hub, and enters GitHub with the login of gh or GH_TOKEN,
# which needs the scopes repo and write:packages.
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: scripts/release/github-release.sh <version> <downloads dir> [changes file]" >&2
  exit 2
fi
VERSION=$1
DIR=$2/v$1
CHANGES=${3:-}
: "${IMAGE:?the image of Docker Hub is missing}"
REPO=${GITHUB_REPO:-teseladb/teseladb}
GHCR=${GHCR_IMAGE:-ghcr.io/teseladb/teseladb}
TAG=v$VERSION
PLATFORMS=(linux-x86_64 linux-aarch64 macos-aarch64)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

fail() { echo "github-release: $*" >&2; exit 1; }
step() { echo "== $*"; }

files=()
for platform in "${PLATFORMS[@]}"; do
  for name in "teseladb-$VERSION-$platform.tar.gz" "teseladb-$VERSION-$platform.tar.gz.sha256"; do
    [[ -f $DIR/$name ]] || fail "$name is missing in $DIR"
    files+=("$DIR/$name")
  done
done
[[ -z $CHANGES || -r $CHANGES ]] || fail "the changes file $CHANGES cannot be read"

notes() {
  echo "TeselaDB $VERSION has binaries for Linux on x86_64 and aarch64 and for macOS on Apple Silicon. Its Docker image"
  echo "is on Docker Hub and on GitHub."
  echo
  if [[ -n $CHANGES ]]; then cat "$CHANGES" && echo; fi
  cat <<EOF
## Install

The install script picks the binary of your system, checks its SHA-256 and copies it to \`/usr/local/bin\`, and it
asks for your password through \`sudo\` when your user cannot write to that folder. With Docker, you pull the image of
this version from Docker Hub or from GitHub, whichever you prefer.

\`\`\`sh
curl -fsSL https://downloads.teseladb.com/install.sh | sh
# or, with Docker
docker pull $IMAGE:$VERSION
docker pull $GHCR:$VERSION
\`\`\`

The [quick start](https://teseladb.com/docs/get-started/quickstart-single-node) takes you from here to a running node with a
shell session on it in about five minutes.

## Files

| File | Platform |
| --- | --- |
| \`teseladb-$VERSION-linux-x86_64.tar.gz\` | Linux x86_64 |
| \`teseladb-$VERSION-linux-aarch64.tar.gz\` | Linux aarch64 |
| \`teseladb-$VERSION-macos-aarch64.tar.gz\` | macOS on Apple Silicon |

Each file has its \`.sha256\`. To check a download, run \`shasum -a 256 -c <file>.sha256\` in the folder where you
saved both files. TeselaDB is source-available under the
[Business Source License 1.1](https://github.com/$REPO/blob/main/LICENSE).
EOF
}

notes >"$WORK/notes.md"

if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  step "upload the files of $TAG to the existing release of $REPO"
  gh release upload "$TAG" --repo "$REPO" --clobber "${files[@]}"
  gh release edit "$TAG" --repo "$REPO" --title "TeselaDB $VERSION" --notes-file "$WORK/notes.md" --latest
else
  step "create the release $TAG of $REPO"
  gh release create "$TAG" --repo "$REPO" --target main --title "TeselaDB $VERSION" --notes-file "$WORK/notes.md" \
    "${files[@]}" --latest
fi
published=$(gh release view "$TAG" --repo "$REPO" --json assets -q '.assets[].name')
for file in "${files[@]}"; do
  grep -qxF "$(basename "$file")" <<<"$published" || fail "$TAG of $REPO lacks $(basename "$file")"
done

step "copy $IMAGE:$VERSION to $GHCR:$VERSION and :latest"
# A Docker config of its own, so the login of GHCR stays out of the operator's.
export DOCKER_CONFIG=$WORK/docker
mkdir -p "$DOCKER_CONFIG"
for plugins in "$HOME/.docker/cli-plugins" /usr/local/lib/docker/cli-plugins; do
  if [[ -d $plugins ]]; then ln -s "$plugins" "$DOCKER_CONFIG/cli-plugins" && break; fi
done
gh auth token | docker login "${GHCR%%/*}" --username "$(gh api user -q .login)" --password-stdin >/dev/null
# The source annotation links the package of GHCR to the repository, which shows it on the page of the repository.
docker buildx imagetools create --annotation "index:org.opencontainers.image.source=https://github.com/$REPO" \
  -t "$GHCR:$VERSION" -t "$GHCR:latest" "$IMAGE:$VERSION"
arches=$(docker buildx imagetools inspect "$GHCR:$VERSION" --format '{{range .Manifest.Manifests}}{{.Platform.Architecture}} {{end}}' |
  tr ' ' '\n' | grep -vx -e unknown -e '' | sort | xargs)
docker logout "${GHCR%%/*}" >/dev/null
[[ $arches == "amd64 arm64" ]] || fail "$GHCR:$VERSION has $arches, not amd64 and arm64"
echo "release $VERSION on https://github.com/$REPO/releases/tag/$TAG and $GHCR:$VERSION"
