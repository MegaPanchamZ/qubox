# Qubox Desktop App (`qubox-client-gui` — Tauri 2 + React)

Primary desktop interface for **Qubox**. Provides a streamlined 2-tab layout (**Computers** and **Settings**) for hosting, connecting to remote computers, managing 4-player Co-Op Gaming sessions, generating Party Share Codes, and configuring Parsec-style per-peer access controls. Low-latency video rendering runs in the bundled `qubox-client-cli` viewer window alongside `qubox-host-agent` and `qubox-daemon` sidecars.

## Features

- **Simplified 2-Tab Navigation:**
  - **Computers:** Pinned top controls (**Share This Computer**, 15-minute **Party Share Code** generator, live search filter, and **Connect via Code** input) above an independently scrollable, compact single-line computer list (`P1`–`P4` · **Computer Name** · `Connected` + **Connect** / **Sync** / **Unpair** actions) that scales cleanly to 100+ computers.
  - **Settings:** Device identity, signaling URL (`ws://` or `wss://`), video codec/FPS/bitrate, privacy blanking, File Sync ignore patterns, and daemon diagnostics.
- **4-Player Co-Op Gaming (`Player 1`–`Player 4`):** Up to 4 simultaneous guests can connect to a host, each assigned a dedicated virtual Xbox 360 controller (`uinput` on Linux, `ViGEmBus` on Windows).
- **Parsec-Style Per-Peer Access Control:** Persistent per-friend permission toggles directly on the active host banner:
  - **Controller** (`sports_esports`) — allow or block gamepad input
  - **Keyboard** (`keyboard`) — allow or block keyboard events
  - **Mouse** (`mouse`) — allow or block mouse cursor/clicks
  - **Connect without approval** (`lock_open`) — auto-approve future pairing/session requests from trusted peers
  - **Kick / Kick All** (`person_remove` / `group_off`) — immediately terminate one or all active guest sessions
- **System Tray Integration:** Show window, toggle hosting, and quit cleanly.

## Development

```bash
npm install
npm run tauri dev
```

## Testing

### Unit Tests (Vitest + `mockIPC`)

```bash
npm test
```

Uses `@tauri-apps/api/mocks` (`mockIPC`) so frontend command calls run without a live Rust backend.

### E2E — Browser Mode (Local, Recommended)

Drives the Vite frontend in headless Chrome via WebdriverIO:

```bash
npm run test:e2e
```

Or with Vite already running on port 1420:

```bash
QUBOX_E2E_SKIP_VITE=1 npm run test:e2e
```

### E2E — Native Mode (Full Tauri Binary)

```bash
cargo build -p qubox-client-gui --release
xvfb-run npm run test:e2e:native
```

See [`../../docs/architecture.md`](../../docs/architecture.md) and [`../../docs/SELF_HOSTING.md`](../../docs/SELF_HOSTING.md) for full system documentation.
