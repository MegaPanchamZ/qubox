#!/usr/bin/env bash
# install.sh — install systemd service (Linux)
# Usage: ./install.sh [PREFIX=/]

set -euo pipefail
PREFIX="${1:-/}"
DIST_DIR="$(cd "$(dirname "$0")" && pwd)"

SYSTEMD_DIR="${PREFIX}/etc/systemd/system"
mkdir -p "$SYSTEMD_DIR"

cp "$DIST_DIR/qubox.service" "$SYSTEMD_DIR/"
cp "$DIST_DIR/qubox.socket" "$SYSTEMD_DIR/"
echo "installed qubox.service"
echo "installed qubox.socket"

if ! command -v systemctl &>/dev/null; then
    echo "WARNING: systemctl not found; skipping systemd reload/enable/start."
    exit 0
fi

if [[ "$PREFIX" != "/" ]]; then
    echo "WARNING: non-default prefix; skipping systemctl operations."
    exit 0
fi

systemctl daemon-reload
systemctl enable qubox.service
systemctl enable qubox.socket
systemctl start qubox.service
echo "service started"
