#!/usr/bin/env bash
# uninstall-macos.sh — remove launchd plist (macOS)
# Usage: ./uninstall-macos.sh

set -euo pipefail

PLIST_DST="/Library/LaunchDaemons/com.qubox.daemon.plist"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: uninstall-macos.sh requires root." >&2
    exit 1
fi

if [[ -f "$PLIST_DST" ]]; then
    launchctl unload "$PLIST_DST" 2>/dev/null || true
    rm -f "$PLIST_DST"
    echo "removed com.qubox.daemon.plist"
fi

echo "service removed"
