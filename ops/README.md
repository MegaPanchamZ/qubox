# ops — Operations

Day-to-day operational scripts and configs for the Qubox core
(signaling + desktop client + TUF metadata). Cloud-only things
(accounts server, billing, managed deployment) live in the
companion `qubox-cloud` repository.

## Layout

```
ops/
├── README.md          ← this file
├── self-host/         ← docker-compose single-stack self-host
├── local/             ← local dev helpers (start signaling, list hosts, sync)
├── signing/           ← cosign release-signing helpers
└── coturn/            ← coturn Dockerfile + turnserver.conf (deprecated for self-host; see ops/self-host)
```

## Self-host (recommended)

The single-stack bring-up of signaling + coturn + caddy is:

```bash
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

See [`ops/self-host/README.md`](self-host/README.md). The peer you connect
to will be `wss://<your-host>` and the default port is 443.

## Local development

`ops/local/start-signaling.sh` runs a development signaling server against
the workspace. `ops/local/list-hosts.sh` prints LAN-discoverable peer URLs.

## Release signing

`ops/signing/{sign-linux,sign-macos,sign-windows}.sh` wrap cosign + the
respective OS native toolchains. `build-release.sh` is called from CI.

## Coturn (legacy / advanced)

`ops/coturn/` contains a standalone coturn config that can be used
outside the Docker Compose stack. Most users should use
`ops/self-host/docker-compose.yml`, which wires coturn + signaling
together.

For an operations deployment runbook see [`docs/operations/coturn-deploy.md`](../docs/operations/coturn-deploy.md).
