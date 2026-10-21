#!/usr/bin/env bash
# Packages built binaries into dist/ (<target>.gz, windows-*.exe, macos-*.zip) plus manifest.json
# with the SHA-256 of each unpacked binary, exe or zip. Usage: scripts/package.sh <version> <target>=<path-to-binary>...
set -euo pipefail
ver="$1"; shift
mkdir -p dist
entries=()
for pair in "$@"; do
  target="${pair%%=*}"; bin="${pair#*=}"
  case "$target" in
    # Windows hosts check a plain .exe with Get-FileHash; elsewhere gzip and the unpacked binary's hash.
    windows-*) file="$target.exe"; cp "$bin" "dist/$file" ;;
    macos-*) file="$target.zip"; cp "$bin" "dist/$file" ;;
    *) file="$target.gz"; gzip -9 -n -c "$bin" > "dist/$file" ;;
  esac
  sha=$( (sha256sum "$bin" 2>/dev/null || shasum -a 256 "$bin") | cut -d' ' -f1)
  size=$(wc -c < "$bin" | tr -d ' ')
  entries+=("\"$target\":{\"file\":\"$file\",\"sha256\":\"$sha\",\"size\":$size}")
done
IFS=,; targets="${entries[*]}"; unset IFS
printf '{"version":"%s","proto":{"min":1,"max":1},"openh264":"2.6.0","targets":{%s}}\n' "$ver" "$targets" > dist/manifest.json
cat dist/manifest.json
