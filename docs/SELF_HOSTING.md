# Self-Hosting Qubox

This guide covers the **self-host** path: a single host running the signaling
server, coturn, and an optional Caddy TLS proxy. For internet-facing
deployments, plan for firewall rules, signed-hello enforcement, and a
managed accounts API in front of signaling (out of scope for this repo;
the wire contract is documented in `apps/qubox-signaling-server/src/enrollment.rs`).

> **Quickest path:** the bundled Docker stack. Continue below.

---

## 1. Hardware & OS

Any modern Linux x86_64 or arm64 host works. TURN benefits from decent
uplink bandwidth; a `t4g.nano` or small VPS is enough for a few concurrent
streams. We test on:

- Ubuntu 22.04 / 24.04 LTS
- Debian 12 (bookworm)
- Arch (rolling)
- macOS 13+ for client development
- Windows 10/11 for client / host-agent builds

## 2. Network ports

| Port | Protocol | Service | Public? |
|------|----------|---------|---------|
| 22   | TCP      | SSH     | restricted to your IP |
| 80   | TCP      | Caddy (HTTP→HTTPS) | yes (only if TLS enabled) |
| 443  | TCP      | Caddy (WSS)  | yes |
| 3478 | UDP/TCP  | coturn STUN/TURN | yes |
| 5349 | UDP/TCP  | coturn TLS (optional) | yes |
| 7000 | TCP      | Signaling (only if not fronted by Caddy) | optional |
| 49152–49251 | UDP | coturn relay | yes |

If you use the TLS profile in `ops/self-host/docker-compose.yml`, expose
**443 + 3478** and keep 7000 closed to the public.

## 3. Three-step start

```bash
git clone https://github.com/MegaPanchamZ/qubox.git
cd qubox
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

That brings up:

- **coturn** — STUN/TURN on UDP+TCP 3478
- **signaling** — WebSocket on 7000
- **caddy** (TLS profile) — 443 with auto-TLS

Verify:

```bash
docker compose -f ops/self-host/docker-compose.yml ps
curl -sS http://127.0.0.1:7000/health
turnutils_stunclient -p 3478 127.0.0.1
```

## 4. TLS (`wss://`)

```bash
export QUBOX_DOMAIN=rd.example.com
docker compose -f ops/self-host/docker-compose.yml --profile tls up -d --build
```

Caddy will request a Let's Encrypt cert for `$QUBOX_DOMAIN` automatically.
Make sure DNS A/AAAA records for the domain point at the host **before**
starting Caddy (port 80 must be reachable for the ACME challenge).

## 5. Build the clients

On the **host machine** (the one you want to control):

```bash
cargo build --release -p qubox-host-agent
QUBOX_IDENTITY_PATH=$HOME/.qubox/host-id.json \
  ./target/release/qubox-host-agent \
    --server ws://127.0.0.1:7000/ws \
    --name "$(hostname)"
```

On a **viewer machine**:

```bash
cargo build --release -p qubox-client-cli
QUBOX_IDENTITY_PATH=$HOME/.qubox/client-id.json \
  ./target/release/qubox-client-cli \
    --server ws://127.0.0.1:7000/ws \
    list-hosts
```

To get a Tauri GUI, see `apps/qubox-client-gui/`.

## 6. Pairing

When a viewer calls `pair --host <name>`, the host's running agent must
approve the request (in the GUI) or you must launch the host agent with
`--auto-approve-pairing` (LAN / single-user only).

Pairing grants are persisted to `/data/pairings.json` inside the signaling
container (mounted as a Docker volume). On the host filesystem this is
`/var/lib/docker/volumes/qubox_qubox-data/_data/pairings.json` by default.

## 7. Operations

| Task | How |
|------|-----|
| Logs | `docker compose -f ops/self-host/docker-compose.yml logs -f` |
| Restart | `docker compose -f ops/self-host/docker-compose.yml restart signaling` |
| Stop | `docker compose -f ops/self-host/docker-compose.yml down` (omit `-v` to keep data) |
| Rotate secrets | `openssl rand -hex 32` → set env, restart containers |
| Update | `git pull && docker compose -f ops/self-host/docker-compose.yml up -d --build` |
| Soak test | see `docs/operations/turn-soak.md` |
| Coturn tuning | see `docs/operations/coturn-deploy.md` |

## 8. Hardening for internet exposure

Open-source self-host runs in **Open enrollment** mode (any caller that
reaches `:7000/ws` can list hosts). That's fine for LANs; for public
exposure, take **all** of these steps:

1. **Front signaling with Caddy** (TLS profile) — never expose 7000 directly.
2. **Require signed `Hello`** — already on by default; verify with
   `RUST_LOG=info` on the signaling container (look for "SignedHello
   required").
3. **Pairing approval** — do **not** use `--auto-approve-pairing` on
   internet-facing hosts; require human approval on the host agent.
4. **TURN ACLs** — keep `realm` and `static-auth-secret` in
   `ops/coturn/turnserver.conf` private; rotate periodically.
5. **Rate limiting** — front signaling with Caddy's `rate_limit` directive
   or an upstream WAF (Cloudflare, etc.).
6. **Optional: managed enrollment** — set `QUBOX_REQUIRE_ENROLLMENT=1`
   and run an accounts API in front of signaling. The wire contract is
   documented; an open-source reference implementation is not in scope for
   this repo.
7. **TUF auto-update** — see `docs/tuf.md` for the trust model and
   the publish flow.

## 9. Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Viewers see "no hosts" | signaling not reachable | check `:7000/ws` and `--server` arg |
| Pairing hangs | host agent not running, or `QUBOX_REQUIRE_SIGNED_HELLO` mismatch | restart host agent; align env |
| Stream stutters | no TURN; NAT symmetric | check `turnutils_stunclient`; verify UDP 3478 reachable |
| TURN auth fails | shared secret mismatch | set `QUBOX_TURN_SECRET` consistently across signaling + coturn |
| Browser client can't connect | missing TLS | run with `--profile tls` |
| High CPU on host | H.265 / AV1 with weak CPU | start sessions with `--codec h264` |

## 10. Where to go next

- `docs/architecture.md` — how the pieces fit together
- `docs/security-hardening.md` — threat model
- `docs/production-readiness.md` — known gaps
- `docs/operations/coturn-deploy.md` — TURN tuning
- `docs/operations/turn-soak.md` — load tests
- `docs/tuf.md` — auto-update model
- `docs/platforms.md` — OS support matrix

For questions, open an issue or email **dev@qubox.app**.