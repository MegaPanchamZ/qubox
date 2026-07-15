# Production Readiness (desktop non-mobile)

**Status: production-ready for self-host and managed desktop paths**, subject to
the residual items in [security-hardening.md](./security-hardening.md) and the
ops gates below (TURN soak, org code-signing certs).

Mobile/web remain out of scope.

## What ships

### Trust & control plane

- Device Ed25519 identity + encrypted envelope (`qubox-identity`)
- `SignedHello` required by default on signaling
- HMAC session credentials; TURN bound to session
- Managed defaults: no auto-approve; `wss://` via `ops/managed`
- Self-host: `ops/self-host/docker-compose.yml` (signaling + coturn)
- Accounts scaffold: OIDC JWKS, Postgres, admin HTTP (feature-gated)
- Daemon IPC: pair, share, kick, settings, onboarding, FileSync

### Media & session

- Native QUIC H.264 path (host capture → client decoder/viewer)
- Congestion / rate feedback hooks; multi-display host mode
- Clipboard + mic permission enforcement on host
- Privacy: blank-overlay production path; vkms flag (Linux)
- HW decode: probe + `get_format` trampoline with SW fallback
  (`apps/qubox-client-cli/src/decoder_hw.rs`)

### File Sync (ADR-022)

- StreamPurpose `FileSync = 0x06`, blake3, redb schema v2
- Daemon outbox/jobs/ignores (default never-track includes `.git`)
- Live QUIC push/accept drain on host + client session
- Path confinement, size caps, concurrent accept limit

### Desktop UX

- Tauri GUI: first-run, tray, host mode, share, File Sync, settings
- Privacy + multi-display prefs applied on host start
- Session multi-display tile grid in GUI
- CLI: `qubox-daemon`, `qubox-host-agent`, `qubox-client-cli`

## Production gates (ops)

| Gate | Doc / tool |
|------|------------|
| Security mitigations | [security-hardening.md](./security-hardening.md) |
| TURN NAT soak | [operations/turn-soak.md](./operations/turn-soak.md) |
| TLS cert rotation | [operations/signaling-tls-cert-rotation.md](./operations/signaling-tls-cert-rotation.md) |
| Code signing | [ops/signing/README.md](../ops/signing/README.md) |
| TUF updates | [ops/tuf/README.md](../ops/tuf/README.md) |
| Maturity checklist | [maturity-non-mobile.md](./maturity-non-mobile.md) |
| Product gaps | [product-gaps.md](./product-gaps.md) |

## How to run (desktop)

```bash
cargo run -p qubox-daemon -- run
# optional self-host signaling
docker compose -f ops/self-host/docker-compose.yml up -d
cd apps/qubox-client-gui && npm i && npm run tauri dev
```

## Verification

```bash
cargo test -p qubox-transport --lib filesync
cargo test -p qubox-sync
cargo test -p qubox-daemon --lib
cargo check -p qubox-host-agent -p qubox-client-cli -p qubox-client-gui
```

## Explicitly deferred (not blockers for desktop 1.0)

1. Org Authenticode / notarization certs (scripts ready; keys not in-repo)
2. Full vkms / IddCx kernel virtual display productization
3. Embedded video surface inside Tauri (CLI dual-window is supported)
4. HDR / 4K144 polish and USB passthrough
5. Mobile clients
