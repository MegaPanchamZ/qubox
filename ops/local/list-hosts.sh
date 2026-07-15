#!/usr/bin/env bash
# List hosts visible to the client on the configured signaling server.
# Useful to verify a host-agent has registered.
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

BIN="$REPO_ROOT/target/release/qubox-client-cli"
SERVER="${QUBOX_SERVER:-ws://127.0.0.1:7000/ws}"
IDENTITY="${QUBOX_IDENTITY_PATH:-$REPO_ROOT/.local/qubox/identity-client.json}"

mkdir -p "$(dirname "$IDENTITY")"

exec "$BIN" \
    --server "$SERVER" \
    --identity-path "$IDENTITY" \
    list-hosts