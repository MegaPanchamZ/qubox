# Production Readiness (Self-Host & Desktop)

**Scope:** Open-source repository (`qubox-signaling-server`, `qubox-host-agent`, `qubox-client-cli`, `qubox-client-gui`, `qubox-daemon`).
Hosted multi-tenant Cloud is maintained separately at <https://qubox.app>.

## What Ships in Open Core

### Trust & Control Plane

- Ed25519 device identity + encrypted keystore (`qubox-identity`)
- `SignedHello` required by default on signaling
- Short-lived HMAC `SessionCredential` tokens bound to sessions
- 15-minute multi-friend **Party Share Codes** for instant co-op sessions
- **Parsec-Style Per-Peer Access Control**: persistent per-peer toggles for **Controller**, **Keyboard**, **Mouse**, and **Connect without approval** (`auto_approve`), plus instant **Kick** / **Kick All**
- Self-host stack: `ops/self-host/docker-compose.yml` (`signaling` + embedded `iroh-relay` + `coturn` + optional `Caddy` TLS)

### Media & Input Plane

- Direct **Iroh P2P + QUIC v2** transport (`qubox-transport`) with automatic NAT hole-punching and relay fallback (`iroh-relay` / TURN)
- Hardware H.264 / HEVC / AV1 encoding and decoding with software fallback
- **4-Player Co-Op Gaming**: up to 4 concurrent guests mapped to dedicated virtual Xbox 360 controllers (`Player 1`–`Player 4` via Linux `/dev/uinput` and Windows `ViGEmBus`)
- Bidirectional clipboard sync and client-to-host Opus microphone passthrough
- Display privacy blanking overlay and multi-display selection

### Context-Aware File Sync (ADR-022)

- Dedicated `StreamPurpose::FileSync` QUIC streams, `blake3` content addressing, `redb` state store
- Automatic `.git` never-track default, path traversal confinement, 512 MiB file size cap, 4 concurrent transfer slots

### Desktop UX (`qubox-client-gui`)

- Streamlined 2-tab Tauri 2 desktop app (**Computers** and **Settings**)
- Pinned host/share controls and independently scrollable compact computer rows tested cleanly up to 100+ hosts with sub-10ms live search filtering
- Prebuilt installers via GitHub Actions for **Windows** (`.exe`, `.msi`), **macOS** (`.dmg`), and **Linux** (`.AppImage`, `.deb`, `.rpm`)

## Ops Gates Before Production Cutover

1. Run [`docs/operations/turn-soak.md`](operations/turn-soak.md) on the target network path.
2. Enable TLS (`wss://`) for any internet-facing signaling server.
3. Never use global `--auto-approve-pairing` on internet-facing hosts; rely on explicit host approval or per-device **Connect without approval** for trusted peers.
4. Rotate `QUBOX_SIGNALING_SECRET` and `QUBOX_TURN_SECRET`.
5. Sign installers / TUF metadata when distributing binaries outside your organization.
