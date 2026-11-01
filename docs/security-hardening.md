# Security Hardening (Desktop + Self-Host Production)

Threat model: paired peers communicating over authenticated WebSocket signaling and end-to-end encrypted Iroh P2P / QUIC v2 media and control streams.
Attackers considered: network MITM, untrusted or over-privileged guest peer in a multi-player session, local unprivileged processes, and path-injection via FileSync.

## Mitigations in Code

| Surface | Mitigation |
|---------|------------|
| Signaling identity | Ed25519 device identity; `SignedHello` enforced by default |
| Transport encryption | End-to-end TLS 1.3 (`rustls` + `ring`) over QUIC v2 and Iroh P2P endpoints authenticated by peer `NodeId` |
| Session auth | HMAC-bound `SessionCredential` with short TTL enforced on session start |
| Global auto-approve | Refused on non-loopback binds (`refuse_auto_approve_on_public_server`) |
| Per-peer access control | Parsec-style per-peer host permissions (`allow_gamepad`, `allow_keyboard`, `allow_mouse`, `auto_approve`) enforced in real time by `qubox-host-agent`; host can kick any or all peers immediately |
| Party Share Codes | 15-minute expiry (`900s` TTL); guests default to controller-only when joining co-op sessions unless granted keyboard/mouse by the host |
| TLS | Caddy profile in `ops/self-host` (`wss://`); never expose raw `:7000` on the public internet |
| Relay privacy | `iroh-relay` (`:3340`) and `coturn` (`:3478`) forward only end-to-end encrypted QUIC/DTLS packets |
| FileSync path confinement | `validate_relative_path` + `resolve_safe_target` rejects `..`, absolute paths, symlinks, null bytes, and control characters |
| FileSync integrity & limits | `blake3` verification before atomic rename from `.qubox-partial`; `MAX_FILESYNC_BYTES` (512 MiB); `MAX_FILESYNC_CONCURRENT` (4); `.git` never-track default |
| Host permissions | Host enforces per-session clipboard and microphone permission bits |
| Daemon IPC | Local Unix domain socket / Windows Named Pipe restricted to the local user |

## Operational Controls (Self-Host)

1. **Do not** run `qubox-host-agent` with global `--auto-approve-pairing` on internet-facing networks; use the GUI's per-device **Connect without approval** toggle only for your own trusted machines.
2. Run `ops/self-host` with the **TLS profile** (`--profile tls`) when exposing signaling outside a private LAN.
3. Restrict FileSync destination (`QUBOX_FILESYNC_DIR`) to a dedicated workspace folder rather than `$HOME`.
4. Revoke pairings or kick guests immediately from the **Computers** tab if a peer device is lost or untrusted.

## Incident Response

Report vulnerabilities privately to **security@qubox.app** per [`SECURITY.md`](../SECURITY.md). Coordinated disclosure default is 90 days.
