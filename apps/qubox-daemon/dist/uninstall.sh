#!/usr/bin/env bash
# uninstall.sh — remove systemd service (Linux)
# Usage: ./uninstall.sh [PREFIX=/]

set -euo pipefail
PREFIX="${1:-/}"

if command -v systemctl &>/dev/null && [[ "$PREFIX" == "/" ]]; then
    systemctl stop qubox.service 2>/dev/null || true
    systemctl disable qubox.service 2>/dev/null || true
    systemctl stop qubox.socket 2>/dev/null || true
    systemctl disable qubox.socket 2>/dev/null || true
fi

SYSTEMD_DIR="${PREFIX}/etc/systemd/system"
rm -f "$SYSTEMD_DIR/qubox.service"
rm -f "$SYSTEMD_DIR/qubox.socket"
echo "removed qubox.service"
echo "removed qubox.socket"

if command -v systemctl &>/dev/null && [[ "$PREFIX" == "/" ]]; then
    systemctl daemon-reload
fi
echo "service removed"
