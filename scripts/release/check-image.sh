#!/usr/bin/env bash
# Checks an image saved with `docker save`: every layer, uncompressed, hashes to the diff_id its config declares in
# the same position. The classic builder once put the base layer of one platform into the image of the other when the
# base was pinned by the digest of its multi-platform index, and the registry then refused the pull of that platform
# with «wrong diff id» (viuzi/teseladb:0.1.0 on arm64). publish.sh runs it on each image before it pushes it.
#   scripts/release/check-image.sh <archive.tar>
set -euo pipefail

archive=${1:?usage: check-image.sh <archive of docker save>}
[[ -f $archive ]] || { echo "check-image: $archive does not exist" >&2; exit 1; }
work=$(mktemp -d)
trap 'find "$work" -depth -delete' EXIT
tar -xf "$archive" -C "$work"

config=$(jq -r '.[0].Config' "$work/manifest.json")
layers=()
while IFS= read -r line; do layers+=("$line"); done < <(jq -r '.[0].Layers[]' "$work/manifest.json")
declared=()
while IFS= read -r line; do declared+=("$line"); done < <(jq -r '.rootfs.diff_ids[]' "$work/$config")
if [[ ${#layers[@]} -ne ${#declared[@]} ]]; then
  echo "check-image: $archive has ${#layers[@]} layers and its config declares ${#declared[@]}" >&2
  exit 1
fi

for i in "${!layers[@]}"; do
  file=$work/${layers[$i]}
  # A layer saved by the containerd store is compressed with gzip; its diff_id is the hash of the uncompressed tar.
  if [[ $(head -c 2 "$file" | od -An -tx1 | tr -d ' \n') == 1f8b ]]; then
    got=sha256:$(gzip -dc "$file" | sha256sum | cut -d' ' -f1)
  else
    got=sha256:$(sha256sum "$file" | cut -d' ' -f1)
  fi
  if [[ $got != "${declared[$i]}" ]]; then
    echo "check-image: $archive layer $((i + 1)) hashes to $got, its config declares ${declared[$i]}" >&2
    exit 1
  fi
done
echo "check-image: $archive: ${#layers[@]} layers match their diff_ids"
