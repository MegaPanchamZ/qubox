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

## Features

- First-run wizard (device name + signaling URL)
- System tray: show, start/stop host, quit
- Hosts / Pairing / Sessions / Host mode / File Sync / Settings
- Share code create/redeem; File Sync never-track (default `.git`)

See `docs/maturity-non-mobile.md` and `docs/user-guide-file-sync.md`.
