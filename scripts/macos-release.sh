#!/usr/bin/env bash
# Builds the macOS release on a Mac: a universal roamtty-desktop inside `Roamtty Desktop.app`,
# signed with Developer ID (hardened runtime, secure timestamp), notarized, stapled, zipped.
# Usage: scripts/macos-release.sh <version>
#   SIGN_IDENTITY   default: the first "Developer ID Application" identity in the keychain
#   ASC_KEY_ID, ASC_ISSUER, ASC_KEY_FILE   App Store Connect API key for notarytool; by default read
#                   from ~/.appstoreconnect/private_keys (app-dev-secret.txt + AuthKey_<id>.p8)
# Over SSH the login keychain is locked, so signing runs as a one-shot LaunchAgent job in the
# user's GUI session, where it is unlocked. Output: dist/macos-universal.zip
set -euo pipefail
ver="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
work="$(mktemp -d /tmp/rd-release.XXXXXX)"
app="$work/Roamtty Desktop.app"
mkdir -p "$app/Contents/MacOS"
lipo -create -output "$app/Contents/MacOS/roamtty-desktop" \
  target/aarch64-apple-darwin/release/roamtty-desktop target/x86_64-apple-darwin/release/roamtty-desktop
"$app/Contents/MacOS/roamtty-desktop" --version | grep -q " $ver " || { echo "binary is not $ver" >&2; exit 1; }
"$app/Contents/MacOS/roamtty-desktop" print-info-plist > "$app/Contents/Info.plist"

identity="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -nE 's/.*"(Developer ID Application: [^"]+)".*/\1/p' | head -1)}"
[ -n "$identity" ] || { echo "no Developer ID Application identity" >&2; exit 1; }
sign=(codesign --force --options runtime --timestamp -s "$identity" "$app")
if [ -n "${SSH_CONNECTION:-}" ]; then
  { echo '#!/bin/bash'; printf '%q ' "${sign[@]}"; printf '> %q 2>&1; echo "exit=$?" >> %q\n' "$work/sign.log" "$work/sign.log"; } > "$work/sign.sh"
  chmod +x "$work/sign.sh"
  cat > "$work/sign.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>com.roamtty.release-sign</string><key>ProgramArguments</key><array><string>$work/sign.sh</string></array><key>RunAtLoad</key><true/></dict></plist>
PLIST
  launchctl bootout "gui/$(id -u)/com.roamtty.release-sign" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$work/sign.plist"
  for _ in $(seq 1 120); do grep -q '^exit=' "$work/sign.log" 2>/dev/null && break; sleep 0.5; done
  launchctl bootout "gui/$(id -u)/com.roamtty.release-sign" 2>/dev/null || true
  cat "$work/sign.log"; grep -q '^exit=0' "$work/sign.log"
else
  "${sign[@]}"
fi
codesign --verify --strict "$app"

secret=~/.appstoreconnect/private_keys/app-dev-secret.txt
kid="${ASC_KEY_ID:-$(sed -nE 's/.*secretID:[[:space:]]*([A-Za-z0-9]+).*/\1/p' "$secret")}"
iss="${ASC_ISSUER:-$(sed -nE 's/.*issuerID:[[:space:]]*([0-9a-f-]+).*/\1/p' "$secret")}"
key="${ASC_KEY_FILE:-$HOME/.appstoreconnect/private_keys/AuthKey_$kid.p8}"
ditto -c -k --keepParent "$app" "$work/notarize.zip"
xcrun notarytool submit "$work/notarize.zip" --key "$key" --key-id "$kid" --issuer "$iss" --wait --timeout 20m
xcrun stapler staple "$app"
spctl -a -t exec -vv "$app"
mkdir -p dist
rm -f dist/macos-universal.zip
ditto -c -k --keepParent "$app" dist/macos-universal.zip
shasum -a 256 dist/macos-universal.zip
