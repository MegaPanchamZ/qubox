#!/usr/bin/env bash
# Pair the qubox client with a host and start a remote-desktop session.
#
# Usage:
#   ops/local/connect.sh my-host                  # pair + start-session
#   ops/local/connect.sh my-host --codec h265     # custom codec
#   ops/local/connect.sh my-host --transport relay-quic
#
# Set QUBOX_SERVER to override the signaling server URL.
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

HOST="${1:?usage: $0 <host-name> [extra args...]}"
shift || true

BIN="$REPO_ROOT/target/release/qubox-client-cli"
if [[ ! -x "$BIN" ]]; then
    echo "error: $BIN not built. Run: cargo build --release -p qubox-client-cli" >&2
    exit 1
fi

SERVER="${QUBOX_SERVER:-ws://127.0.0.1:7000/ws}"
IDENTITY="${QUBOX_IDENTITY_PATH:-$REPO_ROOT/.local/qubox/identity-client.json}"

mkdir -p "$(dirname "$IDENTITY")"

CLI_ARGS=(
    --server "$SERVER"
    --identity-path "$IDENTITY"
    --name "$(hostname)"
)

echo "[client] server=$SERVER"
echo "[client] identity=$IDENTITY"
echo "[client] host=$HOST"

# Pair first (idempotent — server returns 'already paired' on second run)
echo ""
echo "[client] === pairing with $HOST ==="
"$BIN" "${CLI_ARGS[@]}" pair --host "$HOST" || true

# Then start the session
echo ""
echo "[client] === starting session ==="
exec "$BIN" "${CLI_ARGS[@]}" start-session --host "$HOST" "$@"