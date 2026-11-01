Low-latency remote desktop & 4-player co-op gaming powered by Iroh P2P and QUIC v2.

## Highlights

- **Iroh P2P + QUIC v2 Connectivity:** Direct peer-to-peer NAT hole-punching by Ed25519 `NodeId` with automatic fallback to `iroh-relay` (`:3340`) or TURN (`:3478`).
- **Simplified 2-Tab Desktop UI:** Streamlined **Computers** and **Settings** navigation with compact single-line host rows, pinned controls, and instant live search filtering across 100+ computers.
- **4-Player Co-Op Gaming & Party Share Codes:** Up to 4 friends can connect simultaneously with dedicated virtual Xbox 360 controllers (`Player 1`–`Player 4`) via 15-minute multi-friend **Party Share Codes**.
- **Parsec-Style Per-Peer Access Control:** Persistent per-friend toggles for **Controller**, **Keyboard**, **Mouse**, and **Connect without approval** (auto-approve), plus instant **Kick** / **Kick All**.

## Install Desktop App (Recommended)

Download the installer for your operating system from the release assets below:

| Platform | Desktop Installer | Standalone CLI Archive |
|----------|-------------------|------------------------|
| **Windows (x64)** | `Qubox_*_x64-setup.exe` (NSIS) or `.msi` | `qubox-cli-windows-x86_64.zip` |
| **macOS (Apple Silicon)** | `Qubox_*_aarch64.dmg` | `qubox-cli-macos-aarch64.tar.gz` |
| **Linux (x86_64)** | `.AppImage`, `.deb`, or `.rpm` | `qubox-cli-linux-x86_64.tar.gz` |

1. Install **Qubox** on the computer you want to share (host) and the computer(s) connecting to it.
2. Open **Qubox** → click **Share This Computer** on the host, or click **Share Code** to generate a 15-minute Party Share Code for friends.
3. On the client: click **Connect** next to the paired computer or paste the Party Share Code into the **Enter share code...** bar.

## Power Users / Self-Host CLI

Each `qubox-cli-*` archive includes the standalone binaries:

| Binary | Role |
|--------|------|
| `qubox-host-agent` | Host capture & streaming agent |
| `qubox-client-cli` | Native low-latency viewer & CLI |
| `qubox-daemon` | Local background service & FileSync daemon |
| `qubox-signaling-server` | Self-host WebSocket signaling + embedded `iroh-relay` server |

See [`docs/SELF_HOSTING.md`](https://github.com/MegaPanchamZ/qubox/blob/main/docs/SELF_HOSTING.md) for self-hosting instructions.

## What's Changed
