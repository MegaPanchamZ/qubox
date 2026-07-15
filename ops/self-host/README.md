# Self-host Qubox (single compose)

## 3-step start

```bash
cd /path/to/better-parsec
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

Signaling: `ws://127.0.0.1:7000/ws`  
TURN: `turn:127.0.0.1:3478` (shared secret via env)

## Clients

```bash
export QUBOX_SERVER=ws://127.0.0.1:7000/ws

# Controlled machine
target/release/qubox-host-agent --server "$QUBOX_SERVER" --name "$(hostname)"

# Viewer
ops/local/connect.sh "$(hostname)"
```

**Defaults:** signed `Hello` required (no `--allow-unsigned-hello`).  
Do **not** use `--auto-approve-pairing` on internet-facing hosts.

## TLS (`wss://`)

```bash
export QUBOX_DOMAIN=rd.example.com
docker compose -f ops/self-host/docker-compose.yml --profile tls up -d --build
# clients: QUBOX_SERVER=wss://rd.example.com/ws
```

## Legacy LAN only

```bash
SIGNALING_EXTRA_ARGS='--allow-unsigned-hello' \
  docker compose -f ops/self-host/docker-compose.yml up -d
```

## Data

Pair grants: Docker volume `qubox-data` → `/data/pairings.json` (atomic write).

## Stop

```bash
docker compose -f ops/self-host/docker-compose.yml down
# keep data: omit -v
```
