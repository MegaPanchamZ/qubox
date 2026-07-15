# Non-mobile maturity checklist (desktop product)

Target: Parsec-class **desktop** product (Linux / Windows / macOS). Mobile/web out of scope.

## Production status

**Desktop control + session + FileSync path: Done** for self-host and managed
skeletons. Ops gates: TURN soak on staging, org code-signing for public installers.

| Area | Status |
|------|--------|
| Daemon settings Get/Set/List + onboarding | Done |
| GUI first-run wizard | Done |
| Settings persist via daemon | Done |
| System tray (show, host start/stop, quit) | Done |
| Autostart plugin (optional OS login) | Done |
| Host mode panel + share panel | Done |
| File Sync GUI + default `.git` ignores | Done |
| Outbox drain-ready event on session start | Done |
| **FileSync live QUIC push/accept drain** | Done |
| FileSync path/size/concurrency security | Done |
| Privacy mode host flags + GUI → host start | Done |
| Multi-display host mode + GUI tile grid | Done |
| Session permissions / kick / share IPC | Done |
| Dual-mode self-host + managed compose | Done |
| SignedHello / session HMAC / TURN bind | Done |
| HW decode probe + get_format + SW fallback | Done |
| Signing / TUF scripts | Done (certs external) |
| TURN soak runbook | Done |
| Security hardening doc | Done |

### FileSync live path

While a native QUIC session is up:

1. Host/client poll daemon `SyncDrainReady` every 3s.
2. Pending jobs → `push_file_over_connection` (FileSync uni + blake3).
3. Peer `run_filesync_accept_loop` writes under `QUBOX_FILESYNC_DIR` or
   `…/qubox/incoming` (path-confined).
4. Job status updated via `SyncUpdateJob` (InFlight / Done / Failed).
5. Locked/conflict files are skipped.

## Residual (ops / org / GPU — not desktop control-plane blockers)

1. **Paid Authenticode / notarization certs** — scripts + CI GPG dry-run in place  
2. **Real multi-site NAT soak** — coturn loopback CI + runbook; lab for symmetric NAT  
3. **GPU-native capture** — DXGI/SCK/PipeWire soft sessions work; Output Duplication / SCStream / DMA-BUF is hardware follow-up  
4. **Linked libav HW device** — get_format selection done; create needs vendor drivers  

See `docs/gap-inventory-closed.md`.

## How to run the desktop path

```bash
# Terminal 1
cargo run -p qubox-daemon -- run

# Terminal 2 (signaling if self-host)
cargo run -p qubox-signaling-server

# Terminal 3
cd apps/qubox-client-gui && npm i && npm run tauri dev
```

Tray: left-click show; menu Start/Stop host. First launch: onboarding → Hosts → Connect.
