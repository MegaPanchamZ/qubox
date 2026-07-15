#!/usr/bin/env bash
# build-msi.sh — Build the Windows MSI installer for Qubox Daemon.
#
# Prerequisites:
#   - WiX Toolset v4+ installed and `wix` on PATH
#     (https://wixtoolset.org/docs/intro/)
#   - qubox.exe built for x86_64-pc-windows-gnu or msvc
#     (cargo build --release --target x86_64-pc-windows-gnu -p qubox-daemon)
#   - This script runs on Windows (or Linux with Mono + WiX, though WiX 4
#     is .NET 6+ so Linux WixToolset is experimental).
#
# Usage:
#   ./apps/daemon/dist/build-msi.sh [--release-dir <path>]
#
# Examples:
#   ./apps/daemon/dist/build-msi.sh
#   ./apps/daemon/dist/build-msi.sh --release-dir ../target/x86_64-pc-windows-gnu/release
#
# Output:
#   QuboxDaemon-0.1.0-x64.msi

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Default: Linux MinGW cross-compile output path
RELEASE_DIR="${RELEASE_DIR:-$PROJECT_ROOT/target/x86_64-pc-windows-gnu/release}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release-dir) RELEASE_DIR="$2" ; shift 2 ;;
        *) echo "unknown: $1" >&2 ; exit 1 ;;
    esac
done

WXS_FILE="$SCRIPT_DIR/qubox.wxs"
MSI_OUT="${MSI_OUT:-QuboxDaemon-0.1.0-x64.msi}"

if [ ! -f "$RELEASE_DIR/qubox.exe" ]; then
    echo "[!] qubox.exe not found in $RELEASE_DIR"
    echo "    Build it first: cargo build --release --target x86_64-pc-windows-gnu -p qubox-daemon"
    exit 1
fi

if ! command -v wix &>/dev/null; then
    echo "[!] 'wix' CLI not found. Install WiX Toolset v4+ from https://wixtoolset.org/"
    exit 1
fi

echo "[*] Building MSI from $WXS_FILE"
echo "[*] Release dir: $RELEASE_DIR"
echo "[*] Output:      $MSI_OUT"

wix build "$WXS_FILE" \
    -arch x64 \
    -out "$MSI_OUT" \
    -d ReleaseDir="$RELEASE_DIR"

echo "[+] Done: $MSI_OUT"
