#!/usr/bin/env bash
# Publishes a release from the direct release of the operations repository, already inside AWS with the role of the
# stage and with the CDK of the downloads copied to cdk/ of the tree it runs in: builds the binary of each platform in a container of its architecture, packs it, checks the
# install script against it, deploys the downloads bucket and its CloudFront with CDK, copies the release there, builds
# one image per architecture and installs the release from the public host to check it. Only production, a release of
# main, pushes the images to Docker Hub, joined under the version and :latest; development builds and runs them locally.
#   scripts/release/publish.sh <version> <stage dev|prd>
# PGO_SECONDS (default 120) sets how long each workload of the profiles runs.
# Reads BUILD_IMAGE_AMD64 and BUILD_IMAGE_ARM64 (the build image of each architecture, with glibc 2.28), IMAGE,
# DOCKERHUB_USER and AWS_ACCOUNT_ID from its caller, and DOCKERHUB_TOKEN in production.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/release/publish.sh <version> <dev|prd>" >&2
  exit 2
fi
VERSION=$1
STAGE=$2
: "${BUILD_IMAGE_AMD64:?}" "${BUILD_IMAGE_ARM64:?}" "${IMAGE:?}" "${DOCKERHUB_USER:?}" "${AWS_ACCOUNT_ID:?}"
case "$STAGE" in
  prd) HOST=downloads.teseladb.com ;;
  dev) HOST=downloads.teseladb.dev.atbionapps.com ;;
  *) echo "publish: unknown stage $STAGE" >&2; exit 2 ;;
esac
BUCKET="s3://teseladb-downloads-${STAGE}-${AWS_ACCOUNT_ID}"
TRIPLES=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)
VOLUME="teseladb-release-$VERSION-$$"

step() { echo "== $*"; }

step "build"
# Each binary is built in a container of its own architecture, natively, in an image whose glibc
# is the oldest the release runs on (2.28: Debian 10, RHEL 8 and later): the engine stands in
# front of glibc's allocator and of the unwinder, so it is linked as it is tested, by the
# system's compiler against the system's libraries (023 FR-007). The architecture of this
# machine also runs the load that profiles the build; the other one is emulated by the Docker
# engine and takes the same profile.
case "$(docker version --format '{{.Server.Arch}}')" in
  amd64 | x86_64) NATIVE=amd64 OTHER=arm64 ;;
  arm64 | aarch64) NATIVE=arm64 OTHER=amd64 ;;
  *) echo "publish: no build image for the architecture of this Docker engine" >&2; exit 1 ;;
esac
image_of() {
  case $1 in
    amd64) echo "$BUILD_IMAGE_AMD64" ;;
    arm64) echo "$BUILD_IMAGE_ARM64" ;;
  esac
}
TOOLCHAIN=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
# The checkout lives in the job container, so the build containers get it through a volume of the host.
docker volume create "$VOLUME" >/dev/null
trap 'docker volume rm -f "$VOLUME" >/dev/null 2>&1 || true' EXIT
tar -cf - --exclude=./target --exclude=./.git . |
  docker run --rm -i --platform "linux/$NATIVE" -v "$VOLUME:/work" "$(image_of "$NATIVE")" \
    sh -c 'mkdir -p /work/src && tar -xf - -C /work/src'
# The steps of scripts/release/pgo.sh in the build image of one architecture, with the toolchain
# of rust-toolchain.toml from rustup. The binary carries the release's full version
# (0.1.0-dev.N in development), which test-install.sh checks.
build_in() {
  local arch=$1
  shift
  docker run --rm --platform "linux/$arch" -v "$VOLUME:/work" -w /work/src \
    -e CARGO_HOME="/work/cargo-$arch" -e RUSTUP_HOME="/work/rustup-$arch" -e CARGO_TARGET_DIR="/work/target-$arch" \
    -e CARGO_PROFILE_DIST_STRIP=debuginfo -e TESELADB_RELEASE_VERSION="$VERSION" -e PGO_SECONDS -e PGO_GLIBC=2.28 \
    "$(image_of "$arch")" bash -euo pipefail -c "
      curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain '$TOOLCHAIN' >/dev/null
      export PATH=\"\$CARGO_HOME/bin:\$PATH\"
      scripts/release/pgo.sh $*"
}
build_in "$NATIVE" profile build
build_in "$OTHER" build
mkdir -p target-release
docker run --rm --platform "linux/$NATIVE" -v "$VOLUME:/work" "$(image_of "$NATIVE")" tar -cf - -C /work/out . | tar -xf - -C target-release
for triple in "${TRIPLES[@]}"; do [[ -x target-release/$triple/dist/teseladb ]] || { echo "publish: $triple is missing" >&2; exit 1; }; done

step "pack and check the install script"
scripts/release/package.sh "$VERSION" target-release dist/downloads
scripts/release/test-install.sh target-release/x86_64-unknown-linux-gnu/dist/teseladb "$VERSION"

step "deploy the downloads bucket and its CloudFront"
(cd cdk && STAGE="$STAGE" cdk deploy --all --require-approval never)

step "copy the release to $BUCKET"
# The archives of a version never change; install.sh and latest.txt change with each release.
aws s3 cp "dist/downloads/v$VERSION" "$BUCKET/v$VERSION" --recursive --cache-control "public, max-age=31536000, immutable"
sed "s#https://downloads.teseladb.com#https://$HOST#" dist/downloads/install.sh >dist/downloads/install.stage.sh
aws s3 cp dist/downloads/install.stage.sh "$BUCKET/install.sh" --content-type "text/x-shellscript; charset=utf-8" --cache-control "public, max-age=300"
aws s3 cp dist/downloads/latest.txt "$BUCKET/latest.txt" --content-type "text/plain; charset=utf-8" --cache-control "public, max-age=300"
for file in LICENSE COMMERCIAL.md; do
  aws s3 cp "dist/downloads/$file" "$BUCKET/$file" --content-type "text/plain; charset=utf-8" --cache-control "public, max-age=300"
done

step "build the image of each architecture"
# debian:bookworm-slim by the digest of each platform, from the index sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3:
# the classic builder mixes the layers of the platforms of an index pinned by its own digest (docker/release.Dockerfile).
base_of() {
  case $1 in
    amd64) echo debian@sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e ;;
    arm64) echo debian@sha256:7ccb8b5b60c7654f5b795d94ee7d751cd0811500803f0b09cefcd555e755a55a ;;
  esac
}
for pair in amd64=x86_64-unknown-linux-gnu arm64=aarch64-unknown-linux-gnu; do
  arch=${pair%%=*}
  triple=${pair#*=}
  mkdir -p "dist/image/$arch/data"
  install -m 0755 "target-release/$triple/dist/teseladb" "dist/image/$arch/teseladb"
  cp LICENSE COMMERCIAL.md "dist/image/$arch/"
  DOCKER_BUILDKIT=0 docker build --platform "linux/$arch" --build-arg "BASE=$(base_of "$arch")" \
    -f docker/release.Dockerfile -t "$IMAGE:$VERSION-$arch" "dist/image/$arch"
  # Each layer must be the one its config declares, or the registry refuses the pull of this platform.
  docker save "$IMAGE:$VERSION-$arch" -o "dist/image/$arch.tar"
  scripts/release/check-image.sh "dist/image/$arch.tar"
  rm -f "dist/image/$arch.tar"
done

if [[ $STAGE == prd ]]; then
  step "push the images to Docker Hub as $IMAGE:$VERSION and :latest"
  : "${DOCKERHUB_TOKEN:?the Docker Hub token is missing}"
  printf '%s' "$DOCKERHUB_TOKEN" | docker login --username "$DOCKERHUB_USER" --password-stdin
  for arch in amd64 arm64; do docker push "$IMAGE:$VERSION-$arch"; done
  for tag in "$VERSION" latest; do
    docker manifest rm "$IMAGE:$tag" 2>/dev/null || true
    docker manifest create "$IMAGE:$tag" "$IMAGE:$VERSION-amd64" "$IMAGE:$VERSION-arm64"
    docker manifest push "$IMAGE:$tag"
  done
  docker logout >/dev/null
else
  echo "development: the images stay local, Docker Hub only gets the releases of main"
fi

step "install the release from https://$HOST and run the image"
for _ in $(seq 1 30); do curl -fsS -o /dev/null "https://$HOST/latest.txt" && break; sleep 10; done
[[ $(curl -fsSL "https://$HOST/latest.txt") == "$VERSION" ]] || { echo "https://$HOST/latest.txt does not say $VERSION" >&2; exit 1; }
curl -fsSL "https://$HOST/install.sh" | TESELADB_INSTALL_DIR="$PWD/installed" sh
[[ $(./installed/teseladb --version) == "teseladb $VERSION" ]] || { echo "the installed binary is not $VERSION" >&2; exit 1; }
# The runner is amd64, so that is the image it runs: the one of Docker Hub in production, the local one in development.
RUN_IMAGE="$IMAGE:$VERSION-amd64"
if [[ $STAGE == prd ]]; then
  platforms=$(docker manifest inspect "$IMAGE:$VERSION" | jq -r '[.manifests[].platform.architecture] | sort | join(" ")')
  [[ $platforms == "amd64 arm64" ]] || { echo "$IMAGE:$VERSION has $platforms, not amd64 and arm64" >&2; exit 1; }
  RUN_IMAGE="$IMAGE:$VERSION"
fi
[[ $(docker run --rm --platform linux/amd64 "$RUN_IMAGE" --version) == "teseladb $VERSION" ]] || { echo "$RUN_IMAGE does not run $VERSION" >&2; exit 1; }
echo "release $VERSION published to $STAGE: https://$HOST$([[ $STAGE == prd ]] && echo " and $IMAGE:$VERSION")"
