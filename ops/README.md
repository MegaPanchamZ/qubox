# ops — Operations (self-host)

Day-to-day operational scripts and configs for the **open-core** stack
(signaling + desktop clients + TURN + TUF helpers).

Hosted accounts, friends, billing, and managed cloud deployment live in the
companion private product (**Qubox Cloud**), not in this repository.

## Layout

```
ops/
├── README.md          ← this file
├── self-host/         ← docker-compose single-stack self-host
├── coturn/            ← coturn Dockerfile + turnserver.conf (used by self-host)
├── local/             ← local dev helpers (start signaling, list hosts, sync)
└── signing/           ← cosign release-signing helpers
```

## Self-host (recommended)

```bash
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

See [`ops/self-host/README.md`](self-host/README.md). With the TLS profile,
clients use `wss://<your-domain>/ws` on port 443.

## Local development

`ops/local/start-signaling.sh` runs a development signaling server against
the workspace. `ops/local/list-hosts.sh` prints LAN-discoverable peer URLs.

## Release signing

`ops/signing/` wraps cosign + OS-native toolchains for release artifacts.

## Coturn

`ops/coturn/` is the image/config built by `ops/self-host/docker-compose.yml`.
For a standalone TURN runbook see [`docs/operations/coturn-deploy.md`](../docs/operations/coturn-deploy.md).
