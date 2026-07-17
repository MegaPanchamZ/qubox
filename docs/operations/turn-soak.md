# TURN soak matrix (production gate)

Validate relay under NAT before declaring self-host (or any hosted edge) production ready.

## Topology matrix

| Client NAT | Host NAT | Expected path | Pass criteria |
|------------|----------|---------------|---------------|
| Full cone | Full cone | Direct UDP preferred | Session ≤3s to first frame |
| Symmetric | Full cone | TURN allocate + relay | Session ≤8s; RTT < 2× direct |
| Symmetric | Symmetric | Dual TURN | Session ≤12s; no freeze >2s |
| CGNAT mobile | Home NAT | TURN | 10 min soak; loss <3% |
| Firewall UDP-block | Any | TCP TURN (if enabled) | Or hard-fail with clear UI |

## Soak procedure

```bash
# 1. Bring up coturn + signaling (self-host example)
docker compose -f ops/self-host/docker-compose.yml up -d

# 2. Host behind NAT A
export QUBOX_SERVER=wss://signaling.example
cargo run -p qubox-host-agent -- --server "$QUBOX_SERVER" --stream-mode multi-display

# 3. Client behind NAT B
cargo run -p qubox-client-cli -- start-session --host <host-peer-id> \
  --transport native-quic --codec h264 --max-stream-frames 0

# 4. Record for 30 minutes
# - first-frame latency
# - reconnect after 30s network drop
# - FileSync push of a 50 MiB file mid-session
# - kick/share while streaming
```

## Pass/fail log template

```
date:
topology: client=<nat> host=<nat>
first_frame_ms:
median_rtt_ms:
loss_percent:
reconnect_ok: yes|no
filesync_ok: yes|no
notes:
```

## CI

Lab VMs: `ops/vm-lab/`. Full symmetric NAT is not free-CI portable; run this
matrix on staging before each release tag.

## Related

- `ops/coturn/turnserver.conf`
- `apps/qubox-signaling-server/src/turn.rs`
- `crates/qubox-transport/src/turn.rs`
