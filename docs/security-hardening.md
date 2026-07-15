# Security hardening (desktop production)

Threat model: paired peers over authenticated signaling + native QUIC.
Attackers: network MITM, malicious peer after pair compromise, local
unprivileged process, path-injection via FileSync.

## Mitigations in code

| Surface | Mitigation |
|---------|------------|
| Signaling identity | Ed25519 device identity; `SignedHello` required by default |
| Session auth | HMAC-bound `SessionCredential`; short TTL |
| Managed auto-approve | Forbidden on public servers (`refuse_auto_approve_on_public_server`) |
| TLS | Managed path defaults to `wss://` (Caddy); see `ops/managed` |
| TURN | Credentials bound to session; short-term credentials |
| FileSync path | `validate_relative_path` + `resolve_safe_target` (no `..`, abs, null, control) |
| FileSync integrity | blake3 over body; reject on mismatch; atomic rename from `.qubox-partial` |
| FileSync size | `MAX_FILESYNC_BYTES` (512 MiB); streaming hash/I/O |
| FileSync concurrency | `MAX_FILESYNC_CONCURRENT` (4) accept slots |
| FileSync never-track | Default ignores include `.git` (ADR-022) |
| Host permissions | Host enforces input/clipboard/mic permission bits |
| Daemon IPC | Local Unix socket / Named Pipe; no remote bind |
| Updates | TUF repo under `ops/tuf` (when published) |
| Privacy | Blank-overlay / vkms flags; GUI prefs applied on host start |

## Operational controls

1. **Do not** run host-agent with `--auto-approve-pairing` on internet-facing servers.
2. Prefer managed compose with TLS termination; rotate certs per `docs/operations/signaling-tls-cert-rotation.md`.
3. Restrict FileSync destination (`QUBOX_FILESYNC_DIR`) to a dedicated directory; do not point at `$HOME`.
4. Keep pairing approvals human-gated; revoke via CLI/GUI when devices are lost.
5. Run TURN soak before production cutover: `docs/operations/turn-soak.md`.
6. Sign installers when shipping outside lab: `ops/signing/README.md` + `ops/tuf`.

## Residual risk (accepted for 1.0 desktop)

| Risk | Status |
|------|--------|
| Code-signing cert / SmartScreen reputation | Requires org certs (scripts ready) |
| Full HW decode on every GPU | SW fallback always available; HW probe + get_format path present |
| vkms/IddCx productization | Blank-overlay production path works without kernel modules |
| Embedded video in Tauri | Dual-window CLI viewer is the supported product surface |
| USB passthrough | Out of scope unless product requires |

## Incident response

Report to **security@qubox.app** per `SECURITY.md`. Coordinated disclosure default 90 days.
