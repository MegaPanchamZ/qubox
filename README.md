# Qubox

**Secure, self-hostable WebRTC-style remote desktop — pair machines, stream desktops, move files.**

Qubox lets you take control of another machine, share screens, and move files
between them — peer-to-peer when possible, TURN-relayed when not. The core is
written in Rust; the desktop client is a Tauri (TypeScript + Rust) app.

```
┌──────────────────┐  WebSocket (TLS)  ┌──────────────────┐
│  qubox-host-agent│ ─────────────────▶│ qubox-signaling- │
│  (controlled PC) │ ◀─────────────── │     server       │
└──────────────────┘                  └────────┬─────────┘
        ▲                                       │ TURN credentials
        │ QUIC / WebRTC                         ▼
        │                                ┌──────────────┐
┌──────────────────┐                    │   coturn     │
│ qubox-client-cli │  ────relay─────────▶│  (TURN/STUN) │
│ / -gui (viewer)  │                    └──────────────┘
└──────────────────┘
```

> **Managed cloud:** if you'd rather not self-host, the [Qubox Cloud](https://qubox.app)
> service runs the same open-source clients against hosted signaling, TURN, and
> accounts. The cloud is a separate, proprietary codebase that talks to the
> same wire protocols as the open source stack.

---

## Features

- **End-to-end** streaming: display, audio, keyboard, mouse, pen / tablet, gamepad, clipboard
- **Identity & pairing**: Ed25519 device identity, signed `Hello` handshake, host-side approval
- **Transports**: native QUIC, WebRTC (browser-compatible), QUIC relay fallback
- **File sync** between paired machines
- **TURN / STUN** for NAT traversal (uses standard `coturn`)
- **TUF** auto-update with offline root key
- **Cross-platform** host agent and viewer: Linux, Windows, macOS
- **Three clients**: Tauri GUI, headless CLI, browser (WebCodecs)

---

## Quick start (self-host)

The fastest path is the bundled Docker stack:

```bash
git clone https://github.com/MegaPanchamZ/qubox.git
cd qubox
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

This runs the signaling server (`:7000`), coturn (`:3478`), and an
optional Caddy reverse proxy (`:443`) on the same host.

Then on the **machine you want to control** (host):

```bash
cargo build --release -p qubox-host-agent
QUBOX_IDENTITY_PATH=/tmp/host-id.json \
  ./target/release/qubox-host-agent \
    --server ws://127.0.0.1:7000/ws \
    --name "$(hostname)"
```

On the **viewer** (client), in a Tauri GUI or CLI:

```bash
cargo build --release -p qubox-client-cli
QUBOX_IDENTITY_PATH=/tmp/client-id.json \
  ./target/release/qubox-client-cli \
    --server ws://127.0.0.1:7000/ws \
    list-hosts
# pair with the host
./target/release/qubox-client-cli \
    --server ws://127.0.0.1:7000/ws \
    pair --host "$(hostname)"
./target/release/qubox-client-cli \
    --server ws://127.0.0.1:7000/ws \
    start-session --host "$(hostname)" --codec h264
```

A native window opens streaming the host's display. See
[`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md) for the full guide
(network exposure, TLS, TURN, troubleshooting).

> **Default security posture:** the open-source signaling server runs in
> *Open* enrollment mode — anyone who can reach `:7000/ws` can list hosts.
> This is the right default for trusted LANs. For internet exposure, set
> `QUBOX_REQUIRE_ENROLLMENT=1` and run a managed accounts API (out of
> scope for this repo, but the wire format is documented in
> `apps/qubox-signaling-server/src/enrollment.rs`).

---

## Building from source

Requires Rust 1.78+ and a C toolchain (for the codec stack on Linux:
`gcc`, `libx11-dev`, `libxkbcommon-dev`, `libwayland-dev`, `libudev-dev`,
`libpipewire-0.3-dev`, `libopus-dev`).

```bash
cargo build --release \
  -p qubox-signaling-server \
  -p qubox-host-agent \
  -p qubox-client-cli \
  -p qubox-daemon
```

The Tauri GUI requires Node 18+ and `pnpm`:

```bash
cd apps/qubox-client-gui
pnpm install
pnpm tauri build
```

Cross-compile helpers live under `apps/qubox-*/Dockerfile` and
`scratch/`.

---

## Repo layout

```
qubox/
├── apps/                       # binaries
│   ├── qubox-signaling-server/    # WebSocket signaling + TURN credential issuer
│   ├── qubox-host-agent/          # runs on the controlled machine
│   ├── qubox-client-cli/          # headless viewer
│   ├── qubox-client-gui/          # Tauri desktop viewer
│   └── qubox-daemon/              # background daemon (state, IPC, TUF)
├── crates/                     # reusable libraries
│   ├── qubox-signaling/        # protocol + tenant isolation
│   ├── qubox-transport/        # QUIC / TCP
│   ├── qubox-webtransport/     # WebTransport binding
│   ├── qubox-identity/         # device identity + signed hellos
│   ├── qubox-proto/            # shared message types
│   ├── qubox-media/, -display/, -mic/, -pen/, -clipboard/
│   ├── qubox-platform/         # OS shims
│   ├── qubox-rl-policy/        # adaptive bitrate (optional)
│   └── qubox-sync/             # file sync engine
├── clients/webcodecs/          # browser-based viewer
├── docs/                       # architecture, security, operations
├── ops/
│   ├── self-host/              # one-liner Docker stack
│   ├── local/                  # dev scripts (start signaling, connect)
│   ├── signing/                # release signing helpers
│   └── vm-lab/                 # local VM test lab
├── scripts/
├── research/                   # ADRs and roadmap
└── scratch/                    # throwaway / VM build helpers
```

---

## Documentation

| Doc | What it covers |
|-----|----------------|
| [`docs/SELF_HOSTING.md`](docs/SELF_HOSTING.md) | End-to-end self-host guide |
| [`docs/architecture.md`](docs/architecture.md) | High-level architecture |
| [`docs/security-hardening.md`](docs/security-hardening.md) | Threat model + hardening |
| [`docs/production-readiness.md`](docs/production-readiness.md) | What's solid, what isn't |
| [`docs/operations/coturn-deploy.md`](docs/operations/coturn-deploy.md) | TURN server deploy |
| [`docs/operations/turn-soak.md`](docs/operations/turn-soak.md) | Load + soak testing |
| [`docs/tuf.md`](docs/tuf.md) | Auto-update trust model |
| [`docs/platforms.md`](docs/platforms.md) | OS / platform support matrix |

---

## License

This project is licensed under the **GNU Affero General Public License v3.0
or later** (AGPL-3.0-or-later). See [`LICENSE`](LICENSE).

The AGPL means: if you run a modified version of this code as a network
service, you must publish your modifications under the same license. This
keeps the open core genuinely open. If you want to ship a non-AGPL build
or a hosted fork, please contact us about a commercial license.

> **Qubox Cloud** (<https://qubox.app>) is a hosted service built on top of
> this codebase. The service's closed components (account API, billing,
> TUF channel) are in a separate, private repository and are *not* AGPL.

---

## Contributing

Contributions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for
the workflow, coding conventions, and how to run the test suite.

Please report security issues privately to **security@qubox.app** — see
[`SECURITY.md`](SECURITY.md).

---

## Acknowledgments

Qubox is built on the shoulders of giants: the Rust async ecosystem, the
WebRTC community, and the TUF maintainers. See [`NOTICE`](NOTICE).