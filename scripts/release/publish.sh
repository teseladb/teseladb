#!/usr/bin/env bash
# Publishes a release from the deploy workflows (.forgejo/workflows/deploy.yml and deploy-production.yml), already inside
# AWS with the role of the stage: cross-compiles the binary for each platform with cargo-zigbuild, packs it, checks the
# install script against it, deploys the downloads bucket and its CloudFront with CDK, copies the release there, builds
# one image per architecture and installs the release from the public host to check it. Only production, a release of
# main, pushes the images to Docker Hub, joined under the version and :latest; development builds and runs them locally.
#   scripts/release/publish.sh <version> <stage dev|prd>
# Reads ZIGBUILD_IMAGE, IMAGE, DOCKERHUB_USER and AWS_ACCOUNT_ID from the workflow, and DOCKERHUB_TOKEN in production,
# which the workflow reads from Vault (scripts/ci/dockerhub-token.sh).
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/release/publish.sh <version> <dev|prd>" >&2
  exit 2
fi
VERSION=$1
STAGE=$2
: "${ZIGBUILD_IMAGE:?}" "${IMAGE:?}" "${DOCKERHUB_USER:?}" "${AWS_ACCOUNT_ID:?}"
case "$STAGE" in
  prd) HOST=downloads.teseladb.com ;;
  dev) HOST=downloads.teseladb.dev.atbionapps.com ;;
  *) echo "publish: unknown stage $STAGE" >&2; exit 2 ;;
esac
BUCKET="s3://teseladb-downloads-${STAGE}-${AWS_ACCOUNT_ID}"
TRIPLES=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin)
VOLUME="teseladb-release-$VERSION-$$"

step() { echo "== $*"; }

step "cross-compile"
# The checkout lives in the job container, so the build container gets it through a volume of the host.
docker volume create "$VOLUME" >/dev/null
trap 'docker volume rm -f "$VOLUME" >/dev/null 2>&1 || true' EXIT
# The viuzi-* crates go two levels above the code, as next to the repository (001 FR-023).
tar -cf - --exclude=./target --exclude=./.git . | docker run --rm -i -v "$VOLUME:/work" "$ZIGBUILD_IMAGE" sh -c 'mkdir -p /work/repo/src && tar -xf - -C /work/repo/src'
tar -cf - -C ../../shared-crates --exclude=./target --exclude=./.git . |
  docker run --rm -i -v "$VOLUME:/work" "$ZIGBUILD_IMAGE" sh -c 'mkdir -p /work/shared-crates && tar -xf - -C /work/shared-crates'
docker run --rm -v "$VOLUME:/work" -w /work/repo/src -e CARGO_TARGET_DIR=/work/target -e CARGO_PROFILE_DIST_STRIP=debuginfo "$ZIGBUILD_IMAGE" sh -c '
  set -e
  rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin
  # glibc 2.28 (Debian 10, RHEL 8 and later) for the Linux binaries.
  for target in x86_64-unknown-linux-gnu.2.28 aarch64-unknown-linux-gnu.2.28 aarch64-apple-darwin; do
    cargo zigbuild --locked --profile dist -p teseladb-server --bin teseladb --target "$target"
  done
  for triple in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin; do
    mkdir -p "/work/out/$triple/dist" && cp "/work/target/$triple/dist/teseladb" "/work/out/$triple/dist/teseladb"
  done'
mkdir -p target-release
docker run --rm -v "$VOLUME:/work" "$ZIGBUILD_IMAGE" tar -cf - -C /work/out . | tar -xf - -C target-release
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
for pair in amd64=x86_64-unknown-linux-gnu arm64=aarch64-unknown-linux-gnu; do
  arch=${pair%%=*}
  triple=${pair#*=}
  mkdir -p "dist/image/$arch/data"
  install -m 0755 "target-release/$triple/dist/teseladb" "dist/image/$arch/teseladb"
  cp LICENSE COMMERCIAL.md "dist/image/$arch/"
  DOCKER_BUILDKIT=0 docker build --platform "linux/$arch" -f docker/release.Dockerfile -t "$IMAGE:$VERSION-$arch" "dist/image/$arch"
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
