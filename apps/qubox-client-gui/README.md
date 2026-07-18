# Qubox desktop client (Tauri 2 + React)

Production **launcher / control plane** for Qubox. Video still runs in
`qubox-client-cli` (separate window); this app owns pairing, sessions,
host start/stop, File Sync, settings, and tray.

## Prerequisites

- `qubox-daemon run` (settings + File Sync + host lifecycle)
- Optional: `qubox-signaling-server` for self-host
- Built `qubox-client-cli` on `PATH` or next to the GUI binary

## Dev

```bash
npm install
npm run tauri dev
```

## Testing

### Unit (Vitest + mockIPC)

```bash
npm test
```

Uses `@tauri-apps/api/mocks` (`mockIPC`) so frontend command calls run without a Rust backend.

### E2E — browser mode (local, recommended)

Drives the Vite frontend in Chrome via `@wdio/tauri-service` browser mode.
No Tauri binary or WebKitWebDriver required; `invoke()` is mocked.

```bash
# from apps/qubox-client-gui
npm run test:e2e
```

Or with Vite already running on port 1420:

```bash
QUBOX_E2E_SKIP_VITE=1 npm run test:e2e
```

Specs live in `e2e/specs/`. Config: `e2e/wdio.browser.conf.ts`.

### E2E — native mode (full Tauri binary)

```bash
# build the GUI (workspace root)
cargo build -p qubox-client-gui --release

# Linux: WebKitWebDriver + fake display
#   sudo apt-get install -y webkit2gtk-driver xvfb
xvfb-run npm run test:e2e:native
```

Optional: set `QUBOX_E2E_APP=/path/to/qubox-client-gui`.  
For `browser.tauri.mock()` / `execute()` in native mode, add `tauri-plugin-wdio` (and `tauri-plugin-wdio-webdriver` for embedded driver) — see Tauri WDIO docs.

## Features

- First-run wizard (device name + signaling URL)
- System tray: show, start/stop host, quit
- Hosts / Pairing / Sessions / Host mode / File Sync / Settings
- Share code create/redeem; File Sync never-track (default `.git`)

See `docs/maturity-non-mobile.md` and `docs/user-guide-file-sync.md`.
