#!/usr/bin/env bash
# Smoke-test sign-linux.sh dry-run (no GPG key).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP"
echo stub >"$TMP/qubox-daemon"
echo stub >"$TMP/qubox-host-agent"
echo stub >"$TMP/qubox-client-cli"
export QUBOX_SIGN_DRY_RUN=1
"$ROOT/ops/signing/sign-linux.sh" "$TMP"
test -f "$TMP/qubox-daemon.sha256"
test -f "$TMP/qubox-host-agent.sha256"
test -f "$TMP/qubox-client-cli.sha256"
echo "sign-linux dry-run OK"
