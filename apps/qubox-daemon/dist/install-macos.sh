#!/usr/bin/env bash
# install-macos.sh — install launchd plist (macOS)
# Usage: ./install-macos.sh

set -euo pipefail

DIST_DIR="$(cd "$(dirname "$0")" && pwd)"
PLIST_SRC="$DIST_DIR/qubox.plist"
PLIST_DST="/Library/LaunchDaemons/com.qubox.daemon.plist"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: install-macos.sh requires root." >&2
    exit 1
fi

cp "$PLIST_SRC" "$PLIST_DST"
echo "installed com.qubox.daemon.plist"

launchctl load "$PLIST_DST"
echo "service loaded"
