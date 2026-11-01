# Self-Hosting Qubox

This guide covers the **self-host** path: running `qubox-signaling-server` (with embedded `iroh-relay` and optional `coturn` + Caddy TLS proxy) on your own server or LAN. This repository is **self-host / open-core** — no cloud account is required.

> **Qubox Cloud** (hosted accounts, cloud friend graphs, managed global edge) is a separate commercial service at <https://qubox.app>. It uses the exact same open-source client and host binaries.

---

## 1. Hardware & OS

Any modern Linux `x86_64` or `arm64` host works. A small VPS (1 vCPU, 1 GB RAM) easily handles signaling and relay traffic for several concurrent streams. We test on:

- Ubuntu 22.04 / 24.04 LTS
- Debian 12 (bookworm)
- Arch Linux
- macOS 13+ (client & host)
- Windows 10/11 (client & host)

## 2. Network Ports

| Port | Protocol | Service | Public? |
|------|----------|---------|---------|
| 22 | TCP | SSH | restricted to your IP |
| 80 | TCP | Caddy (HTTP → HTTPS ACME) | yes (only if TLS enabled) |
| 443 | TCP | Caddy (`wss://` signaling) | yes |
| 3340 | TCP/UDP | Embedded `iroh-relay` (optional P2P relay fallback) | optional / recommended |
| 3478 | UDP/TCP | coturn STUN/TURN | yes |
| 5349 | UDP/TCP | coturn TLS (optional) | optional |
| 7000 | TCP | Signaling WS (only if not fronted by Caddy) | LAN only |
| 49152–49251 | UDP | coturn media relay range | yes |

If you use the TLS profile in `ops/self-host/docker-compose.yml`, expose **443 + 3340 + 3478** and keep **7000** closed to the public internet.

## 3. Three-Step Start (Docker Compose)

```bash
git clone https://github.com/MegaPanchamZ/qubox.git
cd qubox
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

Verify:

```bash
docker compose -f ops/self-host/docker-compose.yml ps
curl -sS http://127.0.0.1:7000/health
```

### Standalone Binary with Embedded Iroh Relay

You can also run `qubox-signaling-server` directly as a single binary with the built-in `iroh-relay` server enabled:

```bash
cargo build --release -p qubox-signaling-server
./target/release/qubox-signaling-server \
  --listen 0.0.0.0:7000 \
  --iroh-relay-listen 0.0.0.0:3340 \
  --iroh-relay-public-url http://<YOUR_SERVER_IP>:3340
```

## 4. TLS (`wss://`)

```bash
export QUBOX_DOMAIN=rd.example.com
docker compose -f ops/self-host/docker-compose.yml --profile tls up -d --build
```

Caddy automatically provisions a Let's Encrypt certificate for `$QUBOX_DOMAIN`. Ensure DNS `A`/`AAAA` records point at your server before starting Caddy.

## 5. Connecting Desktop Clients & Hosts

Download the **Qubox** desktop installer for Windows, macOS, or Linux from [GitHub Releases](https://github.com/MegaPanchamZ/qubox/releases), or build from source:

```bash
# Host machine
cargo build --release -p qubox-host-agent
QUBOX_IDENTITY_PATH=$HOME/.qubox/host-id.json \
  ./target/release/qubox-host-agent \
    --server ws://127.0.0.1:7000/ws \
    --name "$(hostname)"

# Client machine
cargo build --release -p qubox-client-cli
QUBOX_IDENTITY_PATH=$HOME/.qubox/client-id.json \
  ./target/release/qubox-client-cli \
    --server ws://127.0.0.1:7000/ws \
    list-hosts
```

In the **Qubox** desktop GUI (`apps/qubox-client-gui`), open **Settings** and set your signaling URL (`ws://<LAN_IP>:7000/ws` or `wss://your.domain/ws`).

## 6. Pairing, Party Share Codes & Access Control

- **Standard Pairing:** When a viewer requests to pair with a host, an incoming request banner appears on the host's **Computers** tab with **Approve** and **Reject** actions.
- **Party Share Codes (4-Player Co-Op):** Click **Share Code** on the host's **This Computer** card to generate a 15-minute multi-friend code. Up to 4 friends can paste the code into the **Enter share code...** bar on their **Computers** tab to join immediately as `Player 1`–`Player 4`.
- **Parsec-Style Per-Peer Permissions:** While guests are connected, the host can toggle **Controller**, **Keyboard**, **Mouse**, and **Connect without approval** (auto-approve future connections) per guest, or kick individual/all guests at any time.

## 7. Operations

| Task | Command |
|------|---------|
| View logs | `docker compose -f ops/self-host/docker-compose.yml logs -f` |
| Restart signaling | `docker compose -f ops/self-host/docker-compose.yml restart signaling` |
| Stop stack | `docker compose -f ops/self-host/docker-compose.yml down` |
| Update stack | `git pull && docker compose -f ops/self-host/docker-compose.yml up -d --build` |
| TURN soak test | See [`docs/operations/turn-soak.md`](operations/turn-soak.md) |
| Coturn tuning | See [`docs/operations/coturn-deploy.md`](operations/coturn-deploy.md) |

## 8. Hardening for Internet Exposure

1. **Front signaling with Caddy** (`--profile tls`) — never expose raw port `:7000` publicly.
2. **Keep `SignedHello` enabled** (on by default).
3. **Require human pairing approval** — do not pass `--auto-approve-pairing` on internet-facing hosts; use the GUI's per-friend **Connect without approval** toggle only for trusted devices.
4. **Rotate secrets** (`QUBOX_SIGNALING_SECRET` and `QUBOX_TURN_SECRET`) periodically.

## 9. Where to Go Next

- [`docs/architecture.md`](architecture.md) — Control, Iroh P2P media, input, and platform planes
- [`docs/platforms.md`](platforms.md) — OS support matrix (Linux, Windows, macOS)
- [`docs/security-hardening.md`](security-hardening.md) — Threat model and mitigations
- [`docs/production-readiness.md`](production-readiness.md) — Production deployment checklist
- [`docs/tuf.md`](tuf.md) — TUF auto-update model
- [`docs/adr/021-dual-mode-control-plane.md`](adr/021-dual-mode-control-plane.md) — OSS vs Cloud boundary
