# Test coverage gaps inventory

Last audit: 2026-07-15 (post full gap close-out).

## Closed this wave

- Permission matrix, FileSync congestion + rate feedback hook  
- Soft capture backends (DXGI/SCK/PipeWire), soft_capture unit tests  
- Watcher evaluate unit tests  
- StartHost IPC test  
- GUI vitest (hostPrefs, fileSyncLogic, qr)  
- Xephyr e2e CI + TURN soak CI + dev-sign CI  

## Residual (lab / hardware)

| Path | Notes |
|------|--------|
| Real GPU capture e2e | Soft capture stands in on CI |
| Multi-machine NAT | Manual soak log |
| Paid code signing | Scripts + dry-run only |

## Commands

```bash
cargo test -p qubox-sync -p qubox-transport -p qubox-display --lib
cargo test -p qubox-daemon --lib
cargo test -p qubox-host-agent permissions
cd apps/qubox-client-gui && npm test
DISPLAY=:99 QUBOX_REQUIRE_E2E=1 cargo test -p qubox-host-agent --test privacy_e2e
```
