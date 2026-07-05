#!/usr/bin/env bash
set -euo pipefail

NODE_VERSION="${NODE_VERSION:-v26.4.0}"
DEST="src-tauri/resources/node"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) PLATFORM="darwin-arm64" ;;
  Darwin-x86_64) PLATFORM="darwin-x64" ;;
  Linux-x86_64) PLATFORM="linux-x64" ;;
  Linux-aarch64) PLATFORM="linux-arm64" ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TARBALL="node-${NODE_VERSION}-${PLATFORM}.tar.gz"
URL="https://nodejs.org/dist/${NODE_VERSION}/${TARBALL}"
SHASUMS_URL="https://nodejs.org/dist/${NODE_VERSION}/SHASUMS256.txt"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

echo "Downloading ${TARBALL}"
curl -fsSL -o "${workdir}/${TARBALL}" "$URL"
curl -fsSL -o "${workdir}/SHASUMS256.txt" "$SHASUMS_URL"

echo "Verifying checksum"
( cd "$workdir" && grep " ${TARBALL}$" SHASUMS256.txt | shasum -a 256 -c - )

echo "Extracting into ${DEST}"
rm -rf "$DEST"
mkdir -p "$DEST"
tar -xzf "${workdir}/${TARBALL}" -C "$workdir"
cp -R "${workdir}/node-${NODE_VERSION}-${PLATFORM}/." "$DEST/"

echo "Bundled Node: $("${DEST}/bin/node" --version)"
