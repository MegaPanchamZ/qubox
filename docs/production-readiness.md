# Production readiness (self-host desktop)

**Scope:** this public repository only (self-host signaling, clients, TURN, daemon).  
Mobile/web remain out of scope. Hosted multi-tenant Cloud is a separate product.

## What ships (open core)

### Trust & control plane

- Device Ed25519 identity + encrypted envelope (`qubox-identity`)
- `SignedHello` required by default on signaling
- HMAC session credentials; TURN bound to session
- Self-host stack: `ops/self-host/docker-compose.yml` (signaling + coturn + optional Caddy)
- Open enrollment default (LAN-trust); harden with TLS + pairing approval + network ACLs
- Daemon IPC: pair, share, kick, settings, onboarding, FileSync

### Media & session

- Native QUIC H.264 path (host capture → client decoder/viewer)
- Congestion / rate feedback hooks; multi-display host mode
- Clipboard + mic permission enforcement on host
- Privacy: blank-overlay production path; vkms flag (Linux)
- HW decode with SW fallback on the client CLI path

### File Sync (ADR-022)

- StreamPurpose `FileSync`, blake3, redb schema
- Daemon outbox/jobs/ignores (default never-track includes `.git`)
- Path confinement, size caps, concurrent accept limit

### Desktop UX

- Tauri GUI + CLI viewer; first-run should target **your** signaling URL
- Host agent approval for pairing on public deployments

## Ops gates before calling a deploy “production”

1. `docs/operations/turn-soak.md` on the intended network path  
2. TLS profile (`wss://`) for any internet-facing signaling  
3. No `--auto-approve-pairing` on public hosts  
4. Secrets rotated (`QUBOX_SIGNALING_SECRET`, `QUBOX_TURN_SECRET`)  
5. Installer signing / TUF when distributing binaries outside a lab  

## Explicitly out of scope here

- Accounts API, OIDC, billing, friends/access grants  
- Managed enrollment HTTP client and multi-tenant SaaS edge  
- EC2 / AWS product deploy scripts  

See `docs/adr/021-dual-mode-control-plane.md` and <https://qubox.app> for the Cloud product boundary.

## Residual risk

See `docs/security-hardening.md`.
