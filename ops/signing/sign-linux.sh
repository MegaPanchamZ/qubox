#!/usr/bin/env bash
# GPG + optional cosign for Linux release artifacts.
# QUBOX_SIGN_DRY_RUN=1 → write .sha256 sidecars only (no GPG key required).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$ROOT/target/release}"
GPG_KEY="${QUBOX_GPG_KEY:-}"
DRY="${QUBOX_SIGN_DRY_RUN:-0}"

shopt -s nullglob
files=("$OUT"/qubox-daemon "$OUT"/qubox-host-agent "$OUT"/qubox-client-cli)
found=0
for f in "${files[@]}"; do
  [[ -f "$f" ]] || continue
  found=1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" > "${f}.sha256"
    echo "checksum ${f}.sha256"
  fi
  if [[ "$DRY" == "1" || "$DRY" == "true" ]]; then
    echo "dry-run: skip gpg for $f"
    continue
  fi
  if [[ -z "$GPG_KEY" ]]; then
    echo "error: set QUBOX_GPG_KEY or QUBOX_SIGN_DRY_RUN=1" >&2
    exit 2
  fi
  gpg --local-user "$GPG_KEY" --detach-sign --armor "$f"
  echo "signed $f"
  if command -v cosign >/dev/null 2>&1; then
    cosign sign-blob --yes "$f" --output-signature "${f}.cosign.sig" || true
  fi
done
if [[ "$found" -eq 0 ]]; then
  echo "error: no release binaries under $OUT" >&2
  exit 1
fi
