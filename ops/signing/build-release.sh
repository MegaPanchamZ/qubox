#!/usr/bin/env bash
# Build release binaries + optional dry-run sign. No private keys required.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

echo "== cargo release =="
cargo build --release -p qubox-daemon -p qubox-host-agent -p qubox-client-cli

OUT="$ROOT/target/release"
mkdir -p "$ROOT/dist/release"
for b in qubox-daemon qubox-host-agent qubox-client-cli; do
  if [[ -f "$OUT/$b" ]]; then
    cp -f "$OUT/$b" "$ROOT/dist/release/"
    echo "copied $b"
  fi
done

echo "== checksums + dry-run sign =="
export QUBOX_SIGN_DRY_RUN=1
bash "$ROOT/ops/signing/sign-linux.sh" "$ROOT/dist/release"

echo "== summary =="
ls -la "$ROOT/dist/release"
cat <<'EOF'

Next (with real keys):
  Windows:  pwsh ops/signing/sign-windows.ps1
  macOS:    ops/signing/sign-macos.sh
  Linux:    QUBOX_GPG_KEY=... ops/signing/sign-linux.sh dist/release

Public beta can ship dry-run checksums; EV/Developer ID certs remain org-owned.
EOF
