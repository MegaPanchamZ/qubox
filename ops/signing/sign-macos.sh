#!/usr/bin/env bash
# codesign + notarize macOS binaries. Requires Developer ID + notary profile.
set -euo pipefail
IDENTITY="${QUBOX_CODESIGN_IDENTITY:-}"
PROFILE="${QUBOX_NOTARY_PROFILE:-}"
BIN="${1:-}"

if [[ -z "$IDENTITY" || -z "$BIN" ]]; then
  echo "usage: QUBOX_CODESIGN_IDENTITY=... QUBOX_NOTARY_PROFILE=... $0 <binary-or-app>" >&2
  exit 2
fi

codesign --force --options runtime --sign "$IDENTITY" --timestamp "$BIN"
if [[ -n "$PROFILE" ]]; then
  ditto -c -k --keepParent "$BIN" /tmp/qubox-notarize.zip
  xcrun notarytool submit /tmp/qubox-notarize.zip --keychain-profile "$PROFILE" --wait
  xcrun stapler staple "$BIN" || true
fi
echo "signed $BIN"
