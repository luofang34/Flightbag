#!/usr/bin/env bash
# Installs the Kubo release the publisher starts as a throwaway node when a pinning service is
# configured without a node of its own (see `scripts/ipfs-pin.sh`), and prints the directory
# holding `ipfs`. The archive is pinned by version and SHA-512: the node is handed the
# pinning-service token's counterpart, the published bytes, so it must be the reviewed binary.
#
#   scripts/install-kubo.sh <directory>
set -euo pipefail

version="v0.40.1"
archive="kubo_${version}_linux-amd64.tar.gz"
sha512="86f9c07bca62d09839e53553df0f81bc306364b1467a459d52f3e4505cc948d196ed0c219c03c667cd681e057bb24bbfc0e0eebf4b83bc64a36a0e9923a60e05"

dest="${1:?usage: install-kubo.sh <directory>}"
mkdir -p "$dest"
curl -fsSL --retry 3 --max-time 600 -o "$dest/$archive" \
  "https://github.com/ipfs/kubo/releases/download/$version/$archive"
if command -v sha512sum >/dev/null 2>&1; then
  actual="$(sha512sum "$dest/$archive" | cut -d' ' -f1)"
else
  actual="$(shasum -a 512 "$dest/$archive" | cut -d' ' -f1)"
fi
if [ "$actual" != "$sha512" ]; then
  echo "error: $archive has SHA-512 $actual, expected $sha512" >&2
  exit 1
fi
tar -xzf "$dest/$archive" -C "$dest" kubo/ipfs
echo "$dest/kubo"
