# Binary signing & release integrity

Production shipping needs org-owned certs. This directory holds **runbooks and
scripts**; it does not embed private keys.

## Prerequisites

| Platform | Asset |
|----------|--------|
| Windows | Authenticode OV/EV cert + `signtool` |
| macOS | Developer ID Application + notarytool credentials |
| Linux | GPG key for packages + optional cosign keyless OIDC |

## Scripts

- `sign-windows.ps1` — Authenticode sign release binaries (fails if cert missing)
- `sign-macos.sh` — codesign + notarize
- `sign-linux.sh` — GPG detach-sign + cosign if available
- `build-release.sh` — build + dry-run checksum convenience wrapper

## Release checklist

1. `cargo build --release -p qubox-daemon -p qubox-host-agent -p qubox-client-cli`
2. Build installers (`apps/qubox-daemon/dist/*`)
3. Run platform sign script (set `QUBOX_SIGN_DRY_RUN=1` to skip the
   actual signature step while still producing checksums)
4. Publish release artifacts
5. Verify install on clean machine (SmartScreen / Gatekeeper / apt/rpm)

See research: `research/roadmap/p2-19-signed-binaries.md`.