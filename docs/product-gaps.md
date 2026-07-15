# Product gaps checklist (P0–P3)

Track: dual-mode (self-host + managed). See ADR-021. Non-mobile maturity: `docs/maturity-non-mobile.md`.

## P0 Trust (public / managed blockers)

| Item | Status |
|------|--------|
| Device Ed25519 identity + encrypted envelope | Done (`qubox-identity` v3) |
| `SignedHello` on host/client | Done |
| Server default reject unsigned Hello | Done |
| HMAC-bound `SessionCredential` | Done |
| TURN bound to session credential | Done |
| Auto-approve forbidden in managed defaults | Done |
| TLS (`wss://`) default for managed | Done (`ops/managed`) |

## P1 Self-host product

| Item | Status |
|------|--------|
| Dockerfile + self-host compose | Done |
| Atomic pair store | Done |
| One-line install host-agent | Done |
| GHCR publish | Done |

## P2 Managed control plane

| Item | Status |
|------|--------|
| Accounts + OIDC JWKS + Postgres + admin HTTP | Done (features) |
| Regional TURN docs | Done |
| TURN soak runbook | Done |

## P3 UX

| Item | Status |
|------|--------|
| Human device names | Done |
| Tray host + GUI default path | Done |
| QR / share link | Done (code + copy; QR widget optional) |
| Session permissions + kick UI | Done |
| Settings persistence | Done |
| First-run wizard | Done |
| File Sync GUI + never-track `.git` | Done |
| FileSync live QUIC drain + path security | Done |
| Privacy prefs → host start | Done |
| Multi-display mode pref + session tiles | Done |

## Definition of done (per mode)

**Self-host:** compose up → pair → stream.  
**Managed:** managed compose + `wss://` → approve pair → stream.

### Ops residual (org assets)

- Authenticode / notarization certs for public installers (`ops/signing` + CI dry-run)
- Multi-site TURN soak logs (loopback CI in `.github/workflows/turn-soak.yml`)
- GPU-native capture backends (soft capture sessions shipped)
