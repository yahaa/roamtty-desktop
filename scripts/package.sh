#!/usr/bin/env bash
# Packages built binaries into dist/: <target>.gz plus manifest.json with the SHA-256 of each
# unpacked binary. Usage: scripts/package.sh <version> <target>=<path-to-binary>...
set -euo pipefail
ver="$1"; shift
mkdir -p dist
entries=()
for pair in "$@"; do
  target="${pair%%=*}"; bin="${pair#*=}"
  gzip -9 -n -c "$bin" > "dist/$target.gz"
  sha=$(sha256sum "$bin" | cut -d' ' -f1)
  size=$(stat -c %s "$bin")
  entries+=("\"$target\":{\"file\":\"$target.gz\",\"sha256\":\"$sha\",\"size\":$size}")
done
IFS=,; targets="${entries[*]}"; unset IFS
printf '{"version":"%s","proto":{"min":1,"max":1},"openh264":"2.6.0","targets":{%s}}\n' "$ver" "$targets" > dist/manifest.json
cat dist/manifest.json
