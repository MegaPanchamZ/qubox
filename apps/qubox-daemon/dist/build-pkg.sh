#!/usr/bin/env bash
# build-pkg.sh — Build a macOS installer package (.pkg) for Qubox Daemon.
#
# This script uses pkgbuild + productbuild directly (no GUI tools required).
#
# Prerequisites:
#   - macOS with Xcode Command Line Tools (pkgbuild, productbuild)
#   - qubox compiled for macOS (universal or arm64/x86_64)
#   - This script MUST run on macOS; it is not cross-platform.
#
# Usage:
#   ./apps/daemon/dist/build-pkg.sh [--binary <path>] [--version <ver>]
#
# Output:
#   QuboxDaemon-<version>-universal.pkg

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# ---- defaults ----
VERSION="${VERSION:-0.1.0}"
BINARY="${BINARY:-$PROJECT_ROOT/target/release/qubox}"
PLIST="$SCRIPT_DIR/com.qubox.daemon.plist"
PKG_OUT="${PKG_OUT:-QuboxDaemon-${VERSION}-universal.pkg}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary) BINARY="$2" ; shift 2 ;;
        --version) VERSION="$2" ; shift 2 ;;
        *) echo "unknown: $1" >&2 ; exit 1 ;;
    esac
done

if [ ! -f "$BINARY" ]; then
    echo "[!] qubox binary not found at $BINARY"
    echo "    Build it: cargo build --release -p qubox-daemon"
    exit 1
fi

if ! command -v pkgbuild &>/dev/null || ! command -v productbuild &>/dev/null; then
    echo "[!] pkgbuild / productbuild not found. Install Xcode Command Line Tools."
    exit 1
fi

echo "[*] Building macOS package v$VERSION"
echo "[*] Binary: $BINARY"
echo "[*] Plist:  $PLIST"
echo "[*] Output: $PKG_OUT"

# ---- staging root ----
STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT

mkdir -p "$STAGING_DIR/usr/local/bin"
mkdir -p "$STAGING_DIR/Library/LaunchDaemons"

# install binary
install -m 0755 "$BINARY" "$STAGING_DIR/usr/local/bin/qubox"
# install plist
install -m 0644 "$PLIST" \
    "$STAGING_DIR/Library/LaunchDaemons/com.qubox.daemon.plist"

# ---- preinstall script (stop existing daemon) ----
mkdir -p "$STAGING_DIR/scripts"
cat > "$STAGING_DIR/scripts/preinstall" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
# stop any running instance before installing the new version
if [ -f /Library/LaunchDaemons/com.qubox.daemon.plist ]; then
    /bin/launchctl unload /Library/LaunchDaemons/com.qubox.daemon.plist 2>/dev/null || true
fi
# kill any running qubox process
/usr/bin/killall qubox 2>/dev/null || true
exit 0
SCRIPT
chmod 0755 "$STAGING_DIR/scripts/preinstall"

cat > "$STAGING_DIR/scripts/postinstall" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
# load the newly-installed daemon
/bin/launchctl load /Library/LaunchDaemons/com.qubox.daemon.plist
exit 0
SCRIPT
chmod 0755 "$STAGING_DIR/scripts/postinstall"

# ---- build component package ----
COMPONENT_PKG="$STAGING_DIR/QuboxComponent.pkg"
echo "[*] Building component package..."
pkgbuild \
    --root "$STAGING_DIR/usr" \
    --install-location "/usr" \
    --scripts "$STAGING_DIR/scripts" \
    --identifier "com.qubox.daemon.component" \
    --version "$VERSION" \
    --ownership recommended \
    "$COMPONENT_PKG"

# ---- build distribution (product) package ----
echo "[*] Building distribution package..."
productbuild \
    --package "$COMPONENT_PKG" \
    --identifier "com.qubox.daemon" \
    --version "$VERSION" \
    --sign "-" \
    "$PKG_OUT" 2>/dev/null || productbuild \
    --package "$COMPONENT_PKG" \
    --identifier "com.qubox.daemon" \
    --version "$VERSION" \
    "$PKG_OUT"

echo "[+] Done: $PKG_OUT"
