#!/usr/bin/env bash
# Tests of scripts/release/check-image.sh against archives in the layout of `docker save`, built here without Docker:
# an image whose layers hash to the diff_ids of its config passes, compressed or not; one that carries a layer of
# another image (what the classic builder did with a multi-platform base pinned by the digest of its index) fails and
# names the layer.
#   scripts/tests/check_image_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/release/check-image.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
passed=0
failed=0

# archive <name> <compress: yes|no> <layer contents...>: a docker save archive whose config declares the diff_id of
# each content; with BAD=<n>, layer n is replaced by other bytes after the config is written.
archive() {
  local name=$1 compress=$2
  shift 2
  local dir=$WORK/$name
  mkdir -p "$dir/blobs/sha256"
  local diff_ids=() layers=() i=0
  for content in "$@"; do
    i=$((i + 1))
    printf '%s' "$content" > "$dir/layer.tar"
    diff_ids+=("\"sha256:$(sha256sum "$dir/layer.tar" | cut -d' ' -f1)\"")
    if [[ -n ${BAD:-} && $BAD == "$i" ]]; then printf '%s' "another image's layer $i" > "$dir/layer.tar"; fi
    if [[ $compress == yes ]]; then gzip -n -c "$dir/layer.tar" > "$dir/layer.out"; else mv "$dir/layer.tar" "$dir/layer.out"; fi
    local digest
    digest=$(sha256sum "$dir/layer.out" | cut -d' ' -f1)
    mv "$dir/layer.out" "$dir/blobs/sha256/$digest"
    layers+=("\"blobs/sha256/$digest\"")
    rm -f "$dir/layer.tar"
  done
  local ids layers_json
  ids=$(IFS=,; echo "${diff_ids[*]}")
  layers_json=$(IFS=,; echo "${layers[*]}")
  printf '{"architecture":"arm64","rootfs":{"type":"layers","diff_ids":[%s]}}' "$ids" > "$dir/config.json"
  local config
  config=$(sha256sum "$dir/config.json" | cut -d' ' -f1)
  mv "$dir/config.json" "$dir/blobs/sha256/$config"
  printf '[{"Config":"blobs/sha256/%s","Layers":[%s]}]' "$config" "$layers_json" > "$dir/manifest.json"
  tar -cf "$WORK/$name.tar" -C "$dir" manifest.json blobs
}

expect() {
  local what=$1 want=$2 archive=$3 needle=${4:-}
  local out status=0
  out=$("$SCRIPT" "$archive" 2>&1) || status=$?
  if [[ $want == pass && $status -eq 0 ]] || [[ $want == fail && $status -ne 0 && $out == *"$needle"* ]]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $what (exit $status): $out"
  fi
}

archive plain no "base" "binary" "licences"
expect "uncompressed layers that match pass" pass "$WORK/plain.tar"
archive packed yes "base" "binary"
expect "compressed layers are checked uncompressed" pass "$WORK/packed.tar"
BAD=1 archive mixed yes "base of arm64" "binary"
expect "a layer of another image fails and is named" fail "$WORK/mixed.tar" "layer 1"
expect "a missing archive fails" fail "$WORK/none.tar" "none.tar"

echo "check-image: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
