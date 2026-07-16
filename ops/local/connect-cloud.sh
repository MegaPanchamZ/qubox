#!/usr/bin/env bash
# Connect using the public managed signaling endpoint (signal.qubox.app).
#
# Usage:
#   ops/local/connect-cloud.sh my-host
#   ops/local/connect-cloud.sh my-host --codec h265
#
# Override with QUBOX_SERVER if needed:
#   QUBOX_SERVER=ws://127.0.0.1:7000/ws ops/local/connect-cloud.sh my-host
set -euo pipefail

export QUBOX_SERVER="${QUBOX_SERVER:-wss://signal.qubox.app/ws}"
exec "$(dirname "$0")/connect.sh" "$@"
