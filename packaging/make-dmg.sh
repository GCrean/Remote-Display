#!/usr/bin/env bash
# Build a drag-to-Applications .dmg from the .app that `cargo packager --formats app` produces.
# Uses hdiutil only (no Finder/AppleScript), so it works over SSH and on CI runners.
#
#   packaging/make-dmg.sh "dist/Remote Display.app" dist/remote-display_0.1.0_aarch64.dmg
#
# The app is ad-hoc signed (required for arm64 to launch). For distribution without Gatekeeper
# warnings, sign with a Developer ID and notarize instead (set CODESIGN_IDENTITY).
set -euo pipefail
APP="${1:?usage: make-dmg.sh <path/to/App.app> <output.dmg>}"
OUT="${2:?usage: make-dmg.sh <path/to/App.app> <output.dmg>}"
NAME="$(basename "$APP" .app)"
IDENTITY="${CODESIGN_IDENTITY:--}"

codesign --force --deep --sign "$IDENTITY" "$APP"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT"
hdiutil create -volname "$NAME" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
echo "created $OUT"
