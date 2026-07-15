#!/usr/bin/env bash
# build-appimage.sh — Build a portable Qubox AppImage for Linux.
#
# Prerequisites:
#   - cargo (Rust toolchain with x86_64-unknown-linux-gnu target)
#   - fuse2 / fuse3 (for running the AppImage; not needed for building)
#   - docker or podman (optional, for FUSE-less builds via appimagetool)
#
# Usage:
#   ./apps/daemon/dist/build-appimage.sh
#
# Output:
#   Qubox-<version>-x86_64.AppImage  (in CWD)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
APP_NAME="Qubox"
BINARY_NAME="qubox"
ARCH="x86_64"

# ---- extract version from workspace Cargo.toml ----
VERSION="$(grep -m1 '^version = ' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
echo "[*] Building AppImage for $APP_NAME v$VERSION ($ARCH)"
echo "[*] Project root: $PROJECT_ROOT"

# ---- 1. build the release binary ----
echo "[*] Building $BINARY_NAME (release)..."
cargo build --release --target "${ARCH}-unknown-linux-gnu" -p qubox-daemon \
    --manifest-path "$PROJECT_ROOT/Cargo.toml"

# ---- 2. create AppDir structure ----
APPDIR="$(mktemp -d)/${APP_NAME}.AppDir"
mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/lib/systemd/user"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/scalable/apps"

echo "[*] Populating AppDir..."

# binary
cp "$PROJECT_ROOT/target/${ARCH}-unknown-linux-gnu/release/$BINARY_NAME" \
    "$APPDIR/usr/bin/$BINARY_NAME"
chmod 0755 "$APPDIR/usr/bin/$BINARY_NAME"

# systemd units
cp "$SCRIPT_DIR/qubox.service" \
    "$APPDIR/usr/lib/systemd/user/qubox.service"
cp "$SCRIPT_DIR/qubox.socket" \
    "$APPDIR/usr/lib/systemd/user/qubox.socket"

# desktop file
cat > "$APPDIR/usr/share/applications/qubox.desktop" <<DESKTOP_EOF
[Desktop Entry]
Type=Application
Name=Qubox
Comment=Qubox background daemon — remote gaming/streaming
Exec=qubox
Icon=qubox
Terminal=false
Categories=Network;RemoteAccess;Game;
StartupNotify=false
DESKTOP_EOF

# symlink desktop + icon to AppDir root (required by AppImage spec)
ln -sf "usr/share/applications/qubox.desktop" \
    "$APPDIR/qubox.desktop"

# placeholder SVG icon
cat > "$APPDIR/usr/share/icons/hicolor/scalable/apps/qubox.svg" <<'ICON_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">
  <rect width="128" height="128" rx="16" fill="#2563eb"/>
  <path d="M40 40h48v48H40z" fill="none" stroke="#fff" stroke-width="6"/>
  <path d="M56 56l16 16M72 56l-16 16" stroke="#fff" stroke-width="5" stroke-linecap="round"/>
</svg>
ICON_EOF

# root-level icon symlink (AppImage lookup)
ln -sf "usr/share/icons/hicolor/scalable/apps/qubox.svg" \
    "$APPDIR/qubox.svg"

# ---- 3. AppRun entry point ----
cat > "$APPDIR/AppRun" <<'APPRUN_EOF'
#!/bin/bash
# AppRun — Launched when the AppImage is executed.
# If no arguments, runs the daemon in the foreground.
# With --help or other flags, forwards to qubox.
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/qubox" "$@"
APPRUN_EOF
chmod 0755 "$APPDIR/AppRun"

# ---- 4. download appimagetool if missing ----
APPIMAGETOOL="${APPIMAGETOOL:-appimagetool}"
if ! command -v "$APPIMAGETOOL" &>/dev/null; then
    CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/qubox"
    mkdir -p "$CACHE_DIR"
    APPIMAGETOOL="$CACHE_DIR/appimagetool"
    if [ ! -f "$APPIMAGETOOL" ]; then
        echo "[*] Downloading appimagetool..."
        APPIMAGETOOL_URL="https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-${ARCH}.AppImage"
        curl -fsSL "$APPIMAGETOOL_URL" -o "$APPIMAGETOOL"
        chmod 0755 "$APPIMAGETOOL"
    fi
fi

# ---- 5. build AppImage ----
OUTPUT="${APP_NAME}-${VERSION}-${ARCH}.AppImage"
echo "[*] Building $OUTPUT..."
ARCH="$ARCH" "$APPIMAGETOOL" "$APPDIR" "$OUTPUT"
echo "[+] Done: $OUTPUT"

# clean up
rm -rf "$(dirname "$APPDIR")"
