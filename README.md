# Qubox

**Low-latency remote desktop & 4-player co-op streaming in Rust — QUIC v2 + Iroh P2P media plane, hardware encode/decode, and a self-hostable control plane.**

[![CI](https://github.com/MegaPanchamZ/qubox/actions/workflows/ci.yml/badge.svg)](https://github.com/MegaPanchamZ/qubox/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

> **Two ways to run Qubox**
>
> - **Self-host (this repo):** Run `qubox-signaling-server` (with embedded `iroh-relay` and optional `coturn`) on your own hardware or VPS. Free, AGPL-3.0, no account required. Start with [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md).
> - **Qubox Cloud:** Managed signaling, global relays, accounts, and cloud friend graphs at **[qubox.app](https://qubox.app)**. Same open-source client & host binaries — only the control plane is hosted.

---

## Architecture

```
┌────────────────────────────┐                  ┌────────────────────────────┐
│  qubox-host-agent          │   Iroh P2P /     │  qubox-client-cli / GUI    │
│  • PipeWire / DXGI / SCK   │◄──QUIC v2───────►│  • Hardware H.264/HEVC/AV1 │
│  • NVENC / VAAPI / VT / MF │   (UDP hole-     │  • wgpu + winit renderer   │
│  • Enigo + uinput/ViGEm    │    punching or   │  • Mouse / KB / 4x Gamepad │
│  • Player 1–4 Gamepad slots│    Iroh Relay)   │  • Parsec-style permissions│
│  • Opus audio + FileSync   │                  │  • Opus playback + Mic     │
└─────────────┬──────────────┘                  └──────────────┬─────────────┘
              │  Ed25519 SignedHello + Party Share Codes       │
              └──────────────►┌────────────────┐◄──────────────┘
                              │   signaling    │
                              │   (WS :7000)   │
                              │ + iroh-relay   │
                              │   (:3340)      │
                              └────────────────┘
```

### Key Features

- **Direct Iroh P2P & QUIC v2 Transport (`qubox-transport`):** Automatic NAT hole-punching via Ed25519 NodeIds with zero-config fallback to an embedded `iroh-relay` server (`:3340`) or TURN (`:3478`).
- **Simplified Desktop App (`qubox-client-gui`):** Clean 2-tab interface (**Computers** and **Settings**) with compact single-line host rows, live search filtering across 100+ computers, and one-click connect.
- **4-Player Co-Op Gaming & Party Share Codes:** Host up to 4 simultaneous players with isolated virtual Xbox 360 controllers (`Player 1`–`Player 4` via Linux `uinput` and Windows `ViGEmBus`). Generate 15-minute multi-friend **Party Share Codes** so friends can join with a single code.
- **Parsec-Style Per-Peer Access Control:** Granular, persistent per-friend permissions directly on the host banner — toggle **Controller**, **Keyboard**, **Mouse**, and **Connect without approval** (auto-approve), or kick individual/all guests at any time.
- **Context-Aware File Sync (`qubox-sync`):** Content-addressed (`blake3`) file synchronization over dedicated QUIC streams with path confinement and automatic `.git` ignore rules.

### Crates (`crates/`)

| Crate | Role |
|-------|------|
| `qubox-proto` | Wire protocol (`SignedHello`, pairing, Party Share Codes, `EndpointAddr`, codec negotiation, `StreamPurpose::FileSync`) |
| `qubox-identity` | Ed25519 device identity, encrypted on-disk keystore, signed signaling envelopes |
| `qubox-signaling` | Signaling state machine, pairing store, 15-minute multi-peer share codes, HMAC session credentials |
| `qubox-transport` | QUIC v2 + Iroh P2P endpoint transport, BBRv2/ack-frequency tuning, Reed-Solomon FEC, frame-aware pacer |
| `qubox-webtransport` | WebTransport server for the browser client (`web/`) |
| `qubox-media` | Capture & encode/decode (PipeWire, X11, DXGI, ScreenCaptureKit; FFmpeg/VAAPI/NVENC/VideoToolbox/MediaFoundation; wgpu viewer) |
| `qubox-platform` | OS input injection (Enigo, XTest, Linux `uinput` & Windows `ViGEmBus` virtual Xbox 360 gamepads for up to 4 players) |
| `qubox-display` | Display enumeration, privacy blanking overlay, virtual display helpers (`vkms` / `IddCx`) |
| `qubox-clipboard` | Bidirectional clipboard sync with host permission gate |
| `qubox-mic` | Client-to-host microphone passthrough (Opus + noise suppression) |
| `qubox-pen` | Stylus / tablet pressure & tilt events |
| `qubox-rl-policy` | Pensieve-style ABR bitrate policy |
| `qubox-sync` | Content-addressed (`blake3`) File Sync over QUIC streams with `.git` never-track default |

### Apps (`apps/`)

| Binary | Role |
|--------|------|
| `qubox-signaling-server` | Self-host WebSocket signaling server (`:7000/ws`, `/health`) + optional embedded `iroh-relay` (`:3340`) |
| `qubox-host-agent` | Captures display/audio, runs Iroh + QUIC host listener, enforces per-peer input permissions |
| `qubox-client-cli` | Native low-latency viewer (`list-hosts`, `pair`, `connect`, `share-create`, `share-redeem`) |
| `qubox-daemon` | Local background service (IPC socket/pipe, host lifecycle, FileSync outbox, settings) |
| `qubox-client-gui` | Tauri 2 + React desktop app (Computers & Settings tabs, Party Share Codes, Parsec-style access control, tray) |

---

## Quickstart — Self-Host in 60 Seconds

See **[`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md)** for full instructions, firewall ports, TLS (`wss://`), and hardening.

```bash
git clone https://github.com/MegaPanchamZ/qubox.git
cd qubox

export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)

docker compose -f ops/self-host/docker-compose.yml up -d --build
curl -sS http://127.0.0.1:7000/health
```

Then build and run a host and viewer on your LAN:

```bash
cargo build --release -p qubox-host-agent -p qubox-client-cli

# On the host machine (the one being controlled)
./target/release/qubox-host-agent \
  --server ws://<SIGNALING_IP>:7000/ws \
  --name my-workstation

# On the client machine
./target/release/qubox-client-cli \
  --server ws://<SIGNALING_IP>:7000/ws \
  list-hosts
```

Or download the prebuilt **Qubox Desktop** installer for Windows (`.exe` / `.msi`), macOS (`.dmg`), or Linux (`.AppImage` / `.deb` / `.rpm`) from [**GitHub Releases**](https://github.com/MegaPanchamZ/qubox/releases).

---

## Building from Source

### Prerequisites

- **Rust** 1.83+ (`rustup update stable`)
- **Node.js** 22+ (only required for `apps/qubox-client-gui` and `web/`)
- **Linux system packages** (Ubuntu/Debian):
  ```bash
  sudo apt-get update && sudo apt-get install -y \
    libx11-dev libxrandr-dev libxdo-dev libxkbcommon-dev \
    libwayland-dev libxext-dev libclang-dev pkg-config cmake \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
    libpulse-dev libasound2-dev libdbus-1-dev libudev-dev libsystemd-dev \
    libpipewire-0.3-dev libspa-0.2-dev libopus-dev \
    libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
  ```
- **macOS:** `brew install ffmpeg opus pkg-config cmake`
- **Windows:** Visual Studio 2022 C++ Build Tools, LLVM/Clang, FFmpeg, and `vcpkg install opus:x64-windows-static-md`

### Workspace check & test

```bash
cargo test --workspace --exclude qubox-client-gui
```

---

## Documentation

- [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md) — Docker Compose stack, TLS with Caddy, pairing, Party Share Codes, operations
- [`docs/architecture.md`](docs/architecture.md) — Control, media, Iroh P2P, input, and platform planes
- [`docs/platforms.md`](docs/platforms.md) — Linux, Windows, and macOS capture, encode, and virtual gamepad support
- [`docs/security-hardening.md`](docs/security-hardening.md) — Threat model, Ed25519 identity, per-peer access controls, FileSync confinement
- [`docs/production-readiness.md`](docs/production-readiness.md) — Production checklist for self-hosted deployments
- [`docs/operations/coturn-deploy.md`](docs/operations/coturn-deploy.md) & [`docs/operations/turn-soak.md`](docs/operations/turn-soak.md) — Relay deployment and load testing
- [`docs/tuf.md`](docs/tuf.md) — TUF auto-update signing and verification
- [`docs/adr/`](docs/adr/) — Architecture Decision Records (ADR-001 through ADR-022)

---

## Security

Please report vulnerabilities privately to **security@qubox.app** — see [`SECURITY.md`](SECURITY.md).

## License

Qubox is licensed under the **[GNU Affero General Public License v3.0 or later](LICENSE)** (`AGPL-3.0-or-later`).
For commercial licensing or the hosted multi-tenant platform, visit **[qubox.app](https://qubox.app)**.