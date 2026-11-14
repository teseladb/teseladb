#!/usr/bin/env bash
# Tests of scripts/release/github-release.sh with gh and docker played by local stand-ins that log their calls: a
# missing archive stops it before GitHub, a new release is created with the six files and the notes, an existing one
# gets them again with --clobber, the image goes to GHCR under the version and latest, and an image without both
# architectures fails.
#   scripts/tests/github_release_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/release/github-release.sh
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
passed=0
failed=0

# gh and docker log each call to $WORK/calls; GH_RELEASE_EXISTS and IMAGE_ARCHES choose what they answer.
mkdir -p "$WORK/bin"
cat >"$WORK/bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh $*" >>"$CALLS"
prev=""
case "$1 $2" in
  "release view")
    if [[ " $* " == *" --json "* ]]; then
      printf '%s\n' teseladb-0.2.0-linux-x86_64.tar.gz teseladb-0.2.0-linux-x86_64.tar.gz.sha256 \
        teseladb-0.2.0-linux-aarch64.tar.gz teseladb-0.2.0-linux-aarch64.tar.gz.sha256 \
        teseladb-0.2.0-macos-aarch64.tar.gz teseladb-0.2.0-macos-aarch64.tar.gz.sha256
      exit 0
    fi
    [[ ${GH_RELEASE_EXISTS:-no} == yes ]] ;;
  "release create" | "release edit")
    for arg in "$@"; do
      if [[ $prev == --notes-file ]]; then cp "$arg" "$NOTES_COPY"; fi
      prev=$arg
    done ;;
  "auth token") echo gh-token ;;
  "api user") echo octo ;;
esac
EOF
cat >"$WORK/bin/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >>"$CALLS"
if [[ $1 == login ]]; then read -r token; echo "token $token" >>"$CALLS"; fi
if [[ "$1 $2 $3" == "buildx imagetools inspect" ]]; then echo "${IMAGE_ARCHES:-amd64 arm64}"; fi
exit 0
EOF
chmod +x "$WORK/bin/gh" "$WORK/bin/docker"

dist() {
  rm -rf "$WORK/dist" && mkdir -p "$WORK/dist/v0.2.0"
  for p in linux-x86_64 linux-aarch64 macos-aarch64; do
    touch "$WORK/dist/v0.2.0/teseladb-0.2.0-$p.tar.gz" "$WORK/dist/v0.2.0/teseladb-0.2.0-$p.tar.gz.sha256"
  done
  printf -- '- Faster reads.\n' >"$WORK/notes.md"
}

# check <name> <expected status> <expected text in the output or the calls> [env...]
check() {
  local name=$1 want=$2 text=$3 status=0
  shift 3
  : >"$WORK/calls"
  env PATH="$WORK/bin:$PATH" CALLS="$WORK/calls" NOTES_COPY="$WORK/notes-sent.md" IMAGE=teseladb/teseladb "$@" \
    "$SCRIPT" 0.2.0 "$WORK/dist" "$WORK/notes.md" >"$WORK/out" 2>&1 || status=$?
  if [[ $status -eq $want ]] && grep -qF -- "$text" "$WORK/out" "$WORK/calls"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $name: status $status (want $want), no «${text}» in:" >&2
    cat "$WORK/out" "$WORK/calls" >&2
  fi
}

dist
rm "$WORK/dist/v0.2.0/teseladb-0.2.0-macos-aarch64.tar.gz.sha256"
check "a missing archive" 1 "teseladb-0.2.0-macos-aarch64.tar.gz.sha256 is missing"
if grep -q '^gh release' "$WORK/calls"; then
  failed=$((failed + 1)); echo "FAIL a missing archive reached GitHub" >&2
else
  passed=$((passed + 1))
fi

dist
check "a new release" 0 "gh release create v0.2.0 --repo teseladb/teseladb --target main --title TeselaDB 0.2.0"
check "the six files of a new release" 0 "teseladb-0.2.0-macos-aarch64.tar.gz.sha256 --latest"
if grep -qF -- '- Faster reads.' "$WORK/notes-sent.md" && grep -qF 'docker pull teseladb/teseladb:0.2.0' "$WORK/notes-sent.md"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1)); echo "FAIL the notes lack the changes or the image" >&2; cat "$WORK/notes-sent.md" >&2 || true
fi
if grep -qF 'Business Source License 1.1' "$WORK/notes-sent.md" &&
  grep -qF 'https://teseladb.com/docs/get-started/quickstart-single-node' "$WORK/notes-sent.md"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1)); echo "FAIL the notes lack the license or the quick start of the website" >&2
fi
check "an existing release" 0 "gh release upload v0.2.0 --repo teseladb/teseladb --clobber" GH_RELEASE_EXISTS=yes
check "the notes of an existing release" 0 "gh release edit v0.2.0 --repo teseladb/teseladb --title TeselaDB 0.2.0" GH_RELEASE_EXISTS=yes
check "the login of GHCR" 0 "token gh-token"
check "the image to GHCR" 0 "docker buildx imagetools create --annotation index:org.opencontainers.image.source=https://github.com/teseladb/teseladb -t ghcr.io/teseladb/teseladb:0.2.0 -t ghcr.io/teseladb/teseladb:latest teseladb/teseladb:0.2.0"
check "an image with one architecture" 1 "ghcr.io/teseladb/teseladb:0.2.0 has amd64, not amd64 and arm64" IMAGE_ARCHES=amd64

echo "github_release_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
