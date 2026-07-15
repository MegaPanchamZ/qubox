#!/usr/bin/env bash
# Start the qubox signaling server on the Linux host.
# Listens on 0.0.0.0:7000 by default. Override with QUBOX_BIND.
#
# Usage:
#   ops/local/start-signaling.sh                  # bind 0.0.0.0:7000
#   QUBOX_BIND=127.0.0.1:7000 ops/local/start-signaling.sh
#
# Production-ish setup is in ops/signaling-server/run-signaling-server.sh
# (uses /opt/qubox/bin); this script is for local-dev on the workstation.
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

BIN="$REPO_ROOT/target/release/qubox-signaling-server"
if [[ ! -x "$BIN" ]]; then
    echo "error: $BIN not built. Run: cargo build --release -p qubox-signaling-server" >&2
    exit 1
fi

BIND="${QUBOX_BIND:-0.0.0.0:7000}"
PAIRING_STORE="${QUBOX_PAIRING_STORE:-$REPO_ROOT/.local/qubox/pairing.sqlite}"

mkdir -p "$(dirname "$PAIRING_STORE")"

# Stable secret so session credentials survive restarts (optional).
# If unset, the server generates a per-process secret and warns loudly.
export QUBOX_SIGNALING_SECRET="${QUBOX_SIGNALING_SECRET:-dev-local-please-change-me}"

ARGS=(
    --bind "$BIND"
    --pairing-store "$PAIRING_STORE"
    --allow-unsigned-hello
)

echo "[signaling] bin=$BIN"
echo "[signaling] bind=$BIND"
echo "[signaling] pairing-store=$PAIRING_STORE"
echo "[signaling] secret=${QUBOX_SIGNALING_SECRET:0:8}...(dev)"
exec "$BIN" "${ARGS[@]}"